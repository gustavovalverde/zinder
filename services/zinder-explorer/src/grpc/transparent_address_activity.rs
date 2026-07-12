//! `ExplorerQuery.TransparentAddressActivity` handler.
//!
//! Reads the confirmed-activity feed materialized by
//! [`zinder_derive::TransparentAddressActivityConsumer`]
//! out of the consumer-owned `transparent_address_activity` column family.
//! The storage layout sorts newest-first per address, so the handler
//! serves pages in that order; clients that want oldest-first reverse
//! client-side.

use std::collections::{HashMap, HashSet};

use prost::Message as _;
use tonic::{Request, Response, Status};
use zebra_chain::transparent::Address as ZebraTransparentAddress;
use zinder_core::wire::{
    decode_address_script_hash, decode_height_key_descending, decode_rpc_transaction_id_hex,
    encode_address_script_hash,
};
use zinder_core::{
    ChainEpochId, Network, TransactionFactsArtifact, TransactionId, TransparentAddressScriptHash,
};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2;
use zinder_proto::v1::explorer::{
    TransparentAddressActivityEntry, TransparentAddressActivityRecord,
    TransparentAddressActivityRequest, TransparentAddressActivityResponse,
    TransparentAddressRankingCoverage, TransparentAddressSummary as WireTransparentAddressSummary,
};
use zinder_proto::v1::wallet::{
    self, AddressLookup, LatestBlockRequest, address_lookup::Selector as AddressSelector,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;
use zinder_store::{
    SecondaryChainStore, chain_epoch_from_message, chain_epoch_message, status_from_store_error,
};

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use super::transaction_detail::encode_component_counts;
use super::transparent_input::parent_transaction_ids;
use super::{clamp_max_entries, require_matching_chain_epoch};
use zinder_derive::{
    DeriveStore, TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY, TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN,
    TransparentAddressRankingConsumer,
    TransparentAddressSummary as DerivedTransparentAddressSummary,
};

/// Hard cap on the activity rows one page returns.
const MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST: u32 = 256;

/// Default entries when the caller passes `max_entries = 0`.
const DEFAULT_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES: u32 = 64;

/// Hard cap on offset work for page-number compatibility adapters.
const MAX_TRANSPARENT_ADDRESS_ACTIVITY_OFFSET: u64 = 100_000;

const ADDRESS_HASH_LEN: usize = 32;
const HEIGHT_KEY_OFFSET: usize = ADDRESS_HASH_LEN;
const HEIGHT_KEY_END: usize = HEIGHT_KEY_OFFSET + 4;

/// Executes one `ExplorerQuery.TransparentAddressActivity` request.
pub(crate) struct TransparentAddressActivityContext<'store> {
    pub(crate) derive_store: &'store DeriveStore,
    pub(crate) canonical_store: Option<&'store SecondaryChainStore>,
    pub(crate) network: Network,
    pub(crate) upstream_observation_cache: &'store UpstreamObservationCache,
}

pub(crate) async fn handle_transparent_address_activity(
    context: TransparentAddressActivityContext<'_>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: Request<TransparentAddressActivityRequest>,
) -> Result<Response<TransparentAddressActivityResponse>, Status> {
    let TransparentAddressActivityContext {
        derive_store,
        canonical_store,
        network,
        upstream_observation_cache,
    } = context;
    let inner = request.into_inner();
    validate_activity_pagination(inner.offset, &inner.from_cursor)?;
    let address = inner
        .address
        .ok_or_else(|| ExplorerError::invalid_request("address selector is required"))?;
    let resolved_address = resolve_address_lookup(&address, network)?;
    let active_metadata = TransparentAddressRankingConsumer::active_metadata(derive_store)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(
                "transparent-address ranking has no active materialized generation",
            )
        })?;
    let max_entries = clamp_max_entries(
        inner.max_entries,
        DEFAULT_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES,
        MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST,
    );
    let (mut entries, next_cursor) = scan_address_activity(
        derive_store,
        &ActivityScanParameters {
            script_hash: resolved_address.script_hash,
            offset: inner.offset,
            start_height: inner.start_height,
            end_height: inner.end_height,
            max_entries,
            from_cursor: inner.from_cursor.as_slice(),
        },
    )?;
    let chain_epoch =
        resolve_activity_chain_epoch(canonical_store, wallet_client, inner.at_epoch_id).await?;
    validate_ranking_metadata_at_chain_epoch(active_metadata, &chain_epoch)?;
    let summary =
        read_address_summary_at_metadata(derive_store, active_metadata, &resolved_address)?;
    let coverage = encode_ranking_coverage(active_metadata.coverage);
    enrich_activity_entries(
        canonical_store,
        &chain_epoch,
        resolved_address.script_hash,
        &mut entries,
    )?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(TransparentAddressActivityResponse {
        freshness: Some(freshness),
        entries,
        next_cursor,
        summary: Some(summary),
        coverage: Some(coverage),
    }))
}

