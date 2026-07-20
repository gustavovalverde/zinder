//! `ExplorerQuery.TransactionDetail` handler.
//!
//! Reads one transaction through `WalletQuery.Transaction` and surfaces the
//! status location alongside
//! the cross-cutting [`ExplorerFreshness`] envelope. The handler owns the
//! conversion between the `zinder_core::TransactionPublicFacts` shape and
//! its proto mirror; the source-side parser is the single source of truth
//! for everything else.

use std::collections::{HashMap, HashSet};

use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, ConsensusBranchId, LockTime as CoreLockTime, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST,
    NetworkUpgradeActivations, TransactionFactsArtifact, TransactionId,
    TransactionPublicFacts as CoreFacts, TransactionVersion as CoreTransactionVersion,
    TransparentOutPoint, TransparentOutputFact,
    wire::{
        decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex, encode_branch_id_hex,
        encode_rpc_auth_digest_hex, encode_rpc_transaction_id_hex, encode_rpc_wtxid_hex,
    },
};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_DETAIL_V3;
use zinder_proto::wire::encode_privacy_shape;

use zinder_materialized_views::{MaterializedViewStore, TransactionFeesConsumer};
use zinder_proto::v1::{
    explorer::{
        LockTime as WireLockTime, LockTimeUnlocked, TransactionComponentCounts,
        TransactionDetailRequest, TransactionDetailResponse, TransactionFeesRecord,
        TransactionPublicFacts as WireFacts, TransactionVersion as WireVersion,
        TransactionVersionKind, TransparentOutput as WireTransparentOutput,
        lock_time as wire_lock_time,
    },
    wallet::{
        self, TransactionLocation as WireTransactionLocation,
        transaction_location as wire_location, wallet_query_client::WalletQueryClient,
    },
};
use zinder_runtime::AuthenticatedChannel;
use zinder_store::{
    ChainEpochReader, SecondaryChainStore, chain_epoch_from_message, status_from_store_error,
};

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use super::intrinsic_value_balances::resolve_transaction_intrinsic_value_balances;
use super::require_matching_chain_epoch;
use super::transparent_input::{
    encode_mined_transparent_inputs, encode_unresolved_transparent_inputs, parent_transaction_ids,
};

/// Read backends the `TransactionDetail` handler needs from the adapter.
///
/// Bundled into one struct so the handler signature stays under the
/// workspace's clippy `too-many-arguments` threshold and so adding a new
/// shared dependency does not ripple through every call site.
pub(crate) struct TransactionDetailContext<'context> {
    pub(crate) chain_store: Option<&'context SecondaryChainStore>,
    pub(crate) materialized_view_store: Option<&'context MaterializedViewStore>,
    pub(crate) network: zinder_core::Network,
    pub(crate) upstream_observation_cache: &'context UpstreamObservationCache,
}

/// Executes one `ExplorerQuery.TransactionDetail` request.
pub(crate) async fn query_transaction_detail(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    context: TransactionDetailContext<'_>,
    request: Request<TransactionDetailRequest>,
) -> Result<Response<TransactionDetailResponse>, Status> {
    let TransactionDetailContext {
        chain_store,
        materialized_view_store,
        network,
        upstream_observation_cache,
    } = context;
    let inner = request.into_inner();
    let transaction_id = decode_rpc_transaction_id_hex(&inner.transaction_id)
        .map_err(|error| ExplorerError::invalid_request(error.to_string()))?;

    let status_response = wallet_client
        .transaction(Request::new(wallet::TransactionRequest {
            transaction_id: encode_rpc_transaction_id_hex(transaction_id),
            at_epoch_id: inner.at_epoch_id,
        }))
        .await?
        .into_inner();
    let chain_epoch = status_response
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| ExplorerError::internal("WalletQuery.Transaction missing chain_epoch"))?;
    let location = status_response
        .location
        .and_then(|location| location.location)
        .ok_or_else(|| {
            ExplorerError::internal("WalletQuery.Transaction response missing location")
        })?;
    let canonical_reader = canonical_reader_for_location(chain_store, &chain_epoch, &location)?;

    let transaction =
        resolve_facts_and_location(canonical_reader.as_ref(), network, transaction_id, location)?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            materialized_view_store,
            EXPLORER_TRANSACTION_DETAIL_V3,
            Some(chain_epoch.clone()),
            0,
        )?,
    )
    .await;

    let parent_transactions = read_parent_transaction_facts(
        canonical_reader.as_ref(),
        transaction.canonical_artifact.as_ref(),
    )?;
    let fees = resolve_fee_record(
        materialized_view_store,
        transaction.canonical_artifact.as_ref(),
        &parent_transactions,
    )?;
    let transparent_rows = resolve_transparent_rows(
        wallet_client,
        &chain_epoch,
        &transaction,
        &parent_transactions,
        fees.as_ref(),
    )
    .await?;
    let intrinsic_value_balances = resolve_detail_intrinsic_value_balances(
        canonical_reader.as_ref(),
        network,
        transaction.canonical_artifact.as_ref(),
    )?;
    let (paid_fee_zat, prevout_resolution_status) = fees.as_ref().map_or((None, 0), |record| {
        (record.paid_fee_zat, record.prevout_resolution_status)
    });
    Ok(Response::new(TransactionDetailResponse {
        freshness: Some(freshness),
        facts: Some(encode_public_facts(&transaction.facts)),
        location: Some(transaction.location),
        paid_fee_zat,
        prevout_resolution_status,
        transparent_inputs: transparent_rows.inputs,
        transparent_outputs: transparent_rows.outputs,
        intrinsic_value_balances,
    }))
}

