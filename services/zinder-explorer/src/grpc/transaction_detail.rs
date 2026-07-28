//! `ExplorerQuery.TransactionDetail` handler.
//!
//! Reads one transaction through `WalletQuery.Transaction` and surfaces the
//! status location alongside
//! the cross-cutting [`ExplorerFreshness`] envelope. The handler owns the
//! conversion between the `zinder_core::TransactionPublicFacts` shape and
//! its proto mirror; the source-side parser is the single source of truth
//! for everything else.

use std::collections::HashMap;

use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, ConsensusBranchId, LockTime as CoreLockTime, NetworkUpgradeActivations,
    TransactionId, TransactionPublicFacts as CoreFacts,
    TransactionVersion as CoreTransactionVersion, TransparentOutPoint, TransparentOutputFact,
    wire::{
        decode_rpc_transaction_id_hex, encode_branch_id_hex, encode_rpc_auth_digest_hex,
        encode_rpc_transaction_id_hex, encode_rpc_wtxid_hex,
    },
};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_DETAIL_V4;
use zinder_proto::wire::encode_privacy_shape;

use zinder_materialized_views::{
    MaterializedViewStore, TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TransactionFeesConsumer,
    TransparentOutpointSpendConsumer,
};
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
    transparent_spend_message,
};

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use super::intrinsic_value_balances::resolve_transaction_intrinsic_value_balances;
use super::require_matching_chain_epoch;
use super::transparent_input::encode_unresolved_transparent_inputs;

/// Read backends the `TransactionDetail` handler needs from the adapter.
///
/// Bundled into one struct so the handler signature stays under the
/// workspace's clippy `too-many-arguments` threshold and so adding a new
/// shared dependency does not ripple through every call site.
pub(crate) struct TransactionDetailContext<'context> {
    pub(crate) chain_store: Option<&'context SecondaryChainStore>,
    pub(crate) materialized_view_store: Option<&'context MaterializedViewStore>,
    pub(crate) network: zinder_core::Network,
    pub(crate) network_upgrade_activations: &'context NetworkUpgradeActivations,
    pub(crate) upstream_observation_cache: &'context UpstreamObservationCache,
    pub(crate) include_transaction_fees: bool,
    pub(crate) include_intrinsic_value_balances: bool,
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
        network_upgrade_activations,
        upstream_observation_cache,
        include_transaction_fees,
        include_intrinsic_value_balances,
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
    let canonical_reader = if include_intrinsic_value_balances {
        intrinsic_reader_for_location(chain_store, &chain_epoch, &location)?
    } else {
        None
    };

    let transaction =
        resolve_facts_and_location(network_upgrade_activations, transaction_id, location)?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            materialized_view_store,
            EXPLORER_TRANSACTION_DETAIL_V4,
            Some(chain_epoch.clone()),
            0,
        )?,
    )
    .await;

    let fees = if include_transaction_fees {
        resolve_fee_record(materialized_view_store, &transaction)?
    } else {
        None
    };
    let transparent_rows = resolve_transparent_rows(
        &transaction,
        &TransparentRowsContext {
            chain_epoch: &chain_epoch,
            fees: fees.as_ref(),
            materialized_view_store,
        },
    )?;
    let intrinsic_value_balances = if include_intrinsic_value_balances {
        resolve_detail_intrinsic_value_balances(canonical_reader.as_ref(), network, transaction_id)?
    } else {
        None
    };
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
    transient_fact_set: Option<zinder_source::TransactionPublicFactSet>,
}

struct ResolvedTransparentRows {
    inputs: Vec<zinder_proto::v1::explorer::TransparentInput>,
    outputs: Vec<WireTransparentOutput>,
}

struct TransparentRowsContext<'context> {
    chain_epoch: &'context wallet::ChainEpoch,
    fees: Option<&'context TransactionFeesRecord>,
    materialized_view_store: Option<&'context MaterializedViewStore>,
}

fn resolve_detail_intrinsic_value_balances(
    canonical_reader: Option<&ChainEpochReader<'_>>,
    network: zinder_core::Network,
    transaction_id: TransactionId,
) -> Result<Option<zinder_proto::v1::explorer::TransactionIntrinsicValueBalances>, Status> {
    let Some(reader) = canonical_reader else {
        return Ok(None);
    };
    let Some(transaction) = reader
        .transaction_facts_by_id(transaction_id)
        .map_err(|error| status_from_store_error(&error))?
    else {
        return Ok(None);
    };
    Ok(resolve_transaction_intrinsic_value_balances(
        reader,
        network,
        &[(transaction_id, transaction.location)],
    )?
    .remove(&transaction_id))
}