async fn resolve_activity_chain_epoch(
    canonical_store: Option<&SecondaryChainStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    at_epoch_id: Option<u64>,
) -> Result<wallet::ChainEpoch, Status> {
    if let Some(canonical_store) = canonical_store {
        canonical_store
            .try_catch_up()
            .map_err(|error| status_from_store_error(&error))?;
        let reader = match at_epoch_id {
            Some(chain_epoch_id) => canonical_store
                .chain_epoch_reader_at(ChainEpochId::new(chain_epoch_id))
                .map_err(|error| status_from_store_error(&error))?,
            None => canonical_store
                .current_chain_epoch_reader()
                .map_err(|error| status_from_store_error(&error))?,
        };
        return Ok(chain_epoch_message(reader.chain_epoch()));
    }

    wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id }))
        .await?
        .into_inner()
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("LatestBlockResponse.chain_view.chain_epoch missing").into()
        })
}

fn read_address_summary_at_metadata(
    derive_store: &DeriveStore,
    expected_metadata: zinder_derive::TransparentAddressRankingMetadata,
    resolved_address: &ResolvedAddressLookup,
) -> Result<WireTransparentAddressSummary, Status> {
    let derived_summary =
        TransparentAddressRankingConsumer::summary(derive_store, resolved_address.script_hash)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let confirmed_metadata = TransparentAddressRankingConsumer::active_metadata(derive_store)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    if confirmed_metadata != Some(expected_metadata) {
        return Err(ExplorerError::unsatisfied_precondition(
            "active transparent-address ranking changed while reading address summary",
        )
        .into());
    }
    Ok(encode_address_summary(
        derived_summary,
        resolved_address.script_pub_key.as_deref(),
    ))
}

fn validate_ranking_metadata_at_chain_epoch(
    metadata: zinder_derive::TransparentAddressRankingMetadata,
    chain_epoch: &wallet::ChainEpoch,
) -> Result<(), Status> {
    let visible_tip = chain_epoch.visible_tip.as_ref().ok_or_else(|| {
        ExplorerError::internal("LatestBlockResponse chain epoch visible_tip missing")
    })?;
    let coverage = metadata.coverage;
    let balance_height = coverage.balance_complete_through_height.value();
    let history_height = coverage
        .history_complete_through_height
        .map_or(0, zinder_core::BlockHeight::value);
    if balance_height > visible_tip.height || history_height > visible_tip.height {
        return Err(ExplorerError::unsatisfied_precondition(format!(
            "active transparent-address ranking generation {} is newer than requested chain epoch {}: ranking balance/history through {balance_height}/{history_height}, visible tip {}",
            metadata.generation, chain_epoch.chain_epoch_id, visible_tip.height,
        ))
        .into());
    }
    Ok(())
}

fn validate_activity_pagination(offset: u64, from_cursor: &[u8]) -> Result<(), Status> {
    if offset != 0 && !from_cursor.is_empty() {
        return Err(ExplorerError::invalid_request(
            "from_cursor and nonzero offset are mutually exclusive",
        )
        .into());
    }
    if offset > MAX_TRANSPARENT_ADDRESS_ACTIVITY_OFFSET {
        return Err(ExplorerError::invalid_request(format!(
            "offset exceeds maximum {MAX_TRANSPARENT_ADDRESS_ACTIVITY_OFFSET}",
        ))
        .into());
    }
    Ok(())
}

/// Bundled inputs to [`scan_address_activity`].
struct ActivityScanParameters<'a> {
    script_hash: TransparentAddressScriptHash,
    offset: u64,
    start_height: u32,
    end_height: u32,
    max_entries: u32,
    from_cursor: &'a [u8],
}