struct ResolvedTransactionDetail {
    facts: CoreFacts,
    location: WireTransactionLocation,
    canonical_artifact: Option<TransactionFactsArtifact>,
    transient_fact_set: Option<zinder_source::TransactionPublicFactSet>,
}

struct ResolvedTransparentRows {
    inputs: Vec<zinder_proto::v1::explorer::TransparentInput>,
    outputs: Vec<WireTransparentOutput>,
}

fn resolve_detail_intrinsic_value_balances(
    canonical_reader: Option<&ChainEpochReader<'_>>,
    network: zinder_core::Network,
    transaction: Option<&TransactionFactsArtifact>,
) -> Result<Option<zinder_proto::v1::explorer::TransactionIntrinsicValueBalances>, Status> {
    let (Some(reader), Some(transaction)) = (canonical_reader, transaction) else {
        return Ok(None);
    };
    let transaction_id = transaction.location.transaction_id;
    Ok(resolve_transaction_intrinsic_value_balances(
        reader,
        network,
        &[(transaction_id, transaction.location)],
    )?
    .remove(&transaction_id))
}

async fn resolve_transparent_rows(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    chain_epoch: &wallet::ChainEpoch,
    transaction: &ResolvedTransactionDetail,
    parent_transactions: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    fees: Option<&TransactionFeesRecord>,
) -> Result<ResolvedTransparentRows, Status> {
    let inputs = transaction.canonical_artifact.as_ref().map_or_else(
        || {
            transaction
                .transient_fact_set
                .as_ref()
                .map_or_else(Vec::new, |fact_set| {
                    encode_unresolved_transparent_inputs(&fact_set.transparent_inputs)
                })
        },
        |artifact| encode_mined_transparent_inputs(artifact, parent_transactions, fees),
    );
    let output_spends = resolve_transparent_output_spends(
        wallet_client,
        chain_epoch,
        transaction.canonical_artifact.as_ref(),
    )
    .await?;
    let outputs = transaction.canonical_artifact.as_ref().map_or_else(
        || {
            transaction
                .transient_fact_set
                .as_ref()
                .map_or_else(Vec::new, |fact_set| {
                    encode_transparent_output_facts(
                        fact_set.public_facts.transaction_id,
                        &fact_set.transparent_outputs,
                        &output_spends,
                    )
                })
        },
        |artifact| encode_transparent_outputs(artifact, &output_spends),
    );
    Ok(ResolvedTransparentRows { inputs, outputs })
}

