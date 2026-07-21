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
    NetworkUpgradeActivations, PrivacyShape, TransactionId, TransactionPublicFacts as CoreFacts,
    TransactionVersion as CoreTransactionVersion, TransparentInputFact, TransparentOutPoint,
    TransparentOutputFact,
    wire::{
        decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex, encode_branch_id_hex,
        encode_rpc_auth_digest_hex, encode_rpc_transaction_id_hex, encode_rpc_wtxid_hex,
    },
};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_DETAIL_V4;
use zinder_proto::wire::encode_privacy_shape;

use zinder_materialized_views::MaterializedViewStore;
use zinder_proto::v1::{
    explorer::{
        LockTime as WireLockTime, LockTimeUnlocked, PrevoutResolutionStatus,
        TransactionComponentCounts, TransactionDetailRequest, TransactionDetailResponse,
        TransactionIntrinsicValueBalances as WireIntrinsicValueBalances,
        TransactionPublicFacts as WireFacts, TransactionVersion as WireVersion,
        TransactionVersionKind, TransparentInput as WireTransparentInput,
        TransparentOutput as WireTransparentOutput, lock_time as wire_lock_time,
    },
    wallet::{
        self, TransactionLocation as WireTransactionLocation,
        transaction_location as wire_location, wallet_query_client::WalletQueryClient,
    },
};
use zinder_runtime::AuthenticatedChannel;
use zinder_store::chain_epoch_from_message;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use super::require_matching_chain_epoch;
use super::transparent_input::encode_unresolved_transparent_inputs;

/// Read backends the `TransactionDetail` handler needs from the adapter.
///
/// Bundled into one struct so the handler signature stays under the
/// workspace's clippy `too-many-arguments` threshold and so adding a new
/// shared dependency does not ripple through every call site.
pub(crate) struct TransactionDetailContext<'context> {
    pub(crate) materialized_view_store: Option<&'context MaterializedViewStore>,
    pub(crate) network_upgrade_activations: &'context NetworkUpgradeActivations,
    pub(crate) upstream_observation_cache: &'context UpstreamObservationCache,
}

