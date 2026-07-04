//! `ExplorerQuery.TransactionDetail` handler.
//!
//! Reads one transaction through `WalletQuery.Transaction` and surfaces the
//! status location alongside
//! the cross-cutting [`ExplorerFreshness`] envelope. The handler owns the
//! conversion between the `zinder_core::TransactionPublicFacts` shape and
//! its proto mirror; the source-side parser is the single source of truth
//! for everything else.

use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, ConsensusBranchId, LockTime as CoreLockTime, NetworkUpgradeActivations,
    TransactionPublicFacts as CoreFacts, TransactionVersion as CoreTransactionVersion,
    wire::{
        decode_rpc_transaction_id_hex, encode_branch_id_hex, encode_rpc_auth_digest_hex,
        encode_rpc_transaction_id_hex, encode_rpc_wtxid_hex,
    },
};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_DETAIL_V1;
use zinder_proto::wire::encode_privacy_shape;

use zinder_derive::{DeriveStore, TransactionFeesConsumer};
use zinder_proto::v1::{
    explorer::{
        LockTime as WireLockTime, LockTimeUnlocked, TransactionComponentCounts,
        TransactionDetailRequest, TransactionDetailResponse, TransactionPublicFacts as WireFacts,
        TransactionVersion as WireVersion, TransactionVersionKind, lock_time as wire_lock_time,
    },
    wallet::{
        self, TransactionLocation as WireTransactionLocation,
        transaction_location as wire_location, wallet_query_client::WalletQueryClient,
    },
};
use zinder_runtime::AuthenticatedChannel;
use zinder_store::{SecondaryChainStore, chain_epoch_from_message, status_from_store_error};

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

/// Read backends the `TransactionDetail` handler needs from the adapter.
///
/// Bundled into one struct so the handler signature stays under the
/// workspace's clippy `too-many-arguments` threshold and so adding a new
/// shared dependency does not ripple through every call site.
pub(crate) struct TransactionDetailContext<'context> {
    pub(crate) chain_store: Option<&'context SecondaryChainStore>,
    pub(crate) derive_store: Option<&'context DeriveStore>,
    pub(crate) network: zinder_core::Network,
    pub(crate) upstream_observation_cache: &'context UpstreamObservationCache,
}