fn scan_address_activity(
    derive_store: &DeriveStore,
    parameters: &ActivityScanParameters<'_>,
) -> Result<(Vec<TransparentAddressActivityEntry>, Vec<u8>), Status> {
    let prefix = encode_address_script_hash(parameters.script_hash);
    let ScanKeys {
        mut start_key,
        end_key,
        mut resume_cursor,
    } = build_scan_keys(prefix, parameters.from_cursor)?;
    let scan_cap = usize::try_from(MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST)
        .unwrap_or(usize::MAX)
        .saturating_add(1);
    let mut remaining_offset = parameters.offset;
    let mut entries =
        Vec::with_capacity(usize::try_from(parameters.max_entries).unwrap_or(usize::MAX));
    let mut last_key: Option<RowKey> = None;
    'scan: loop {
        let rows = derive_store
            .range_iterate_consumer(
                TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY,
                &start_key,
                &end_key,
                scan_cap,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        let row_count = rows.len();
        let mut last_scanned_key = None;
        for (key, payload) in rows {
            let key_array: RowKey = key
                .as_slice()
                .try_into()
                .map_err(|_| ExplorerError::internal("activity row key not 40 bytes"))?;
            if resume_cursor.is_some_and(|cursor| key_array == cursor) {
                continue;
            }
            let key_address = decode_address_script_hash(&key_array[0..ADDRESS_HASH_LEN])
                .map_err(|error| ExplorerError::internal(error.to_string()))?;
            if key_address != parameters.script_hash {
                break 'scan;
            }
            last_scanned_key = Some(key_array);
            let height = decode_row_height(&key_array)?;
            if height < parameters.start_height || height > parameters.end_height {
                continue;
            }
            if remaining_offset != 0 {
                remaining_offset -= 1;
                continue;
            }
            let record = TransparentAddressActivityRecord::decode(payload.as_slice())
                .map_err(|error| ExplorerError::internal(error.to_string()))?;
            entries.push(activity_entry_from_record(record, height));
            last_key = Some(key_array);
            if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= parameters.max_entries {
                break 'scan;
            }
        }
        if row_count < scan_cap {
            break;
        }
        let Some(last_scanned_key) = last_scanned_key else {
            break;
        };
        start_key = last_scanned_key;
        resume_cursor = Some(last_scanned_key);
    }

    let next_cursor = if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= parameters.max_entries
    {
        last_key.map_or_else(Vec::new, |key| key.to_vec())
    } else {
        Vec::new()
    };
    Ok((entries, next_cursor))
}

fn activity_entry_from_record(
    record: TransparentAddressActivityRecord,
    block_height: u32,
) -> TransparentAddressActivityEntry {
    TransparentAddressActivityEntry {
        transaction_id: record.transaction_id,
        block_height,
        block_time_unix_seconds: record.block_time_unix_seconds,
        net_value_zat: record.net_value_zat,
        input_count: record.input_count,
        output_count: record.output_count,
        prevout_resolution_status: record.prevout_resolution_status,
        transaction_index: None,
        size_bytes: None,
        component_counts: None,
        input_value_zat: None,
        output_value_zat: None,
        other_input_script_pub_keys: Vec::new(),
        other_output_script_pub_keys: Vec::new(),
        input_facts_complete: false,
    }
}

fn enrich_activity_entries(
    canonical_store: Option<&SecondaryChainStore>,
    chain_epoch: &wallet::ChainEpoch,
    requested_script_hash: TransparentAddressScriptHash,
    entries: &mut [TransparentAddressActivityEntry],
) -> Result<(), Status> {
    let Some(canonical_store) = canonical_store else {
        return Ok(());
    };
    canonical_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = canonical_store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    require_matching_chain_epoch(core_epoch, reader.chain_epoch())?;

    let transaction_ids = entries
        .iter()
        .map(|entry| {
            decode_rpc_transaction_id_hex(&entry.transaction_id)
                .map_err(|error| ExplorerError::internal(error.to_string()).into())
        })
        .collect::<Result<Vec<_>, Status>>()?;
    let transactions = reader
        .transaction_facts_by_ids(&transaction_ids)
        .map_err(|error| status_from_store_error(&error))?;
    let parent_ids = parent_transaction_ids(transactions.values().flatten());
    let parent_transactions = reader
        .transaction_facts_by_ids(&parent_ids)
        .map_err(|error| status_from_store_error(&error))?;

    for (entry, transaction_id) in entries.iter_mut().zip(transaction_ids) {
        let Some(transaction) = transactions.get(&transaction_id).and_then(Option::as_ref) else {
            continue;
        };
        validate_transaction_identity(transaction_id, transaction)?;
        let facts =
            address_transaction_facts(transaction, &parent_transactions, requested_script_hash)?;
        entry.transaction_index = Some(transaction.location.tx_index_in_block);
        entry.size_bytes = Some(transaction.public_facts.size_bytes);
        entry.component_counts = Some(encode_component_counts(transaction.public_facts.counts));
        entry.input_value_zat = facts.input_value_zat;
        entry.output_value_zat = Some(facts.output_value_zat);
        if let Some(net_value_zat) =
            complete_address_net_value(facts.input_value_zat, facts.output_value_zat)?
        {
            entry.net_value_zat = Some(net_value_zat);
        }
        entry.other_input_script_pub_keys = facts.other_input_script_pub_keys;
        entry.other_output_script_pub_keys = facts.other_output_script_pub_keys;
        entry.input_facts_complete = facts.input_facts_complete;
    }
    Ok(())
}