/// Executes one `ExplorerQuery.TransactionDetail` request.
pub(crate) async fn query_transaction_detail(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    context: TransactionDetailContext<'_>,
    request: Request<TransactionDetailRequest>,
) -> Result<Response<TransactionDetailResponse>, Status> {
    let TransactionDetailContext {
        materialized_view_store,
        network_upgrade_activations,
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
    validate_response_consistency(inner.at_epoch_id, &chain_epoch, &location)?;
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

    let transparent_rows =
        resolve_transparent_rows(wallet_client, &chain_epoch, &transaction).await?;
    let intrinsic_value_balances = encode_mined_intrinsic_value_balances(&transaction);
    Ok(Response::new(TransactionDetailResponse {
        freshness: Some(freshness),
        facts: Some(encode_public_facts(&transaction.facts)),
        location: Some(transaction.location),
        paid_fee_zat: transparent_rows.paid_fee_zat,
        prevout_resolution_status: transparent_rows.prevout_resolution_status,
        transparent_inputs: transparent_rows.inputs,
        transparent_outputs: transparent_rows.outputs,
        intrinsic_value_balances,
    }))
}

struct ResolvedTransactionDetail {
    facts: CoreFacts,
    location: WireTransactionLocation,
    fact_set: zinder_source::TransactionPublicFactSet,
}

struct ResolvedTransparentRows {
    inputs: Vec<WireTransparentInput>,
    outputs: Vec<WireTransparentOutput>,
    paid_fee_zat: Option<u64>,
    prevout_resolution_status: i32,
}

fn validate_response_consistency(
    requested_epoch_id: Option<u64>,
    chain_epoch: &wallet::ChainEpoch,
    location: &wire_location::Location,
) -> Result<(), Status> {
    if requested_epoch_id.is_some_and(|epoch_id| epoch_id != chain_epoch.chain_epoch_id) {
        return Err(ExplorerError::internal(
            "WalletQuery.Transaction response does not match the requested chain epoch",
        )
        .into());
    }
    let core_epoch = chain_epoch_from_message(chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    if let wire_location::Location::Mined(mined) = location {
        let mined_location = mined.location.as_ref().ok_or_else(|| {
            ExplorerError::internal("WalletQuery.Transaction mined response missing location")
        })?;
        if mined_location.block_height > core_epoch.visible_tip_height.value() {
            return Err(ExplorerError::internal(
                "WalletQuery.Transaction mined location is above the visible tip",
            )
            .into());
        }
        let mined_block_hash = decode_rpc_block_hash_hex(&mined_location.block_hash)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if mined_location.block_height == core_epoch.visible_tip_height.value()
            && mined_block_hash != core_epoch.visible_tip_hash
        {
            return Err(ExplorerError::internal(
                "WalletQuery.Transaction mined location visible-tip hash mismatch",
            )
            .into());
        }
        if mined_location.block_height == core_epoch.settled_tip_height.value()
            && mined_block_hash != core_epoch.settled_tip_hash
        {
            return Err(ExplorerError::internal(
                "WalletQuery.Transaction mined location settled-tip hash mismatch",
            )
            .into());
        }
        let chain_context = mined.chain_context.as_ref().ok_or_else(|| {
            ExplorerError::internal("WalletQuery.Transaction mined response missing chain context")
        })?;
        let expected_confirmations = core_epoch
            .visible_tip_height
            .value()
            .checked_sub(mined_location.block_height)
            .and_then(|height_delta| height_delta.checked_add(1))
            .ok_or_else(|| {
                ExplorerError::internal(
                    "WalletQuery.Transaction mined confirmations exceed the wire range",
                )
            })?;
        if chain_context.confirmations != expected_confirmations {
            return Err(ExplorerError::internal(
                "WalletQuery.Transaction mined confirmations do not match the visible tip",
            )
            .into());
        }
    }
    Ok(())
}

async fn resolve_transparent_rows(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    chain_epoch: &wallet::ChainEpoch,
    transaction: &ResolvedTransactionDetail,
) -> Result<ResolvedTransparentRows, Status> {
    let is_mined = matches!(
        transaction.location.location,
        Some(wire_location::Location::Mined(_))
    );
    if !is_mined {
        return Ok(ResolvedTransparentRows {
            inputs: encode_unresolved_transparent_inputs(&transaction.fact_set.transparent_inputs),
            outputs: encode_transparent_output_facts(
                transaction.fact_set.public_facts.transaction_id,
                &transaction.fact_set.transparent_outputs,
                &HashMap::new(),
            ),
            paid_fee_zat: None,
            prevout_resolution_status: PrevoutResolutionStatus::Unspecified as i32,
        });
    }

    let inputs = resolve_mined_transparent_inputs(
        wallet_client,
        chain_epoch,
        &transaction.fact_set.transparent_inputs,
    )
    .await?;
    let output_spends = resolve_transparent_output_spends(
        wallet_client,
        chain_epoch,
        transaction.fact_set.public_facts.transaction_id,
        &transaction.fact_set.transparent_outputs,
    )
    .await?;
    let outputs = encode_transparent_output_facts(
        transaction.fact_set.public_facts.transaction_id,
        &transaction.fact_set.transparent_outputs,
        &output_spends,
    );
    let all_inputs_resolved = inputs.iter().all(|input| input.value_zat.is_some());
    let prevout_resolution_status = if all_inputs_resolved {
        PrevoutResolutionStatus::Resolved
    } else {
        PrevoutResolutionStatus::Partial
    };
    let paid_fee_zat = (all_inputs_resolved
        && transaction.fact_set.public_facts.privacy_shape == PrivacyShape::TransparentOnly)
        .then(|| paid_fee_from_resolved_inputs(&inputs, &transaction.fact_set.transparent_outputs))
        .flatten();
    Ok(ResolvedTransparentRows {
        inputs,
        outputs,
        paid_fee_zat,
        prevout_resolution_status: prevout_resolution_status as i32,
    })
}

/// Resolves the parsed public facts and the wire location for one transaction.
///
/// The `location` oneof is carried through verbatim so the explorer detail
/// returns the same `{ mined, in_mempool }` shape the wallet plane answered
/// with. Facts come from the canonical store for mined transactions and from
/// raw bytes for mempool transactions.
fn resolve_facts_and_location(
    activations: &NetworkUpgradeActivations,
    transaction_id: zinder_core::TransactionId,
    location: wire_location::Location,
) -> Result<ResolvedTransactionDetail, Status> {
    let (facts, inner, fact_set) = match location {
        wire_location::Location::Mined(mined) => {
            let mined_location = mined.location.as_ref().ok_or_else(|| {
                ExplorerError::internal("WalletQuery.Transaction mined response missing location")
            })?;
            let mined_height = BlockHeight::new(mined_location.block_height);
            let branch_id = mined_consensus_branch_id(&mined)?;
            let location_transaction_id =
                decode_rpc_transaction_id_hex(&mined_location.transaction_id)
                    .map_err(|error| ExplorerError::internal(error.to_string()))?;
            if location_transaction_id != transaction_id {
                return Err(ExplorerError::internal(
                    "WalletQuery.Transaction mined location transaction id mismatch",
                )
                .into());
            }
            decode_rpc_block_hash_hex(&mined_location.block_hash)
                .map_err(|error| ExplorerError::internal(error.to_string()))?;
            let raw_transaction_bytes =
                mined.raw_transaction_bytes.as_deref().ok_or_else(|| {
                    ExplorerError::not_materialized(
                        "WalletQuery.Transaction mined response omitted retained transaction bytes",
                    )
                })?;
            let fact_set = zinder_source::parse_transaction_public_fact_set(
                raw_transaction_bytes,
                Some(mined_height),
                activations,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
            if fact_set.public_facts.transaction_id != transaction_id {
                return Err(ExplorerError::internal(
                    "WalletQuery.Transaction mined raw transaction id mismatch",
                )
                .into());
            }
            require_matching_parsed_consensus_branch(
                fact_set.public_facts.consensus_branch_id,
                branch_id,
                fact_set.public_facts.version,
                activations,
            )?;
            let mut facts = fact_set.public_facts.clone();
            reconcile_mined_consensus_branch(&mut facts, branch_id, activations);
            (facts, wire_location::Location::Mined(mined), fact_set)
        }
        wire_location::Location::InMempool(mempool) => {
            let fact_set = zinder_source::parse_transaction_public_fact_set(
                &mempool.raw_transaction_bytes,
                None,
                activations,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
            if fact_set.public_facts.transaction_id != transaction_id {
                return Err(ExplorerError::internal(
                    "WalletQuery.Transaction mempool raw transaction id mismatch",
                )
                .into());
            }
            (
                fact_set.public_facts.clone(),
                wire_location::Location::InMempool(mempool),
                fact_set,
            )
        }
    };
    Ok(ResolvedTransactionDetail {
        facts,
        location: WireTransactionLocation {
            location: Some(inner),
        },
        fact_set,
    })
}

async fn resolve_mined_transparent_inputs(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    chain_epoch: &wallet::ChainEpoch,
    inputs: &[TransparentInputFact],
) -> Result<Vec<WireTransparentInput>, Status> {
    let mut resolved = Vec::with_capacity(inputs.len());
    for input_batch in inputs.chunks(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST) {
        resolved
            .extend(fetch_transparent_input_batch(wallet_client, chain_epoch, input_batch).await?);
    }
    Ok(resolved)
}

async fn fetch_transparent_input_batch(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    expected_chain_epoch: &wallet::ChainEpoch,
    inputs: &[TransparentInputFact],
) -> Result<Vec<WireTransparentInput>, Status> {
    let requested_outpoints = inputs
        .iter()
        .map(|input| wallet::OutPoint {
            transaction_id: encode_rpc_transaction_id_hex(input.spent_outpoint.transaction_id),
            output_index: input.spent_outpoint.output_index,
        })
        .collect();
    let response = wallet_client
        .transparent_outputs_by_outpoint(Request::new(
            wallet::TransparentOutputsByOutpointRequest {
                outpoints: requested_outpoints,
                at_epoch_id: Some(expected_chain_epoch.chain_epoch_id),
            },
        ))
        .await?
        .into_inner();
    require_response_chain_epoch(
        expected_chain_epoch,
        response.chain_view,
        "WalletQuery.TransparentOutputsByOutpoint",
    )?;
    if response.entries.len() != inputs.len() {
        return Err(ExplorerError::internal(
            "WalletQuery.TransparentOutputsByOutpoint did not preserve request cardinality",
        )
        .into());
    }
    inputs
        .iter()
        .zip(response.entries)
        .map(|(input, entry)| {
            let returned_outpoint = entry.outpoint.as_ref().ok_or_else(|| {
                ExplorerError::internal(
                    "WalletQuery.TransparentOutputsByOutpoint entry missing outpoint",
                )
            })?;
            let returned = TransparentOutPoint::new(
                decode_rpc_transaction_id_hex(&returned_outpoint.transaction_id)
                    .map_err(|error| ExplorerError::internal(error.to_string()))?,
                returned_outpoint.output_index,
            );
            if returned != input.spent_outpoint {
                return Err(ExplorerError::internal(
                    "WalletQuery.TransparentOutputsByOutpoint did not preserve request order",
                )
                .into());
            }
            Ok(WireTransparentInput {
                input_index: input.input_index,
                spent_outpoint: entry.outpoint,
                value_zat: entry.output.as_ref().map(|output| output.value_zat),
                script_pub_key: entry.output.map(|output| output.script_pub_key),
            })
        })
        .collect()
}

async fn resolve_transparent_output_spends(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    chain_epoch: &wallet::ChainEpoch,
    transaction_id: TransactionId,
    outputs: &[TransparentOutputFact],
) -> Result<HashMap<TransparentOutPoint, wallet::TransparentSpend>, Status> {
    let requested_outpoints: Vec<TransparentOutPoint> = outputs
        .iter()
        .map(|output| TransparentOutPoint::new(transaction_id, output.output_index))
        .collect();
    if requested_outpoints.is_empty() {
        return Ok(HashMap::new());
    }
    let requested_outpoint_set: HashSet<TransparentOutPoint> =
        requested_outpoints.iter().copied().collect();
    if requested_outpoint_set.len() != requested_outpoints.len() {
        return Err(ExplorerError::internal(
            "TransactionDetail parsed transaction contains duplicate transparent output indexes",
        )
        .into());
    }

    let mut spends = HashMap::new();
    for outpoint_batch in requested_outpoints.chunks(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST) {
        let batch_spends =
            fetch_transparent_output_spend_batch(wallet_client, chain_epoch, outpoint_batch)
                .await?;
        merge_transparent_output_spend_batch(&mut spends, outpoint_batch, batch_spends)?;
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
    require_response_chain_epoch(
        expected_chain_epoch,
        response.chain_view,
        "WalletQuery.TransparentSpendsByOutpoint",
    )?;
    Ok(response.spends)
}

fn require_response_chain_epoch(
    expected_chain_epoch: &wallet::ChainEpoch,
    chain_view: Option<wallet::ChainView>,
    method: &'static str,
) -> Result<(), Status> {
    let response_chain_epoch = chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| ExplorerError::internal(format!("{method} missing chain_epoch")))?;
    let expected_core_epoch = chain_epoch_from_message(expected_chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let response_core_epoch = chain_epoch_from_message(response_chain_epoch)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    require_matching_chain_epoch(expected_core_epoch, response_core_epoch)
}

fn merge_transparent_output_spend_batch(
    spends: &mut HashMap<TransparentOutPoint, wallet::TransparentSpend>,
    requested_outpoints: &[TransparentOutPoint],
    batch_spends: Vec<wallet::TransparentSpend>,
) -> Result<(), Status> {
    let requested_outpoint_set = requested_outpoints.iter().copied().collect();
    insert_transparent_output_spends(spends, &requested_outpoint_set, batch_spends)
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

fn paid_fee_from_resolved_inputs(
    inputs: &[WireTransparentInput],
    outputs: &[TransparentOutputFact],
) -> Option<u64> {
    let total_input_zat = inputs.iter().try_fold(0_i128, |sum, input| {
        input
            .value_zat
            .map(|value_zat| sum.saturating_add(i128::from(value_zat)))
    })?;
    let total_output_zat = outputs.iter().fold(0_i128, |sum, output| {
        sum.saturating_add(i128::from(output.value_zat))
    });
    total_input_zat
        .checked_sub(total_output_zat)
        .filter(|fee| *fee >= 0)
        .and_then(|fee| u64::try_from(fee).ok())
}

const fn encode_intrinsic_value_balances(
    balances: &zinder_core::TransactionIntrinsicValueBalances,
) -> WireIntrinsicValueBalances {
    WireIntrinsicValueBalances {
        sprout_zat: balances.sprout_zat,
        sapling_zat: balances.sapling_zat,
        orchard_zat: balances.orchard_zat,
        ironwood_zat: balances.ironwood_zat,
    }
}

fn encode_mined_intrinsic_value_balances(
    transaction: &ResolvedTransactionDetail,
) -> Option<WireIntrinsicValueBalances> {
    matches!(
        transaction.location.location,
        Some(wire_location::Location::Mined(_))
    )
    .then(|| encode_intrinsic_value_balances(&transaction.fact_set.intrinsic_value_balances))
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

fn require_matching_parsed_consensus_branch(
    parsed_branch_id: Option<ConsensusBranchId>,
    verified_branch_id: ConsensusBranchId,
    version: CoreTransactionVersion,
    activations: &NetworkUpgradeActivations,
) -> Result<(), Status> {
    if parsed_consensus_branch_is_independent(version, activations)
        && parsed_branch_id.is_some_and(|branch_id| branch_id != verified_branch_id)
    {
        return Err(ExplorerError::internal(
            "WalletQuery.Transaction mined raw transaction consensus branch mismatch",
        )
        .into());
    }
    Ok(())
}

fn parsed_consensus_branch_is_independent(
    version: CoreTransactionVersion,
    activations: &NetworkUpgradeActivations,
) -> bool {
    !matches!(
        version,
        CoreTransactionVersion::V3 | CoreTransactionVersion::V4
    ) || !activations.activations().is_empty()
}

fn reconcile_mined_consensus_branch(
    facts: &mut CoreFacts,
    verified_branch_id: ConsensusBranchId,
    activations: &NetworkUpgradeActivations,
) {
    match facts.version {
        CoreTransactionVersion::V1 | CoreTransactionVersion::V2 => {}
        CoreTransactionVersion::V3 | CoreTransactionVersion::V4
            if activations.activations().is_empty() =>
        {
            facts.consensus_branch_id = Some(verified_branch_id);
        }
        CoreTransactionVersion::V3
        | CoreTransactionVersion::V4
        | CoreTransactionVersion::V5
        | CoreTransactionVersion::V6
        | CoreTransactionVersion::Unsupported { .. } => {
            facts.consensus_branch_id.get_or_insert(verified_branch_id);
        }
    }
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
        ChainTipMetadata, Network, TransactionFactsArtifact, TransactionId, TransactionLocation,
        TransparentInputFact, TransparentOutPoint, UnixTimestampMillis,
    };
    use zinder_proto::v1::explorer::{TransactionFeesRecord, TransparentInputValueRecord};
    use zinder_testkit::synthetic_transaction_public_facts;

    use super::super::transparent_input::encode_mined_transparent_inputs;
    use super::*;

    #[test]
    fn mempool_detail_preserves_ordered_transparent_rows_from_raw_transaction_bytes()
    -> eyre::Result<()> {
        let raw_transaction_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let parsed = zinder_source::parse_transaction_public_fact_set(
            &raw_transaction_bytes,
            None,
            &activations,
        )?;
        let transaction_id = parsed.public_facts.transaction_id;

        let resolved = resolve_facts_and_location(
            &activations,
            transaction_id,
            wire_location::Location::InMempool(wallet::MempoolEntry {
                raw_transaction_bytes,
                compact_transaction_data: Some(wallet::CompactTransactionData::default()),
                first_seen_unix_millis: 1_700_000_000_999,
                ..Default::default()
            }),
        )?;
        let fact_set = resolved.fact_set;
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

    #[test]
    fn mempool_detail_omits_mined_only_intrinsic_value_balances() -> eyre::Result<()> {
        let raw_transaction_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let transaction_id = zinder_source::parse_transaction_public_fact_set(
            &raw_transaction_bytes,
            None,
            &activations,
        )?
        .public_facts
        .transaction_id;
        let resolved = resolve_facts_and_location(
            &activations,
            transaction_id,
            wire_location::Location::InMempool(wallet::MempoolEntry {
                raw_transaction_bytes,
                compact_transaction_data: Some(wallet::CompactTransactionData::default()),
                first_seen_unix_millis: 1_700_000_000_999,
                ..Default::default()
            }),
        )?;

        assert!(encode_mined_intrinsic_value_balances(&resolved).is_none());
        Ok(())
    }

    #[test]
    fn mined_detail_parses_public_facts_from_wallet_bytes_without_canonical_store()
    -> eyre::Result<()> {
        let raw_transaction_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let parsed = zinder_source::parse_transaction_public_fact_set(
            &raw_transaction_bytes,
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
                    block_hash: "11".repeat(32),
                    tx_index_in_block: 0,
                }),
                chain_context: Some(wallet::MinedTransactionChainContext {
                    consensus_branch_id: 0,
                    block_time: 1_700_000_000,
                    confirmations: 1,
                }),
                raw_transaction_bytes: Some(raw_transaction_bytes),
            }),
        )?;

        assert_eq!(resolved.facts.transaction_id, transaction_id);
        assert_eq!(
            resolved.fact_set.public_facts.transaction_id,
            transaction_id
        );
        assert_eq!(resolved.facts.consensus_branch_id, None);
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_missing_wallet_transaction_bytes() -> eyre::Result<()> {
        let raw_transaction_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let transaction_id = zinder_source::parse_transaction_public_fact_set(
            &raw_transaction_bytes,
            Some(BlockHeight::new(1)),
            &activations,
        )?
        .public_facts
        .transaction_id;

        let outcome = resolve_facts_and_location(
            &activations,
            transaction_id,
            wire_location::Location::Mined(wallet::MinedTransaction {
                location: Some(wallet::MinedBlockLocation {
                    transaction_id: encode_rpc_transaction_id_hex(transaction_id),
                    block_height: 1,
                    block_hash: "11".repeat(32),
                    tx_index_in_block: 0,
                }),
                chain_context: Some(wallet::MinedTransactionChainContext::default()),
                raw_transaction_bytes: None,
            }),
        );

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("missing mined transaction bytes were accepted"))?;
        assert!(
            status
                .message()
                .contains("omitted retained transaction bytes")
        );
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_raw_bytes_with_a_different_transaction_id() -> eyre::Result<()> {
        let raw_transaction_bytes = transparent_transaction_bytes();
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let requested_transaction_id = TransactionId::from_bytes([0xFF; 32]);

        let outcome = resolve_facts_and_location(
            &activations,
            requested_transaction_id,
            wire_location::Location::Mined(wallet::MinedTransaction {
                location: Some(wallet::MinedBlockLocation {
                    transaction_id: encode_rpc_transaction_id_hex(requested_transaction_id),
                    block_height: 1,
                    block_hash: "11".repeat(32),
                    tx_index_in_block: 0,
                }),
                chain_context: Some(wallet::MinedTransactionChainContext::default()),
                raw_transaction_bytes: Some(raw_transaction_bytes),
            }),
        );

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("mismatched mined transaction bytes were accepted"))?;
        assert!(status.message().contains("raw transaction id mismatch"));
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_a_parsed_branch_that_disagrees_with_verified_context()
    -> eyre::Result<()> {
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let outcome = require_matching_parsed_consensus_branch(
            Some(ConsensusBranchId::new(1)),
            ConsensusBranchId::new(2),
            CoreTransactionVersion::V6,
            &activations,
        );

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("contradictory parsed branch was accepted"))?;
        assert!(
            status
                .message()
                .contains("raw transaction consensus branch")
        );
        Ok(())
    }

    #[test]
    fn v4_branch_derived_from_an_empty_fallback_does_not_override_wallet_context()
    -> eyre::Result<()> {
        let activations = NetworkUpgradeActivations::empty(Network::ZcashRegtest);

        require_matching_parsed_consensus_branch(
            Some(ConsensusBranchId::PRE_OVERWINTER),
            ConsensusBranchId::new(0x76b8_09bb),
            CoreTransactionVersion::V4,
            &activations,
        )?;
        Ok(())
    }

    #[test]
    fn transaction_detail_rejects_a_wallet_epoch_other_than_the_requested_epoch() -> eyre::Result<()>
    {
        let chain_epoch = sample_chain_epoch();
        let location = sample_mined_location(9, 2);

        let outcome = validate_response_consistency(Some(8), &chain_epoch, &location);

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("mismatched requested epoch was accepted"))?;
        assert!(status.message().contains("requested chain epoch"));
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_a_location_above_the_response_visible_tip() -> eyre::Result<()> {
        let chain_epoch = sample_chain_epoch();
        let location = sample_mined_location(11, 0);

        let outcome = validate_response_consistency(Some(7), &chain_epoch, &location);

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("mined location above the visible tip was accepted"))?;
        assert!(status.message().contains("above the visible tip"));
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_confirmations_that_disagree_with_the_response_tip() -> eyre::Result<()>
    {
        let chain_epoch = sample_chain_epoch();
        let location = sample_mined_location(9, 1);

        let outcome = validate_response_consistency(Some(7), &chain_epoch, &location);

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("contradictory mined confirmations were accepted"))?;
        assert!(status.message().contains("confirmations"));
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_a_location_hash_that_disagrees_with_the_visible_tip() -> eyre::Result<()>
    {
        let chain_epoch = sample_chain_epoch();
        let location = sample_mined_location(10, 1);

        let outcome = validate_response_consistency(Some(7), &chain_epoch, &location);

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("contradictory visible-tip hash was accepted"))?;
        assert!(status.message().contains("visible-tip hash"));
        Ok(())
    }

    #[test]
    fn mined_detail_rejects_a_location_hash_that_disagrees_with_the_settled_tip() -> eyre::Result<()>
    {
        let chain_epoch = sample_chain_epoch();
        let location = sample_mined_location_with_hash(9, 2, 0x44);

        let outcome = validate_response_consistency(Some(7), &chain_epoch, &location);

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("contradictory settled-tip hash was accepted"))?;
        assert!(status.message().contains("settled-tip hash"));
        Ok(())
    }

    fn sample_chain_epoch() -> wallet::ChainEpoch {
        wallet::ChainEpoch {
            chain_epoch_id: 7,
            network_name: "zcash-regtest".to_owned(),
            artifact_schema_version: 1,
            created_at_millis: 1,
            visible_tip: Some(wallet::BlockTip {
                height: 10,
                hash: "11".repeat(32),
            }),
            settled_tip: Some(wallet::BlockTip {
                height: 9,
                hash: "22".repeat(32),
            }),
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        }
    }

    fn sample_mined_location(block_height: u32, confirmations: u32) -> wire_location::Location {
        sample_mined_location_with_hash(block_height, confirmations, 0x22)
    }

    fn sample_mined_location_with_hash(
        block_height: u32,
        confirmations: u32,
        block_hash_byte: u8,
    ) -> wire_location::Location {
        wire_location::Location::Mined(wallet::MinedTransaction {
            location: Some(wallet::MinedBlockLocation {
                transaction_id: "33".repeat(32),
                block_height,
                block_hash: format!("{block_hash_byte:02x}").repeat(32),
                tx_index_in_block: 0,
            }),
            chain_context: Some(wallet::MinedTransactionChainContext {
                consensus_branch_id: 0,
                block_time: 1_700_000_000,
                confirmations,
            }),
            raw_transaction_bytes: Some(transparent_transaction_bytes()),
        })
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

    #[test]
    fn spend_batch_rejects_an_outpoint_requested_only_in_a_later_chunk() -> eyre::Result<()> {
        let transaction_id = TransactionId::from_bytes([0x77; 32]);
        let requested_outpoints = (0..=MAX_TRANSPARENT_OUTPUTS_PER_REQUEST)
            .map(|output_index| {
                Ok(TransparentOutPoint::new(
                    transaction_id,
                    u32::try_from(output_index)?,
                ))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let injected_outpoint = requested_outpoints[MAX_TRANSPARENT_OUTPUTS_PER_REQUEST];
        let injected_spend = wallet::TransparentSpend {
            spent_outpoint: Some(wallet::OutPoint {
                transaction_id: encode_rpc_transaction_id_hex(injected_outpoint.transaction_id),
                output_index: injected_outpoint.output_index,
            }),
            ..Default::default()
        };
        let mut spends = HashMap::new();

        let outcome = merge_transparent_output_spend_batch(
            &mut spends,
            &requested_outpoints[..MAX_TRANSPARENT_OUTPUTS_PER_REQUEST],
            vec![injected_spend],
        );

        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("cross-chunk spend injection was accepted"))?;
        assert!(status.message().contains("unrequested outpoint"));
        Ok(())
    }
}