fn resolve_transparent_rows(
    transaction: &ResolvedTransactionDetail,
    context: &TransparentRowsContext<'_>,
) -> Result<ResolvedTransparentRows, Status> {
    let TransparentRowsContext {
        chain_epoch,
        fees,
        materialized_view_store,
    } = context;
    let inputs = transaction
        .transient_fact_set
        .as_ref()
        .map_or_else(Vec::new, |fact_set| {
            encode_unresolved_transparent_inputs(&fact_set.transparent_inputs, *fees)
        });
    let output_spends =
        resolve_transparent_output_spends(chain_epoch, transaction, *materialized_view_store)?;
    let outputs = transaction
        .transient_fact_set
        .as_ref()
        .map_or_else(Vec::new, |fact_set| {
            encode_transparent_output_facts(
                fact_set.public_facts.transaction_id,
                &fact_set.transparent_outputs,
                &output_spends,
            )
        });
    Ok(ResolvedTransparentRows { inputs, outputs })
}

/// Resolves the parsed public facts and the wire location for one transaction.
///
/// The `location` oneof is carried through verbatim so the explorer detail
/// returns the same `{ mined, in_mempool }` shape the wallet plane answered
/// with. Facts come from the raw bytes retained by the wallet endpoint for
/// both mined and mempool transactions.
fn resolve_facts_and_location(
    network_upgrade_activations: &NetworkUpgradeActivations,
    transaction_id: zinder_core::TransactionId,
    location: wire_location::Location,
) -> Result<ResolvedTransactionDetail, Status> {
    let (facts, inner, transient_fact_set) = match location {
        wire_location::Location::Mined(mined) => {
            let branch_id = mined_consensus_branch_id(&mined)?;
            let raw_transaction_bytes =
                mined.raw_transaction_bytes.as_deref().ok_or_else(|| {
                    ExplorerError::not_materialized(format!(
                        "raw transaction bytes are not available for {transaction_id:?}"
                    ))
                })?;
            let fact_set = zinder_source::parse_transaction_public_fact_set(
                raw_transaction_bytes,
                Some(BlockHeight::new(
                    mined
                        .location
                        .as_ref()
                        .ok_or_else(|| {
                            ExplorerError::internal("MinedTransaction missing block location")
                        })?
                        .block_height,
                )),
                network_upgrade_activations,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
            if fact_set.public_facts.transaction_id != transaction_id {
                return Err(ExplorerError::internal(
                    "WalletQuery.Transaction mined payload transaction id mismatch",
                )
                .into());
            }
            let mut facts = fact_set.public_facts.clone();
            facts.consensus_branch_id = Some(branch_id);
            (facts, wire_location::Location::Mined(mined), Some(fact_set))
        }
        wire_location::Location::InMempool(mempool) => {
            let fact_set = zinder_source::parse_transaction_public_fact_set(
                &mempool.raw_transaction_bytes,
                None,
                network_upgrade_activations,
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
                Some(fact_set),
            )
        }
    };
    Ok(ResolvedTransactionDetail {
        facts,
        location: WireTransactionLocation {
            location: Some(inner),
        },
        transient_fact_set,
    })
}

fn resolve_fee_record(
    materialized_view_store: Option<&MaterializedViewStore>,
    transaction: &ResolvedTransactionDetail,
) -> Result<Option<TransactionFeesRecord>, Status> {
    if transaction.facts.is_coinbase {
        return Ok(None);
    }
    let Some(materialized_view_store) = materialized_view_store else {
        return Ok(None);
    };

    Ok(TransactionFeesConsumer::read_fees_record(
        materialized_view_store,
        transaction.facts.transaction_id,
        transaction.facts.privacy_shape,
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?)
}

fn intrinsic_reader_for_location<'store>(
    chain_store: Option<&'store SecondaryChainStore>,
    chain_epoch: &wallet::ChainEpoch,
    location: &wire_location::Location,
) -> Result<Option<ChainEpochReader<'store>>, Status> {
    if !matches!(location, wire_location::Location::Mined(_)) {
        return Ok(None);
    }
    let Some(store) = chain_store else {
        return Ok(None);
    };
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

fn resolve_transparent_output_spends(
    chain_epoch: &wallet::ChainEpoch,
    transaction: &ResolvedTransactionDetail,
    materialized_view_store: Option<&MaterializedViewStore>,
) -> Result<HashMap<TransparentOutPoint, wallet::TransparentSpend>, Status> {
    if matches!(
        transaction.location.location,
        Some(wire_location::Location::InMempool(_))
    ) {
        return Ok(HashMap::new());
    }
    let transparent_outputs = transaction
        .transient_fact_set
        .as_ref()
        .map(|fact_set| fact_set.transparent_outputs.as_slice())
        .unwrap_or_default();
    let requested_outpoints: Vec<TransparentOutPoint> = transparent_outputs
        .iter()
        .map(|output| {
            TransparentOutPoint::new(transaction.facts.transaction_id, output.output_index)
        })
        .collect();
    if requested_outpoints.is_empty() {
        return Ok(HashMap::new());
    }
    let materialized_view_store = materialized_view_store.ok_or_else(|| {
        ExplorerError::dependency_not_configured(
            "TransactionDetail requires the explorer materialized-view store",
        )
    })?;
    resolve_materialized_transparent_output_spends(
        materialized_view_store,
        chain_epoch,
        transaction,
        &requested_outpoints,
    )
}

fn resolve_materialized_transparent_output_spends(
    materialized_view_store: &MaterializedViewStore,
    chain_epoch: &wallet::ChainEpoch,
    transaction: &ResolvedTransactionDetail,
    requested_outpoints: &[TransparentOutPoint],
) -> Result<HashMap<TransparentOutPoint, wallet::TransparentSpend>, Status> {
    materialized_view_store
        .try_catch_up()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let expected_epoch = chain_epoch_from_message(chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mined_height = transaction
        .location
        .location
        .as_ref()
        .and_then(|location| match location {
            wire_location::Location::Mined(mined) => mined
                .location
                .as_ref()
                .map(|location| location.block_height),
            wire_location::Location::InMempool(_) => None,
        })
        .map(BlockHeight::new)
        .ok_or_else(|| ExplorerError::internal("MinedTransaction missing block location"))?;
    let spends = {
        let snapshot = materialized_view_store.read_snapshot();
        let state = snapshot
            .consumer_state(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)
            .map_err(|error| ExplorerError::internal(error.to_string()))?
            .ok_or_else(|| {
                ExplorerError::not_materialized(
                    "transparent outpoint spend materialized-view state is unavailable",
                )
            })?;
        let state_matches_epoch = state.chain_epoch_id == expected_epoch.id
            && state.tip_height == expected_epoch.visible_tip_height
            && state.tip_hash == expected_epoch.visible_tip_hash;
        let coverage_is_complete = state.coverage.is_some_and(|coverage| {
            coverage.complete_from_height <= mined_height
                && coverage.complete_through_height >= expected_epoch.visible_tip_height
                && coverage.complete_through_hash == expected_epoch.visible_tip_hash
        });
        if !state_matches_epoch || !coverage_is_complete {
            return Err(ExplorerError::not_materialized(
                "transparent outpoint spend materialized view does not cover the requested chain epoch",
            )
            .into());
        }
        let spends = TransparentOutpointSpendConsumer::read_spends_by_outpoints_snapshot(
            &snapshot,
            requested_outpoints,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
        drop(snapshot);
        spends
    };
    Ok(spends
        .into_iter()
        .map(|(outpoint, spend)| (outpoint, transparent_spend_message(&spend)))
        .collect())
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
        ChainTipMetadata, Network, TransactionId, UnixTimestampMillis,
    };
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
            &activations,
            transaction_id,
            wire_location::Location::InMempool(wallet::MempoolEntry {
                raw_transaction_bytes: payload_bytes,
                first_seen_unix_millis: 1_700_000_000_000,
                ..Default::default()
            }),
        )?;
        let fact_set = resolved
            .transient_fact_set
            .ok_or_else(|| eyre::eyre!("mempool fact set missing"))?;
        let inputs = encode_unresolved_transparent_inputs(&fact_set.transparent_inputs, None);
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

    #[test]
    fn mined_detail_parses_wallet_bytes_without_legacy_canonical_store() -> eyre::Result<()> {
        let payload_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let parsed = zinder_source::parse_transaction_public_fact_set(
            &payload_bytes,
            Some(BlockHeight::new(1)),
            &activations,
        )?;
        let transaction_id = parsed.public_facts.transaction_id;

        let resolved = resolve_facts_and_location(
            &activations,
            transaction_id,
            wire_location::Location::Mined(wallet::MinedTransaction {
                location: Some(wallet::MinedBlockLocation {
                    transaction_id: encode_rpc_transaction_id_hex(transaction_id),
                    block_height: 1,
                    block_hash: "00".repeat(32),
                    tx_index_in_block: 0,
                }),
                chain_context: Some(wallet::MinedTransactionChainContext {
                    consensus_branch_id: 0xAABB_CCDD,
                    block_time: 1_700_000_000,
                    confirmations: 1,
                }),
                raw_transaction_bytes: Some(payload_bytes),
            }),
        )?;

        assert!(resolved.transient_fact_set.is_some());
        assert_eq!(resolved.facts.transaction_id, transaction_id);
        assert_eq!(
            resolved.facts.consensus_branch_id,
            Some(ConsensusBranchId::new(0xAABB_CCDD))
        );
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
    fn absent_fee_projection_remains_unknown() {
        let transaction_id = TransactionId::from_bytes([9; 32]);
        let resolved = ResolvedTransactionDetail {
            facts: synthetic_transaction_public_facts(transaction_id, 64),
            location: WireTransactionLocation::default(),
            transient_fact_set: None,
        };
        let outcome = resolve_fee_record(None, &resolved);

        assert!(matches!(outcome, Ok(None)));
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