struct AddressTransactionFacts {
    input_value_zat: Option<u64>,
    output_value_zat: u64,
    other_input_script_pub_keys: Vec<Vec<u8>>,
    other_output_script_pub_keys: Vec<Vec<u8>>,
    input_facts_complete: bool,
}

fn address_transaction_facts(
    transaction: &TransactionFactsArtifact,
    parent_transactions: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    requested_script_hash: TransparentAddressScriptHash,
) -> Result<AddressTransactionFacts, Status> {
    let mut output_value_zat = 0_u64;
    let mut other_output_script_pub_keys = Vec::new();
    let mut seen_output_scripts = HashSet::new();
    for output in &transaction.transparent_outputs {
        if output.address_script_hash == requested_script_hash {
            output_value_zat = output_value_zat
                .checked_add(output.value_zat)
                .ok_or_else(|| {
                    ExplorerError::internal("transparent output value sum overflowed")
                })?;
        } else if seen_output_scripts.insert(output.script_pub_key.clone()) {
            other_output_script_pub_keys.push(output.script_pub_key.clone());
        }
    }

    if transaction.public_facts.is_coinbase {
        return Ok(AddressTransactionFacts {
            input_value_zat: Some(0),
            output_value_zat,
            other_input_script_pub_keys: Vec::new(),
            other_output_script_pub_keys,
            input_facts_complete: true,
        });
    }

    let mut input_value_zat = 0_u64;
    let mut input_facts_complete = true;
    let mut other_input_script_pub_keys = Vec::new();
    let mut seen_input_scripts = HashSet::new();
    for input in transaction
        .transparent_inputs
        .iter()
        .filter(|input| !input.spent_outpoint.is_coinbase_sentinel())
    {
        let parent = parent_transactions
            .get(&input.spent_outpoint.transaction_id)
            .and_then(Option::as_ref);
        let Some(parent) = parent else {
            input_facts_complete = false;
            continue;
        };
        validate_transaction_identity(input.spent_outpoint.transaction_id, parent)?;
        let prevout = parent
            .transparent_outputs
            .iter()
            .find(|output| output.output_index == input.spent_outpoint.output_index);
        let Some(prevout) = prevout else {
            input_facts_complete = false;
            continue;
        };
        if prevout.address_script_hash == requested_script_hash {
            input_value_zat = input_value_zat
                .checked_add(prevout.value_zat)
                .ok_or_else(|| ExplorerError::internal("transparent input value sum overflowed"))?;
        } else if seen_input_scripts.insert(prevout.script_pub_key.clone()) {
            other_input_script_pub_keys.push(prevout.script_pub_key.clone());
        }
    }

    Ok(AddressTransactionFacts {
        input_value_zat: input_facts_complete.then_some(input_value_zat),
        output_value_zat,
        other_input_script_pub_keys,
        other_output_script_pub_keys,
        input_facts_complete,
    })
}

fn complete_address_net_value(
    input_value_zat: Option<u64>,
    output_value_zat: u64,
) -> Result<Option<i64>, Status> {
    let Some(input_value_zat) = input_value_zat else {
        return Ok(None);
    };
    let net_value_zat = i128::from(output_value_zat) - i128::from(input_value_zat);
    i64::try_from(net_value_zat).map(Some).map_err(|_| {
        ExplorerError::internal("transparent address net value does not fit in sint64").into()
    })
}

fn validate_transaction_identity(
    expected_transaction_id: TransactionId,
    transaction: &TransactionFactsArtifact,
) -> Result<(), Status> {
    if transaction.location.transaction_id != expected_transaction_id
        || transaction.public_facts.transaction_id != expected_transaction_id
    {
        return Err(ExplorerError::internal(
            "canonical transaction facts do not match the requested transaction id",
        )
        .into());
    }
    Ok(())
}