/// Executes one `ExplorerQuery.TransactionDetail` request.
pub(crate) async fn handle_transaction_detail(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    context: TransactionDetailContext<'_>,
    request: Request<TransactionDetailRequest>,
) -> Result<Response<TransactionDetailResponse>, Status> {
    let TransactionDetailContext {
        chain_store,
        derive_store,
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

    let (core_facts, wire_location) = resolve_facts_and_location(
        chain_store,
        network,
        chain_epoch.clone(),
        transaction_id,
        location,
    )?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            derive_store,
            EXPLORER_TRANSACTION_DETAIL_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;

    let fees = derive_store
        .and_then(|store| TransactionFeesConsumer::read_fees_record(store, transaction_id).ok())
        .flatten();
    let (paid_fee_zat, prevout_resolution_status, transparent_inputs) = match fees {
        Some(record) => (
            record.paid_fee_zat,
            record.prevout_resolution_status,
            record.transparent_inputs,
        ),
        None => (None, 0, Vec::new()),
    };
    Ok(Response::new(TransactionDetailResponse {
        freshness: Some(freshness),
        facts: Some(encode_public_facts(&core_facts)),
        location: Some(wire_location),
        paid_fee_zat,
        prevout_resolution_status,
        transparent_inputs,
    }))
}

/// Resolves the parsed public facts and the wire location for one transaction.
///
/// The `location` oneof is carried through verbatim so the explorer detail
/// returns the same `{ mined, in_mempool, conflicting }` shape the wallet
/// plane answered with. Facts come from the canonical store for mined,
/// from the raw bytes for mempool, and from the minimal txid-only shape for
/// conflicting (which carries no transaction bytes).
fn resolve_facts_and_location(
    chain_store: Option<&SecondaryChainStore>,
    network: zinder_core::Network,
    chain_epoch: wallet::ChainEpoch,
    transaction_id: zinder_core::TransactionId,
    location: wire_location::Location,
) -> Result<(CoreFacts, WireTransactionLocation), Status> {
    let (core_facts, inner) = match location {
        wire_location::Location::Mined(mined) => {
            let branch_id = mined_consensus_branch_id(&mined)?;
            let mut facts = read_mined_public_facts(chain_store, chain_epoch, transaction_id)?;
            facts.consensus_branch_id = Some(branch_id);
            (facts, wire_location::Location::Mined(mined))
        }
        wire_location::Location::InMempool(mempool) => {
            let activations = NetworkUpgradeActivations::empty(network);
            let facts = zinder_source::parse_transaction_public_facts(
                &mempool.payload_bytes,
                None,
                &activations,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
            (facts, wire_location::Location::InMempool(mempool))
        }
        wire_location::Location::Conflicting(conflicting) => (
            conflicting_public_facts(transaction_id),
            wire_location::Location::Conflicting(conflicting),
        ),
    };
    Ok((
        core_facts,
        WireTransactionLocation {
            location: Some(inner),
        },
    ))
}

fn read_mined_public_facts(
    chain_store: Option<&SecondaryChainStore>,
    chain_epoch: wallet::ChainEpoch,
    transaction_id: zinder_core::TransactionId,
) -> Result<CoreFacts, Status> {
    let store = chain_store.ok_or_else(|| {
        ExplorerError::dependency_not_configured(
            "TransactionDetail requires the canonical fact store; configure --storage-path",
        )
    })?;
    store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(chain_epoch)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    let artifact = reader
        .transaction_facts_by_id(transaction_id)
        .map_err(|error| status_from_store_error(&error))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "transaction facts are not available for {transaction_id:?}"
            ))
        })?;
    Ok(artifact.public_facts)
}

fn mined_consensus_branch_id(
    mined: &wallet::MinedTransaction,
) -> Result<ConsensusBranchId, Status> {
    let details = mined
        .details
        .as_ref()
        .ok_or_else(|| ExplorerError::internal("MinedTransaction missing details"))?;
    Ok(ConsensusBranchId::new(details.consensus_branch_id))
}

/// Builds the minimal public facts for a conflicting-chain transaction.
///
/// A conflicting-chain status carries no transaction bytes and is not in the
/// canonical fact store, so the only fact the explorer can assert is the
/// requested transaction id. The remaining fields surface the
/// not-decodable shape: `Unsupported` version, `Unclassified` privacy.
fn conflicting_public_facts(transaction_id: zinder_core::TransactionId) -> CoreFacts {
    CoreFacts {
        transaction_id,
        auth_digest: None,
        wtxid: None,
        version: CoreTransactionVersion::Unsupported {
            effective_version: 0,
            version_group_id: None,
        },
        consensus_branch_id: None,
        lock_time: CoreLockTime::Unlocked,
        expiry_height: None,
        size_bytes: 0,
        counts: zinder_core::TransactionComponentCounts::EMPTY,
        privacy_shape: zinder_core::PrivacyShape::Unclassified,
        is_coinbase: false,
        unsupported_sections: Vec::new(),
    }
}

fn encode_public_facts(facts: &CoreFacts) -> WireFacts {
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
        counts: Some(TransactionComponentCounts {
            transparent_input_count: facts.counts.transparent_input_count,
            transparent_output_count: facts.counts.transparent_output_count,
            sapling_spend_count: facts.counts.sapling_spend_count,
            sapling_output_count: facts.counts.sapling_output_count,
            orchard_action_count: facts.counts.orchard_action_count,
            ironwood_action_count: facts.counts.ironwood_action_count,
            sprout_joinsplit_count: facts.counts.sprout_joinsplit_count,
        }),
        privacy_shape: encode_privacy_shape(facts.privacy_shape) as i32,
        is_coinbase: facts.is_coinbase,
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
