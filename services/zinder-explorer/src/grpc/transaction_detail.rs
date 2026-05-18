//! `ExplorerQuery.TransactionDetail` handler.
//!
//! Reads one transaction through `WalletQuery.Transaction`, parses the
//! returned `payload_bytes` once via `zinder_source::parse_transaction_public_facts`,
//! and surfaces the result as a typed [`TransactionDetailResponse`] alongside
//! the cross-cutting [`ExplorerFreshness`] envelope. The handler owns the
//! conversion between the `zinder_core::TransactionPublicFacts` shape and
//! its proto mirror; the source-side parser is the single source of truth
//! for everything else.

use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, ConsensusBranchId, LockTime as CoreLockTime, NetworkUpgradeActivations,
    TransactionPublicFacts as CoreFacts, TransactionVersion as CoreTransactionVersion,
    wire::{decode_internal_transaction_id, encode_branch_id_hex, encode_internal_transaction_id},
};
use zinder_proto::capabilities::EXPLORER_TRANSACTION_DETAIL_V1;
use zinder_proto::wire::encode_privacy_shape;

use crate::consumer::transaction_fees::TransactionFeesConsumer;
use crate::store::DeriveStore;
use zinder_proto::v1::{
    explorer::{
        ExplorerFreshness, LockTime as WireLockTime, LockTimeUnlocked, MempoolLocation,
        MinedLocation, TransactionComponentCounts, TransactionDetailRequest,
        TransactionDetailResponse, TransactionLocation as WireTransactionLocation,
        TransactionPublicFacts as WireFacts, TransactionVersion as WireVersion,
        TransactionVersionKind, lock_time as wire_lock_time, transaction_location as wire_location,
    },
    wallet::{self, transaction_status_response, wallet_query_client::WalletQueryClient},
};
use zinder_runtime::AuthenticatedChannel;

/// Executes one `ExplorerQuery.TransactionDetail` request.
pub(crate) async fn handle_transaction_detail(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    derive_store: Option<&DeriveStore>,
    network: zinder_core::Network,
    request: Request<TransactionDetailRequest>,
) -> Result<Response<TransactionDetailResponse>, Status> {
    let inner = request.into_inner();
    let transaction_id = decode_internal_transaction_id(&inner.transaction_id)
        .map_err(|error| Status::invalid_argument(error.to_string()))?;

    let status_response = wallet_client
        .transaction(Request::new(wallet::TransactionRequest {
            transaction_id: encode_internal_transaction_id(transaction_id).to_vec(),
            at_epoch: inner.at_epoch,
        }))
        .await?
        .into_inner();
    let chain_epoch = status_response
        .chain_epoch
        .ok_or_else(|| Status::internal("WalletQuery.Transaction response missing chain_epoch"))?;
    let status = status_response
        .status
        .ok_or_else(|| Status::internal("WalletQuery.Transaction response missing status"))?;

    let (raw_bytes, location, mined_height, wallet_branch_id) = match status {
        transaction_status_response::Status::Mined(mined) => extract_mined(mined)?,
        transaction_status_response::Status::InMempool(mempool) => extract_mempool(mempool),
        transaction_status_response::Status::Conflicting(_) => {
            return Err(Status::failed_precondition(
                "transaction is conflicting-chain; ExplorerQuery.TransactionDetail returns mined or mempool only",
            ));
        }
    };

    let activations = NetworkUpgradeActivations::empty(network);
    let mut core_facts =
        zinder_source::parse_transaction_public_facts(&raw_bytes, mined_height, &activations)
            .map_err(|error| Status::internal(error.to_string()))?;
    if let Some(branch_id) = wallet_branch_id {
        core_facts.consensus_branch_id = Some(branch_id);
    }

    let freshness = ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: 0,
        derive_cursor_lag_blocks: 0,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_TRANSACTION_DETAIL_V1.to_owned(),
        unavailable: Vec::new(),
    };

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
        location: Some(location),
        raw_transaction_bytes: raw_bytes,
        paid_fee_zat,
        prevout_resolution_status,
        transparent_inputs,
    }))
}

/// Tuple returned by both extractors so the caller has one consistent
/// destructure target regardless of where the bytes came from. Mempool
/// extractions populate the trailing `Option`s with `None`.
type ExtractedTransaction = (
    Vec<u8>,
    WireTransactionLocation,
    Option<BlockHeight>,
    Option<ConsensusBranchId>,
);

fn extract_mined(mined: wallet::MinedTransaction) -> Result<ExtractedTransaction, Status> {
    let transaction = mined
        .transaction
        .ok_or_else(|| Status::internal("MinedTransaction missing transaction artifact"))?;
    let details = mined
        .details
        .ok_or_else(|| Status::internal("MinedTransaction missing details"))?;
    let mined_height = BlockHeight::new(transaction.block_height);
    let location = WireTransactionLocation {
        kind: Some(wire_location::Kind::Mined(MinedLocation {
            block_height: transaction.block_height,
            block_hash: transaction.block_hash,
            block_time_unix_seconds: details.block_time,
            confirmations: details.confirmations,
        })),
    };
    Ok((
        transaction.payload_bytes,
        location,
        Some(mined_height),
        Some(ConsensusBranchId::new(details.consensus_branch_id)),
    ))
}

fn extract_mempool(mempool: wallet::MempoolTransaction) -> ExtractedTransaction {
    let first_seen_unix_millis = u64::try_from(mempool.first_seen_unix_seconds)
        .map_or(0, |seconds| seconds.saturating_mul(1_000));
    let location = WireTransactionLocation {
        kind: Some(wire_location::Kind::InMempool(MempoolLocation {
            first_seen_unix_millis,
            first_seen_chain_epoch: None,
        })),
    };
    (mempool.payload_bytes, location, None, None)
}

fn encode_public_facts(facts: &CoreFacts) -> WireFacts {
    WireFacts {
        transaction_id: encode_internal_transaction_id(facts.transaction_id).to_vec(),
        auth_digest: facts
            .auth_digest
            .map(|digest| digest.as_bytes().to_vec())
            .unwrap_or_default(),
        wtxid: facts
            .wtxid
            .map(|wtxid| wtxid.as_bytes().to_vec())
            .unwrap_or_default(),
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