fn encode_address_summary(
    summary: Option<DerivedTransparentAddressSummary>,
    requested_script_pub_key: Option<&[u8]>,
) -> WireTransparentAddressSummary {
    let Some(summary) = summary else {
        return WireTransparentAddressSummary {
            script_pub_key: requested_script_pub_key.map(<[u8]>::to_vec),
            balance_zat: 0,
            total_received_zat: None,
            total_sent_zat: None,
            distinct_transaction_count: None,
            first_seen_unix_seconds: None,
            last_seen_unix_seconds: None,
        };
    };
    WireTransparentAddressSummary {
        script_pub_key: summary
            .script_pub_key
            .or_else(|| requested_script_pub_key.map(<[u8]>::to_vec)),
        balance_zat: summary.balance_zat,
        total_received_zat: Some(summary.total_received_zat),
        total_sent_zat: Some(summary.total_sent_zat),
        distinct_transaction_count: Some(summary.distinct_transaction_count),
        first_seen_unix_seconds: minimum_optional(
            summary.first_seen_unix_seconds,
            summary.snapshot_first_seen_unix_seconds,
        ),
        last_seen_unix_seconds: maximum_optional(
            summary.last_seen_unix_seconds,
            summary.snapshot_last_seen_unix_seconds,
        ),
    }
}

fn encode_ranking_coverage(
    coverage: zinder_derive::TransparentAddressRankingCoverage,
) -> TransparentAddressRankingCoverage {
    TransparentAddressRankingCoverage {
        balance_complete_through_height: coverage.balance_complete_through_height.value(),
        history_complete_from_height: coverage
            .history_complete_from_height
            .map(zinder_core::BlockHeight::value),
        history_complete_through_height: coverage
            .history_complete_through_height
            .map(zinder_core::BlockHeight::value),
        lifetime_statistics_complete: coverage.lifetime_statistics_complete,
    }
}

fn minimum_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timestamp), None) | (None, Some(timestamp)) => Some(timestamp),
        (None, None) => None,
    }
}

fn maximum_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(timestamp), None) | (None, Some(timestamp)) => Some(timestamp),
        (None, None) => None,
    }
}

/// Owned row-key array, repeated three times in [`ScanKeys`].
type RowKey = [u8; TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN];

/// Iterator bounds and optional resume cursor returned by [`build_scan_keys`].
struct ScanKeys {
    start_key: RowKey,
    end_key: RowKey,
    resume_cursor: Option<RowKey>,
}

/// Computes the iterator bounds and the optional resume-cursor row.
fn build_scan_keys(prefix: [u8; ADDRESS_HASH_LEN], from_cursor: &[u8]) -> Result<ScanKeys, Status> {
    let mut start_key = [0u8; TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN];
    start_key[0..ADDRESS_HASH_LEN].copy_from_slice(&prefix);
    let mut end_key = [0xFFu8; TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN];
    end_key[0..ADDRESS_HASH_LEN].copy_from_slice(&prefix);
    if from_cursor.is_empty() {
        return Ok(ScanKeys {
            start_key,
            end_key,
            resume_cursor: None,
        });
    }
    let bytes: [u8; TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN] = from_cursor
        .try_into()
        .map_err(|_| ExplorerError::invalid_request("from_cursor must be 40 bytes"))?;
    if bytes[0..ADDRESS_HASH_LEN] != prefix {
        return Err(ExplorerError::invalid_request(
            "from_cursor address prefix does not match request address",
        )
        .into());
    }
    start_key = bytes;
    Ok(ScanKeys {
        start_key,
        end_key,
        resume_cursor: Some(bytes),
    })
}

fn decode_row_height(key: &RowKey) -> Result<u32, Status> {
    let height_bytes: [u8; 4] = key[HEIGHT_KEY_OFFSET..HEIGHT_KEY_END]
        .try_into()
        .map_err(|_| ExplorerError::internal("height segment not 4 bytes"))?;
    decode_height_key_descending(&height_bytes)
        .map(zinder_core::BlockHeight::value)
        .map_err(|error| ExplorerError::internal(error.to_string()).into())
}

/// Resolves an [`AddressLookup`] selector to a transparent script hash,
/// rejecting addresses whose network does not match the server network.
pub(crate) fn address_lookup_to_script_hash(
    address: &AddressLookup,
    network: Network,
) -> Result<TransparentAddressScriptHash, Status> {
    resolve_address_lookup(address, network).map(|resolved| resolved.script_hash)
}

