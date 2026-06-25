//! `ExplorerQuery.TransparentAddressActivity` handler.
//!
//! Reads the confirmed-activity feed materialized by
//! [`zinder_derive::TransparentAddressActivityConsumer`]
//! out of the consumer-owned `transparent_address_activity` column family.
//! The storage layout sorts newest-first per address, so the handler
//! serves pages in that order; clients that want oldest-first reverse
//! client-side.

use prost::Message as _;
use tonic::{Request, Response, Status};
use zebra_chain::transparent::Address as ZebraTransparentAddress;
use zinder_core::wire::{
    decode_address_script_hash, decode_height_key_descending, encode_address_script_hash,
};
use zinder_core::{Network, TransparentAddressScriptHash};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1;
use zinder_proto::v1::explorer::{
    TransparentAddressActivityEntry, TransparentAddressActivityRecord,
    TransparentAddressActivityRequest, TransparentAddressActivityResponse,
};
use zinder_proto::v1::wallet::{
    AddressLookup, LatestBlockRequest, address_lookup::Selector as AddressSelector,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use zinder_derive::{
    DeriveStore, TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY, TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN,
};

/// Hard cap on the activity rows one page returns.
const MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST: u32 = 256;

/// Default entries when the caller passes `max_entries = 0`.
const DEFAULT_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES: u32 = 64;

const ADDRESS_HASH_LEN: usize = 32;
const HEIGHT_KEY_OFFSET: usize = ADDRESS_HASH_LEN;
const HEIGHT_KEY_END: usize = HEIGHT_KEY_OFFSET + 4;

/// Executes one `ExplorerQuery.TransparentAddressActivity` request.
pub(crate) async fn handle_transparent_address_activity(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: Network,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<TransparentAddressActivityRequest>,
) -> Result<Response<TransparentAddressActivityResponse>, Status> {
    let inner = request.into_inner();
    let address = inner
        .address
        .ok_or_else(|| ExplorerError::invalid_request("address selector is required"))?;
    let script_hash = address_lookup_to_script_hash(&address, network)?;
    let max_entries = clamp_max_entries(
        inner.max_entries,
        DEFAULT_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES,
        MAX_TRANSPARENT_ADDRESS_ACTIVITY_ENTRIES_PER_REQUEST,
    );
    let (entries, next_cursor) = scan_address_activity(
        derive_store,
        &ActivityScanParameters {
            script_hash,
            start_height: inner.start_height,
            end_height: inner.end_height,
            max_entries,
            from_cursor: inner.from_cursor.as_slice(),
        },
    )?;
    let latest = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner();
    let chain_epoch = latest
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("LatestBlockResponse.chain_view.chain_epoch missing")
        })?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(TransparentAddressActivityResponse {
        freshness: Some(freshness),
        entries,
        next_cursor,
    }))
}

/// Bundled inputs to [`scan_address_activity`].
struct ActivityScanParameters<'a> {
    script_hash: TransparentAddressScriptHash,
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
        start_key,
        end_key,
        resume_cursor,
    } = build_scan_keys(prefix, parameters.from_cursor)?;

    let scan_cap =
        (parameters.max_entries as usize).saturating_add(usize::from(resume_cursor.is_some()));
    let rows = derive_store
        .range_iterate_consumer(
            TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY,
            &start_key,
            &end_key,
            scan_cap,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;

    let mut entries: Vec<TransparentAddressActivityEntry> = Vec::with_capacity(rows.len());
    let mut last_key: Option<RowKey> = None;
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
            break;
        }
        let height = decode_row_height(&key_array)?;
        if height < parameters.start_height || height > parameters.end_height {
            continue;
        }
        let record = TransparentAddressActivityRecord::decode(payload.as_slice())
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        entries.push(TransparentAddressActivityEntry {
            transaction_id: record.transaction_id,
            block_height: height,
            block_time_unix_seconds: record.block_time_unix_seconds,
            net_value_zat: record.net_value_zat,
            input_count: record.input_count,
            output_count: record.output_count,
            prevout_resolution_status: record.prevout_resolution_status,
        });
        last_key = Some(key_array);
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= parameters.max_entries {
            break;
        }
    }

    let next_cursor = if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= parameters.max_entries
    {
        last_key.map_or_else(Vec::new, |key| key.to_vec())
    } else {
        Vec::new()
    };
    Ok((entries, next_cursor))
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
            Ok(TransparentAddressScriptHash::from_bytes(hash_bytes))
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
            Ok(TransparentAddressScriptHash::of_script_pub_key(
                &script_pub_key,
            ))
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