/// Resolves the parsed public facts and the wire location for one transaction.
///
/// The `location` oneof is carried through verbatim so the explorer detail
/// returns the same `{ mined, in_mempool }` shape the wallet plane answered
/// with. Facts come from the canonical store for mined transactions and from
/// raw bytes for mempool transactions.
fn resolve_facts_and_location(
    canonical_reader: Option<&ChainEpochReader<'_>>,
    network: zinder_core::Network,
    transaction_id: zinder_core::TransactionId,
    location: wire_location::Location,
) -> Result<ResolvedTransactionDetail, Status> {
    let (facts, inner, canonical_artifact, transient_fact_set) = match location {
        wire_location::Location::Mined(mined) => {
            let branch_id = mined_consensus_branch_id(&mined)?;
            let artifact = read_mined_transaction_facts(canonical_reader, transaction_id)?;
            let mut facts = artifact.public_facts.clone();
            facts.consensus_branch_id = Some(branch_id);
            (
                facts,
                wire_location::Location::Mined(mined),
                Some(artifact),
                None,
            )
        }
        wire_location::Location::InMempool(mempool) => {
            let activations = NetworkUpgradeActivations::empty(network);
            let fact_set = zinder_source::parse_transaction_public_fact_set(
                &mempool.payload_bytes,
                None,
                &activations,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
            if fact_set.public_facts.transaction_id != transaction_id {
                return Err(ExplorerError::internal(
                    "WalletQuery.Transaction mempool payload transaction id mismatch",
                )
                .into());
            }
            (
                fact_set.public_facts.clone(),
                wire_location::Location::InMempool(mempool),
                None,
                Some(fact_set),
            )
        }
    };
    Ok(ResolvedTransactionDetail {
        facts,
        location: WireTransactionLocation {
            location: Some(inner),
        },
        canonical_artifact,
        transient_fact_set,
    })
}

fn resolve_fee_record(
    materialized_view_store: Option<&MaterializedViewStore>,
    transaction: Option<&TransactionFactsArtifact>,
    parent_transactions: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
) -> Result<Option<TransactionFeesRecord>, Status> {
    let Some(transaction) = transaction else {
        return Ok(None);
    };
    if transaction.public_facts.is_coinbase {
        return Ok(None);
    }
    let Some(materialized_view_store) = materialized_view_store else {
        return Ok(None);
    };

    let projected = TransactionFeesConsumer::read_fees_record(
        materialized_view_store,
        transaction.location.transaction_id,
        transaction.public_facts.privacy_shape,
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let recovered = TransactionFeesConsumer::recover_fee_record_from_parent_facts(
        transaction,
        parent_transactions,
    );
    Ok(recovered
        .as_ref()
        .map(|record| {
            TransactionFeesConsumer::merge_fee_records(transaction, projected.as_ref(), record)
        })
        .or(projected))
}

fn canonical_reader_for_location<'store>(
    chain_store: Option<&'store SecondaryChainStore>,
    chain_epoch: &wallet::ChainEpoch,
    location: &wire_location::Location,
) -> Result<Option<ChainEpochReader<'store>>, Status> {
    if !matches!(location, wire_location::Location::Mined(_)) {
        return Ok(None);
    }
    let store = chain_store.ok_or_else(|| {
        ExplorerError::dependency_not_configured(
            "TransactionDetail requires the canonical store; configure --storage-path",
        )
    })?;
    store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    require_matching_chain_epoch(core_epoch, reader.chain_epoch())?;
    Ok(Some(reader))
}

fn read_parent_transaction_facts(
    canonical_reader: Option<&ChainEpochReader<'_>>,
    transaction: Option<&TransactionFactsArtifact>,
) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, Status> {
    let Some(transaction) = transaction else {
        return Ok(HashMap::new());
    };
    let parent_transaction_ids = parent_transaction_ids([transaction]);
    if parent_transaction_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let reader = canonical_reader.ok_or_else(|| {
        ExplorerError::dependency_not_configured(
            "TransactionDetail prevout resolution requires the canonical store",
        )
    })?;
    reader
        .transaction_facts_by_ids(&parent_transaction_ids)
        .map_err(|error| status_from_store_error(&error))
}

async fn resolve_transparent_output_spends(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    chain_epoch: &wallet::ChainEpoch,
    transaction: Option<&TransactionFactsArtifact>,
) -> Result<HashMap<TransparentOutPoint, wallet::TransparentSpend>, Status> {
    let Some(transaction) = transaction else {
        return Ok(HashMap::new());
    };
    let requested_outpoints: Vec<TransparentOutPoint> = transaction
        .transparent_outputs
        .iter()
        .map(|output| {
            TransparentOutPoint::new(transaction.location.transaction_id, output.output_index)
        })
        .collect();
    if requested_outpoints.is_empty() {
        return Ok(HashMap::new());
    }
    let requested_outpoint_set: HashSet<TransparentOutPoint> =
        requested_outpoints.iter().copied().collect();
    if requested_outpoint_set.len() != requested_outpoints.len() {
        return Err(ExplorerError::internal(
            "TransactionDetail transaction artifact contains duplicate transparent output indexes",
        )
        .into());
    }

    let mut spends = HashMap::new();
    for outpoint_batch in requested_outpoints.chunks(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST) {
        let batch_spends =
            fetch_transparent_output_spend_batch(wallet_client, chain_epoch, outpoint_batch)
                .await?;
        insert_transparent_output_spends(&mut spends, &requested_outpoint_set, batch_spends)?;
    }
    Ok(spends)
}