struct ResolvedAddressLookup {
    script_hash: TransparentAddressScriptHash,
    script_pub_key: Option<Vec<u8>>,
}

fn resolve_address_lookup(
    address: &AddressLookup,
    network: Network,
) -> Result<ResolvedAddressLookup, Status> {
    let selector = address
        .selector
        .as_ref()
        .ok_or_else(|| ExplorerError::invalid_request("address selector arm is required"))?;
    match selector {
        AddressSelector::ScriptHash(bytes) => {
            let hash_bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| ExplorerError::invalid_request("script_hash must be 32 bytes"))?;
            Ok(ResolvedAddressLookup {
                script_hash: TransparentAddressScriptHash::from_bytes(hash_bytes),
                script_pub_key: None,
            })
        }
        AddressSelector::Address(text) => {
            let parsed = text.parse::<ZebraTransparentAddress>().map_err(|_| {
                ExplorerError::invalid_request("transparent address could not be parsed")
            })?;
            if !network_matches(parsed.network_kind(), network) {
                return Err(ExplorerError::invalid_request(
                    "transparent address network does not match server network",
                )
                .into());
            }
            let script_pub_key = parsed.script().as_raw_bytes().to_vec();
            if script_pub_key.is_empty() {
                return Err(ExplorerError::invalid_request(
                    "transparent address resolved to empty scriptPubKey",
                )
                .into());
            }
            Ok(ResolvedAddressLookup {
                script_hash: TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
                script_pub_key: Some(script_pub_key),
            })
        }
    }
}