async fn fetch_transparent_output_spend_batch(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    expected_chain_epoch: &wallet::ChainEpoch,
    outpoints: &[TransparentOutPoint],
) -> Result<Vec<wallet::TransparentSpend>, Status> {
    let response = wallet_client
        .transparent_spends_by_outpoint(Request::new(wallet::TransparentSpendsByOutpointRequest {
            outpoints: outpoints
                .iter()
                .map(|outpoint| wallet::OutPoint {
                    transaction_id: encode_rpc_transaction_id_hex(outpoint.transaction_id),
                    output_index: outpoint.output_index,
                })
                .collect(),
            at_epoch_id: Some(expected_chain_epoch.chain_epoch_id),
        }))
        .await?
        .into_inner();
    let response_chain_epoch = response
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("WalletQuery.TransparentSpendsByOutpoint missing chain_epoch")
        })?;
    let expected_core_epoch = chain_epoch_from_message(expected_chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let response_core_epoch = chain_epoch_from_message(response_chain_epoch)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    require_matching_chain_epoch(expected_core_epoch, response_core_epoch)?;
    Ok(response.spends)
}

fn insert_transparent_output_spends(
    spends: &mut HashMap<TransparentOutPoint, wallet::TransparentSpend>,
    requested_outpoints: &HashSet<TransparentOutPoint>,
    batch_spends: Vec<wallet::TransparentSpend>,
) -> Result<(), Status> {
    for spend in batch_spends {
        let spent_outpoint = spend
            .spent_outpoint
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("TransparentSpend missing spent_outpoint"))?;
        let outpoint = TransparentOutPoint::new(
            decode_rpc_transaction_id_hex(&spent_outpoint.transaction_id)
                .map_err(|error| ExplorerError::internal(error.to_string()))?,
            spent_outpoint.output_index,
        );
        if !requested_outpoints.contains(&outpoint) {
            return Err(ExplorerError::internal(
                "WalletQuery.TransparentSpendsByOutpoint returned an unrequested outpoint",
            )
            .into());
        }
        decode_rpc_transaction_id_hex(&spend.spending_transaction_id)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        let spending_block = spend
            .spending_block
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("TransparentSpend missing spending_block"))?;
        decode_rpc_block_hash_hex(&spending_block.hash)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if spends.insert(outpoint, spend).is_some() {
            return Err(ExplorerError::internal(
                "WalletQuery.TransparentSpendsByOutpoint returned a duplicate outpoint",
            )
            .into());
        }
    }
    Ok(())
}

fn encode_transparent_outputs(
    artifact: &TransactionFactsArtifact,
    spends: &HashMap<TransparentOutPoint, wallet::TransparentSpend>,
) -> Vec<WireTransparentOutput> {
    encode_transparent_output_facts(
        artifact.location.transaction_id,
        &artifact.transparent_outputs,
        spends,
    )
}

fn encode_transparent_output_facts(
    transaction_id: TransactionId,
    outputs: &[TransparentOutputFact],
    spends: &HashMap<TransparentOutPoint, wallet::TransparentSpend>,
) -> Vec<WireTransparentOutput> {
    outputs
        .iter()
        .map(|output| {
            let outpoint = TransparentOutPoint::new(transaction_id, output.output_index);
            WireTransparentOutput {
                output_index: output.output_index,
                output: Some(wallet::TransparentOutput {
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key.clone(),
                }),
                spent_by: spends.get(&outpoint).cloned(),
            }
        })
        .collect()
}

fn read_mined_transaction_facts(
    canonical_reader: Option<&ChainEpochReader<'_>>,
    transaction_id: zinder_core::TransactionId,
) -> Result<TransactionFactsArtifact, Status> {
    let reader = canonical_reader.ok_or_else(|| {
        ExplorerError::dependency_not_configured(
            "TransactionDetail requires the canonical store; configure --storage-path",
        )
    })?;
    let artifact = reader
        .transaction_facts_by_id(transaction_id)
        .map_err(|error| status_from_store_error(&error))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "transaction facts are not available for {transaction_id:?}"
            ))
        })?;
    Ok(artifact)
}

fn mined_consensus_branch_id(
    mined: &wallet::MinedTransaction,
) -> Result<ConsensusBranchId, Status> {
    let chain_context = mined
        .chain_context
        .as_ref()
        .ok_or_else(|| ExplorerError::internal("MinedTransaction missing chain context"))?;
    Ok(ConsensusBranchId::new(chain_context.consensus_branch_id))
}

pub(crate) fn encode_public_facts(facts: &CoreFacts) -> WireFacts {
    WireFacts {
        transaction_id: encode_rpc_transaction_id_hex(facts.transaction_id),
        auth_digest: facts
            .auth_digest
            .map(encode_rpc_auth_digest_hex)
            .unwrap_or_default(),
        wtxid: facts.wtxid.map(encode_rpc_wtxid_hex).unwrap_or_default(),
        version: Some(encode_transaction_version(facts.version)),
        consensus_branch_id_hex: facts
            .consensus_branch_id
            .map(encode_branch_id_hex)
            .unwrap_or_default(),
        lock_time: Some(encode_lock_time(facts.lock_time)),
        expiry_height: facts.expiry_height.map_or(0, BlockHeight::value),
        size_bytes: facts.size_bytes,
        counts: Some(encode_component_counts(facts.counts)),
        privacy_shape: encode_privacy_shape(facts.privacy_shape) as i32,
        is_coinbase: facts.is_coinbase,
    }
}

/// Encodes the public component counts shared by transaction and mempool rows.
pub(crate) fn encode_component_counts(
    counts: zinder_core::TransactionComponentCounts,
) -> TransactionComponentCounts {
    TransactionComponentCounts {
        transparent_input_count: counts.transparent_input_count,
        transparent_output_count: counts.transparent_output_count,
        sapling_spend_count: counts.sapling_spend_count,
        sapling_output_count: counts.sapling_output_count,
        orchard_action_count: counts.orchard_action_count,
        ironwood_action_count: counts.ironwood_action_count,
        sprout_joinsplit_count: counts.sprout_joinsplit_count,
    }
}

fn encode_transaction_version(version: CoreTransactionVersion) -> WireVersion {
    let (kind, version_group_id) = match version {
        CoreTransactionVersion::V1 => (TransactionVersionKind::V1, None),
        CoreTransactionVersion::V2 => (TransactionVersionKind::V2, None),
        CoreTransactionVersion::V3 => (TransactionVersionKind::V3, None),
        CoreTransactionVersion::V4 => (TransactionVersionKind::V4, None),
        CoreTransactionVersion::V5 => (TransactionVersionKind::V5, None),
        CoreTransactionVersion::V6 => (TransactionVersionKind::V6, None),
        CoreTransactionVersion::Unsupported {
            version_group_id, ..
        } => (TransactionVersionKind::Unsupported, version_group_id),
    };
    WireVersion {
        kind: kind as i32,
        effective_version: version.effective_version(),
        version_group_id,
    }
}