fn network_matches(
    address_network: zebra_chain::parameters::NetworkKind,
    server_network: Network,
) -> bool {
    use zebra_chain::parameters::NetworkKind;
    matches!(
        (address_network, server_network),
        (NetworkKind::Mainnet, Network::ZcashMainnet)
            | (
                NetworkKind::Testnet | NetworkKind::Regtest,
                Network::ZcashTestnet | Network::ZcashRegtest
            )
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use prost::Message as _;
    use tempfile::tempdir;
    use tonic::Code;
    use zinder_core::{
        BlockHash, BlockHeight, LockTime, PrivacyShape, TransactionComponentCounts as CoreCounts,
        TransactionId, TransactionLocation, TransactionPublicFacts, TransactionVersion,
        TransparentInputFact, TransparentOutPoint, TransparentOutputFact,
    };
    use zinder_derive::{
        DeriveStoreOptions, TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
        TransparentAddressActivityConsumer,
        TransparentAddressRankingCoverage as DerivedRankingCoverage,
        TransparentAddressRankingMetadata, TransparentAddressScriptTypeTotals,
    };
    use zinder_store::RocksDbResourceBudget;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    #[test]
    fn offset_and_cursor_are_mutually_exclusive() {
        let outcome = validate_activity_pagination(1, &[0xAA]);
        assert!(matches!(outcome, Err(error) if error.code() == Code::InvalidArgument));
    }

    #[test]
    fn newer_ranking_generation_is_rejected_for_older_chain_epoch() {
        let metadata = ranking_metadata(101);
        let chain_epoch = wallet::ChainEpoch {
            chain_epoch_id: 7,
            visible_tip: Some(wallet::BlockTip {
                height: 100,
                hash: "11".repeat(32),
            }),
            ..Default::default()
        };

        let outcome = validate_ranking_metadata_at_chain_epoch(metadata, &chain_epoch);

        assert!(matches!(outcome, Err(error) if error.code() == Code::FailedPrecondition));
    }

    #[test]
    fn ranking_at_pinned_visible_tip_is_accepted() -> TestResult {
        let metadata = ranking_metadata(100);
        let chain_epoch = wallet::ChainEpoch {
            chain_epoch_id: 7,
            visible_tip: Some(wallet::BlockTip {
                height: 100,
                hash: "11".repeat(32),
            }),
            ..Default::default()
        };

        validate_ranking_metadata_at_chain_epoch(metadata, &chain_epoch)?;
        Ok(())
    }

    #[test]
    fn offset_skips_matching_rows_without_changing_cursor_order() -> TestResult {
        let tempdir = tempdir()?;
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: &[TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        let script_hash = TransparentAddressScriptHash::from_bytes([0x11; 32]);
        for (height, seed) in [(103, 3_u8), (102, 2_u8), (101, 1_u8)] {
            let record = TransparentAddressActivityRecord {
                transaction_id: format!("{seed:02x}").repeat(32),
                block_time_unix_seconds: i64::from(height),
                net_value_zat: Some(i64::from(seed)),
                input_count: 0,
                output_count: 1,
                prevout_resolution_status:
                    zinder_proto::v1::explorer::PrevoutResolutionStatus::Resolved as i32,
            };
            store.put_consumer(
                TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY,
                &TransparentAddressActivityConsumer::key_for_row(
                    script_hash,
                    BlockHeight::new(height),
                    0,
                ),
                &record.encode_to_vec(),
            )?;
        }

        let (entries, next_cursor) = scan_address_activity(
            &store,
            &ActivityScanParameters {
                script_hash,
                offset: 1,
                start_height: 0,
                end_height: u32::MAX,
                max_entries: 1,
                from_cursor: &[],
            },
        )?;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].block_height, 102);
        assert_eq!(next_cursor.len(), TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN);
        Ok(())
    }

    #[test]
    fn missing_summary_maps_to_valid_zero_state_with_requested_script() {
        let script = vec![0x76, 0xa9, 0x14, 0x88, 0xac];
        let summary = encode_address_summary(None, Some(&script));

        assert_eq!(summary.script_pub_key, Some(script));
        assert_eq!(summary.balance_zat, 0);
        assert_eq!(summary.total_received_zat, None);
        assert_eq!(summary.total_sent_zat, None);
        assert_eq!(summary.distinct_transaction_count, None);
        assert_eq!(summary.first_seen_unix_seconds, None);
        assert_eq!(summary.last_seen_unix_seconds, None);
    }

    #[test]
    fn summary_uses_snapshot_and_tail_timestamp_extrema() {
        let summary = encode_address_summary(
            Some(DerivedTransparentAddressSummary {
                script_pub_key: None,
                balance_zat: 900,
                total_received_zat: 1_200,
                total_sent_zat: 300,
                distinct_transaction_count: 7,
                first_seen_unix_seconds: Some(120),
                last_seen_unix_seconds: Some(180),
                snapshot_first_seen_unix_seconds: Some(100),
                snapshot_last_seen_unix_seconds: Some(200),
            }),
            Some(&[0x51]),
        );

        assert_eq!(summary.script_pub_key, Some(vec![0x51]));
        assert_eq!(summary.first_seen_unix_seconds, Some(100));
        assert_eq!(summary.last_seen_unix_seconds, Some(200));
        assert_eq!(summary.total_received_zat, Some(1_200));
    }

    #[test]
    fn transaction_facts_sum_requested_values_and_deduplicate_other_scripts() -> TestResult {
        let requested_script = vec![0x51];
        let other_script = vec![0x52];
        let requested_hash = TransparentAddressScriptHash::of_script_pub_key(&requested_script);
        let other_hash = TransparentAddressScriptHash::of_script_pub_key(&other_script);
        let requested_parent_id = transaction_id(1);
        let other_parent_id = transaction_id(2);
        let requested_parent = transaction_artifact(
            requested_parent_id,
            true,
            Vec::new(),
            vec![output(0, 400, requested_script.clone(), requested_hash)],
        );
        let other_parent = transaction_artifact(
            other_parent_id,
            true,
            Vec::new(),
            vec![output(0, 500, other_script.clone(), other_hash)],
        );
        let transaction = transaction_artifact(
            transaction_id(3),
            false,
            vec![
                TransparentInputFact::new(0, TransparentOutPoint::new(requested_parent_id, 0)),
                TransparentInputFact::new(1, TransparentOutPoint::new(other_parent_id, 0)),
                TransparentInputFact::new(2, TransparentOutPoint::new(other_parent_id, 0)),
            ],
            vec![
                output(0, 700, requested_script, requested_hash),
                output(1, 100, other_script.clone(), other_hash),
                output(2, 200, other_script.clone(), other_hash),
            ],
        );
        let parents = HashMap::from([
            (requested_parent_id, Some(requested_parent)),
            (other_parent_id, Some(other_parent)),
        ]);

        let facts = address_transaction_facts(&transaction, &parents, requested_hash)?;

        assert_eq!(facts.input_value_zat, Some(400));
        assert_eq!(facts.output_value_zat, 700);
        assert_eq!(
            facts.other_input_script_pub_keys,
            vec![other_script.clone()]
        );
        assert_eq!(facts.other_output_script_pub_keys, vec![other_script]);
        assert!(facts.input_facts_complete);
        Ok(())
    }

    #[test]
    fn unresolved_parent_suppresses_partial_input_value() -> TestResult {
        let requested_script = vec![0x51];
        let requested_hash = TransparentAddressScriptHash::of_script_pub_key(&requested_script);
        let resolved_parent_id = transaction_id(1);
        let missing_parent_id = transaction_id(2);
        let transaction = transaction_artifact(
            transaction_id(3),
            false,
            vec![
                TransparentInputFact::new(0, TransparentOutPoint::new(resolved_parent_id, 0)),
                TransparentInputFact::new(1, TransparentOutPoint::new(missing_parent_id, 0)),
            ],
            Vec::new(),
        );
        let parents = HashMap::from([
            (
                resolved_parent_id,
                Some(transaction_artifact(
                    resolved_parent_id,
                    true,
                    Vec::new(),
                    vec![output(0, 400, requested_script, requested_hash)],
                )),
            ),
            (missing_parent_id, None),
        ]);

        let facts = address_transaction_facts(&transaction, &parents, requested_hash)?;

        assert_eq!(facts.input_value_zat, None);
        assert!(!facts.input_facts_complete);
        Ok(())
    }

    #[test]
    fn coinbase_has_complete_empty_input_facts() -> TestResult {
        let requested_hash = TransparentAddressScriptHash::from_bytes([0x11; 32]);
        let transaction = transaction_artifact(
            transaction_id(3),
            true,
            vec![TransparentInputFact::new(
                0,
                TransparentOutPoint::COINBASE_SENTINEL,
            )],
            Vec::new(),
        );

        let facts = address_transaction_facts(&transaction, &HashMap::new(), requested_hash)?;

        assert_eq!(facts.input_value_zat, Some(0));
        assert!(facts.input_facts_complete);
        assert!(facts.other_input_script_pub_keys.is_empty());
        Ok(())
    }

    #[test]
    fn complete_canonical_values_recover_a_missing_activity_net_value() -> TestResult {
        assert_eq!(
            complete_address_net_value(Some(0), 987_500_260)?,
            Some(987_500_260)
        );
        assert_eq!(complete_address_net_value(Some(100), 40)?, Some(-60));
        assert_eq!(complete_address_net_value(None, 40)?, None);
        Ok(())
    }

    fn transaction_id(seed: u8) -> TransactionId {
        TransactionId::from_bytes([seed; 32])
    }

    fn ranking_metadata(height: u32) -> TransparentAddressRankingMetadata {
        TransparentAddressRankingMetadata {
            generation: 9,
            positive_address_count: 1,
            total_positive_balance_zat: 100,
            top_10_balance_zat: 100,
            top_100_balance_zat: 100,
            p2pkh: TransparentAddressScriptTypeTotals::default(),
            p2sh: TransparentAddressScriptTypeTotals::default(),
            coverage: DerivedRankingCoverage {
                balance_complete_through_height: BlockHeight::new(height),
                history_complete_from_height: Some(BlockHeight::new(1)),
                history_complete_through_height: Some(BlockHeight::new(height)),
                lifetime_statistics_complete: true,
            },
        }
    }

    fn output(
        output_index: u32,
        value_zat: u64,
        script_pub_key: Vec<u8>,
        address_script_hash: TransparentAddressScriptHash,
    ) -> TransparentOutputFact {
        TransparentOutputFact::new(output_index, value_zat, script_pub_key, address_script_hash)
    }

    fn transaction_artifact(
        transaction_id: TransactionId,
        is_coinbase: bool,
        transparent_inputs: Vec<TransparentInputFact>,
        transparent_outputs: Vec<TransparentOutputFact>,
    ) -> TransactionFactsArtifact {
        TransactionFactsArtifact::new(
            TransactionLocation::new(
                transaction_id,
                BlockHeight::new(100),
                BlockHash::from_bytes([0xAA; 32]),
                3,
            ),
            TransactionPublicFacts {
                transaction_id,
                auth_digest: None,
                wtxid: None,
                version: TransactionVersion::V5,
                consensus_branch_id: None,
                lock_time: LockTime::Unlocked,
                expiry_height: None,
                size_bytes: 512,
                counts: CoreCounts::EMPTY,
                privacy_shape: PrivacyShape::Unclassified,
                is_coinbase,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                unsupported_sections: Vec::new(),
            },
        )
        .with_transparent_facts(transparent_inputs, transparent_outputs)
    }
}