fn encode_lock_time(lock_time: CoreLockTime) -> WireLockTime {
    let kind = match lock_time {
        CoreLockTime::Unlocked => wire_lock_time::Kind::Unlocked(LockTimeUnlocked {}),
        CoreLockTime::Height(height) => wire_lock_time::Kind::Height(height.value()),
        CoreLockTime::UnixSeconds(seconds) => wire_lock_time::Kind::UnixSeconds(seconds),
    };
    WireLockTime { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use zinder_core::{
        ArtifactSchemaVersion, BlockHash, ChainEpoch as CoreChainEpoch, ChainEpochId,
        ChainTipMetadata, Network, TransactionId, TransactionLocation, TransparentInputFact,
        TransparentOutPoint, UnixTimestampMillis,
    };
    use zinder_proto::v1::explorer::TransparentInputValueRecord;
    use zinder_testkit::synthetic_transaction_public_facts;

    use super::*;

    #[test]
    fn mempool_detail_preserves_ordered_transparent_rows_from_payload() -> eyre::Result<()> {
        let payload_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let parsed =
            zinder_source::parse_transaction_public_fact_set(&payload_bytes, None, &activations)?;
        let transaction_id = parsed.public_facts.transaction_id;

        let resolved = resolve_facts_and_location(
            None,
            Network::ZcashRegtest,
            transaction_id,
            wire_location::Location::InMempool(wallet::MempoolTransaction {
                payload_bytes,
                first_seen_unix_seconds: 1_700_000_000,
            }),
        )?;
        let fact_set = resolved
            .transient_fact_set
            .ok_or_else(|| eyre::eyre!("mempool fact set missing"))?;
        let inputs = encode_unresolved_transparent_inputs(&fact_set.transparent_inputs);
        let outputs = encode_transparent_output_facts(
            transaction_id,
            &fact_set.transparent_outputs,
            &HashMap::new(),
        );

        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].input_index, 0);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].output_index, 0);
        assert_eq!(
            outputs[0].output.as_ref().map(|output| output.value_zat),
            Some(1_000)
        );
        assert_eq!(outputs[1].output_index, 1);
        assert_eq!(
            outputs[1].output.as_ref().map(|output| output.value_zat),
            Some(2_500)
        );
        assert!(outputs.iter().all(|output| output.spent_by.is_none()));
        Ok(())
    }

    fn transparent_transaction_bytes() -> Vec<u8> {
        let mut bytes = vec![1, 0, 0, 0, 1];
        bytes.extend_from_slice(&[0xA5; 32]);
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.push(2);
        for (value_zat, script_pub_key) in [(1_000_u64, 0x51_u8), (2_500_u64, 0x52_u8)] {
            bytes.extend_from_slice(&value_zat.to_le_bytes());
            bytes.extend_from_slice(&[1, script_pub_key]);
        }
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes
    }

    #[test]
    fn canonical_fee_fallback_stays_behind_materialized_view_capability() {
        let transaction_id = TransactionId::from_bytes([9; 32]);
        let transaction = TransactionFactsArtifact::new(
            TransactionLocation::new(
                transaction_id,
                BlockHeight::new(1),
                BlockHash::from_bytes([8; 32]),
                1,
            ),
            synthetic_transaction_public_facts(transaction_id, 64),
        );

        let outcome = resolve_fee_record(None, Some(&transaction), &HashMap::new());

        assert!(matches!(outcome, Ok(None)));
    }

    #[test]
    fn transparent_input_keeps_projected_value_when_parent_script_is_missing() {
        let transaction_id = TransactionId::from_bytes([9; 32]);
        let parent_transaction_id = TransactionId::from_bytes([7; 32]);
        let transaction = TransactionFactsArtifact::new(
            TransactionLocation::new(
                transaction_id,
                BlockHeight::new(1),
                BlockHash::from_bytes([8; 32]),
                1,
            ),
            synthetic_transaction_public_facts(transaction_id, 64),
        )
        .with_transparent_facts(
            vec![TransparentInputFact::new(
                0,
                TransparentOutPoint::new(parent_transaction_id, 3),
            )],
            Vec::new(),
        );
        let fees = TransactionFeesRecord {
            transparent_inputs: vec![TransparentInputValueRecord {
                input_index: 0,
                value_zat: Some(42_000),
            }],
            ..Default::default()
        };
        let parent_transactions = HashMap::from([(parent_transaction_id, None)]);

        let encoded =
            encode_mined_transparent_inputs(&transaction, &parent_transactions, Some(&fees));

        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0].value_zat, Some(42_000));
        assert_eq!(encoded[0].script_pub_key, None);
    }

    #[test]
    fn malformed_non_coinbase_sentinel_is_not_exposed_as_a_transparent_input() {
        let transaction_id = TransactionId::from_bytes([9; 32]);
        let transaction = TransactionFactsArtifact::new(
            TransactionLocation::new(
                transaction_id,
                BlockHeight::new(1),
                BlockHash::from_bytes([8; 32]),
                1,
            ),
            synthetic_transaction_public_facts(transaction_id, 64),
        )
        .with_transparent_facts(
            vec![TransparentInputFact::new(
                0,
                TransparentOutPoint::COINBASE_SENTINEL,
            )],
            Vec::new(),
        );

        let encoded = encode_mined_transparent_inputs(&transaction, &HashMap::new(), None);

        assert!(encoded.is_empty());
    }

    #[test]
    fn chain_epoch_identity_rejects_equal_ids_with_different_tips() {
        let expected = CoreChainEpoch {
            id: ChainEpochId::new(7),
            network: Network::ZcashRegtest,
            visible_tip_height: BlockHeight::new(9),
            visible_tip_hash: BlockHash::from_bytes([1; 32]),
            settled_tip_height: BlockHeight::new(8),
            settled_tip_hash: BlockHash::from_bytes([2; 32]),
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::new(0, 0, 0),
            created_at: UnixTimestampMillis::new(123),
        };
        let actual = CoreChainEpoch {
            visible_tip_hash: BlockHash::from_bytes([3; 32]),
            ..expected
        };

        assert!(require_matching_chain_epoch(expected, actual).is_err());
    }
}
