//! `ExplorerQuery.MempoolSummary` and `ExplorerQuery.MempoolActivity`.
//!
//! Both handlers compose `WalletQuery.MempoolSnapshot` at request time;
//! no derive consumer is required. The summary aggregates every entry
//! into one explorer-shaped page; the activity feed projects the same
//! entries into typed rows ordered by newest-first observation time and
//! paginates with an opaque cursor that encodes
//! `(first_seen_unix_millis, transaction_id)`.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use tonic::{Request, Response, Status};
use zinder_core::wire::{decode_rpc_transaction_id_hex, encode_rpc_transaction_id_hex};
use zinder_core::{
    NetworkUpgradeActivations, TransactionPublicFacts as CoreFacts,
    TransactionVersion as CoreTransactionVersion,
};
use zinder_proto::capabilities::{EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_SUMMARY_V1};
use zinder_proto::v1::explorer::{
    ExplorerFreshness, MempoolActivityEntry, MempoolActivityRequest, MempoolActivityResponse,
    MempoolSummaryRequest, MempoolSummaryResponse, PrivacyShapeCount,
    TransactionVersion as WireVersion, TransactionVersionCount, TransactionVersionKind,
};
use zinder_proto::v1::wallet::{
    self, MempoolEntry, MempoolSnapshotRequest, wallet_query_client::WalletQueryClient,
};
use zinder_proto::wire::encode_privacy_shape;
use zinder_runtime::AuthenticatedChannel;

/// Hard cap on the mempool entries the summary aggregates in one call.
///
/// Mempool sizes on mainnet sit in the low thousands; bounding the read
/// keeps parse cost predictable and the gRPC frame within tonic's
/// per-message limit even when every entry hydrates the raw transaction
/// bytes.
const MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_REQUEST: u32 = 4_096;

/// Hard cap on the rows one `MempoolActivity` page returns.
///
/// The handler parses every entry it returns to populate the privacy
/// shape and version classifications, so the cap mirrors other
/// page-oriented explorer reads.
const MAX_MEMPOOL_ACTIVITY_ENTRIES_PER_REQUEST: u32 = 256;

const DEFAULT_MEMPOOL_ACTIVITY_ENTRIES: u32 = 64;

/// Cursor envelope used by `MempoolActivity` pagination.
///
/// Layout (12-byte big-endian-packed):
///   [0..8)  `first_seen_unix_millis`
///   [8..12) prefix of `transaction_id` (last 4 bytes)
///
/// The activity feed orders entries newest-first. To resume strictly
/// after a prior entry, the server compares entries against the cursor:
/// `(millis, txid_prefix)` greater-than means "newer," so the resume
/// criterion is "the entry's `(millis, txid_prefix)` is strictly less
/// than the cursor's." Ties on `first_seen_unix_millis` are broken by
/// the txid-prefix tail; a full-txid cursor would be more robust but the
/// 4-byte prefix is sufficient given the per-request cap.
const CURSOR_BYTES: usize = 12;

/// Executes one `ExplorerQuery.MempoolSummary` request.
pub(crate) async fn handle_mempool_summary(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: zinder_core::Network,
    request: Request<MempoolSummaryRequest>,
) -> Result<Response<MempoolSummaryResponse>, Status> {
    let _inner = request.into_inner();
    let snapshot = fetch_mempool_snapshot(wallet_client).await?;
    let now_unix_millis = current_unix_millis();
    let activations = NetworkUpgradeActivations::empty(network);

    let mut total_size_bytes: u64 = 0;
    let mut transaction_count: u32 = 0;
    let mut privacy_counts: BTreeMap<i32, u32> = BTreeMap::new();
    let mut version_counts: BTreeMap<i32, u32> = BTreeMap::new();
    let mut oldest_first_seen: Option<u64> = None;
    let mut newest_first_seen: Option<u64> = None;

    for entry in &snapshot.entries {
        let facts = parse_facts(entry, &activations)?;
        transaction_count = transaction_count.saturating_add(1);
        total_size_bytes = total_size_bytes.saturating_add(u64::from(facts.size_bytes));
        *privacy_counts
            .entry(encode_privacy_shape(facts.privacy_shape) as i32)
            .or_insert(0) += 1;
        *version_counts
            .entry(encode_transaction_version_kind(facts.version) as i32)
            .or_insert(0) += 1;

        let first_seen = entry.first_seen_unix_millis;
        oldest_first_seen =
            Some(oldest_first_seen.map_or(first_seen, |prior| prior.min(first_seen)));
        newest_first_seen =
            Some(newest_first_seen.map_or(first_seen, |prior| prior.max(first_seen)));
    }

    let chain_epoch = snapshot
        .chain_epoch
        .clone()
        .ok_or_else(|| Status::internal("MempoolSnapshotResponse.chain_epoch missing"))?;
    let freshness = ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: snapshot.snapshot_age_millis,
        derive_cursor_lag_blocks: 0,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_MEMPOOL_SUMMARY_V1.to_owned(),
        unavailable: Vec::new(),
    };

    Ok(Response::new(MempoolSummaryResponse {
        freshness: Some(freshness),
        transaction_count,
        total_size_bytes,
        privacy_shape_distribution: privacy_counts
            .into_iter()
            .map(|(shape, count)| PrivacyShapeCount { shape, count })
            .collect(),
        version_distribution: version_counts
            .into_iter()
            .map(|(kind, count)| TransactionVersionCount { kind, count })
            .collect(),
        oldest_entry_age_millis: oldest_first_seen
            .map_or(0, |first_seen| now_unix_millis.saturating_sub(first_seen)),
        newest_entry_age_millis: newest_first_seen
            .map_or(0, |first_seen| now_unix_millis.saturating_sub(first_seen)),
    }))
}

/// Executes one `ExplorerQuery.MempoolActivity` request.
pub(crate) async fn handle_mempool_activity(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: zinder_core::Network,
    request: Request<MempoolActivityRequest>,
) -> Result<Response<MempoolActivityResponse>, Status> {
    let inner = request.into_inner();
    let max_entries = clamp_max_entries(
        inner.max_entries,
        DEFAULT_MEMPOOL_ACTIVITY_ENTRIES,
        MAX_MEMPOOL_ACTIVITY_ENTRIES_PER_REQUEST,
    );
    let cursor = decode_activity_cursor(&inner.from_cursor)?;

    let snapshot = fetch_mempool_snapshot(wallet_client).await?;
    let activations = NetworkUpgradeActivations::empty(network);

    let mut sorted: Vec<&MempoolEntry> = snapshot.entries.iter().collect();
    sorted.sort_by(|left, right| {
        right
            .first_seen_unix_millis
            .cmp(&left.first_seen_unix_millis)
            .then_with(|| {
                transaction_id_tail(&right.transaction_id)
                    .cmp(&transaction_id_tail(&left.transaction_id))
            })
    });

    let mut entries = Vec::with_capacity(max_entries as usize);
    let mut last_emitted: Option<(u64, u32)> = None;
    for entry in sorted {
        let position = (
            entry.first_seen_unix_millis,
            transaction_id_tail(&entry.transaction_id),
        );
        if let Some((cursor_millis, cursor_tail)) = cursor
            && (position.0, position.1) >= (cursor_millis, cursor_tail)
        {
            continue;
        }
        let facts = parse_facts(entry, &activations)?;
        let counts = facts.counts;
        let logical_actions = counts.logical_actions();
        entries.push(MempoolActivityEntry {
            transaction_id: encode_rpc_transaction_id_hex(facts.transaction_id),
            first_seen_unix_millis: entry.first_seen_unix_millis,
            size_bytes: facts.size_bytes,
            privacy_shape: encode_privacy_shape(facts.privacy_shape) as i32,
            version: Some(encode_transaction_version(facts.version)),
            zip317_conventional_fee_zat: counts.zip317_conventional_fee_zat(),
            paid_fee_zat: None,
            logical_actions,
        });
        last_emitted = Some(position);
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= max_entries {
            break;
        }
    }

    let next_cursor = last_emitted
        .map(|(millis, tail)| encode_activity_cursor(millis, tail))
        .unwrap_or_default();
    let chain_epoch = snapshot
        .chain_epoch
        .ok_or_else(|| Status::internal("MempoolSnapshotResponse.chain_epoch missing"))?;
    let freshness = ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: snapshot.snapshot_age_millis,
        derive_cursor_lag_blocks: 0,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_MEMPOOL_ACTIVITY_V1.to_owned(),
        unavailable: Vec::new(),
    };

    Ok(Response::new(MempoolActivityResponse {
        freshness: Some(freshness),
        entries,
        next_cursor,
    }))
}

async fn fetch_mempool_snapshot(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<wallet::MempoolSnapshotResponse, Status> {
    Ok(wallet_client
        .mempool_snapshot(Request::new(MempoolSnapshotRequest {
            max_entries: MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_REQUEST,
            from_cursor: Vec::new(),
        }))
        .await?
        .into_inner())
}

fn parse_facts(
    entry: &MempoolEntry,
    activations: &NetworkUpgradeActivations,
) -> Result<CoreFacts, Status> {
    zinder_source::parse_transaction_public_facts(&entry.raw_transaction_bytes, None, activations)
        .map_err(|error| Status::internal(error.to_string()))
}

const fn encode_transaction_version_kind(
    version: CoreTransactionVersion,
) -> TransactionVersionKind {
    match version {
        CoreTransactionVersion::V1 => TransactionVersionKind::V1,
        CoreTransactionVersion::V2 => TransactionVersionKind::V2,
        CoreTransactionVersion::V3 => TransactionVersionKind::V3,
        CoreTransactionVersion::V4 => TransactionVersionKind::V4,
        CoreTransactionVersion::V5 => TransactionVersionKind::V5,
        CoreTransactionVersion::Unsupported { .. } => TransactionVersionKind::Unsupported,
    }
}

fn encode_transaction_version(version: CoreTransactionVersion) -> WireVersion {
    let kind = encode_transaction_version_kind(version);
    let version_group_id = match version {
        CoreTransactionVersion::Unsupported {
            version_group_id, ..
        } => version_group_id,
        CoreTransactionVersion::V1
        | CoreTransactionVersion::V2
        | CoreTransactionVersion::V3
        | CoreTransactionVersion::V4
        | CoreTransactionVersion::V5 => None,
    };
    WireVersion {
        kind: kind as i32,
        effective_version: version.effective_version(),
        version_group_id,
    }
}

/// Returns the trailing 4 internal-byte-order bytes as a big-endian `u32`.
///
/// The caller passes the canonical RPC-form 64-character hex string;
/// decode failures produce 0 so a pathological wallet response cannot
/// panic the activity sort.
fn transaction_id_tail(transaction_id_rpc_hex: &str) -> u32 {
    let Ok(internal_bytes) = decode_rpc_transaction_id_hex(transaction_id_rpc_hex) else {
        return 0;
    };
    let bytes = internal_bytes.as_bytes();
    let mut tail = [0_u8; 4];
    tail.copy_from_slice(&bytes[bytes.len() - 4..]);
    u32::from_be_bytes(tail)
}

fn encode_activity_cursor(first_seen_unix_millis: u64, transaction_id_tail: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(CURSOR_BYTES);
    bytes.extend_from_slice(&first_seen_unix_millis.to_be_bytes());
    bytes.extend_from_slice(&transaction_id_tail.to_be_bytes());
    bytes
}

fn decode_activity_cursor(cursor: &[u8]) -> Result<Option<(u64, u32)>, Status> {
    if cursor.is_empty() {
        return Ok(None);
    }
    if cursor.len() != CURSOR_BYTES {
        return Err(Status::invalid_argument(format!(
            "MempoolActivity cursor must be {CURSOR_BYTES} bytes; got {}",
            cursor.len()
        )));
    }
    let mut millis_bytes = [0_u8; 8];
    millis_bytes.copy_from_slice(&cursor[0..8]);
    let mut tail_bytes = [0_u8; 4];
    tail_bytes.copy_from_slice(&cursor[8..12]);
    Ok(Some((
        u64::from_be_bytes(millis_bytes),
        u32::from_be_bytes(tail_bytes),
    )))
}

fn clamp_max_entries(requested: u32, default: u32, cap: u32) -> u32 {
    let target = if requested == 0 { default } else { requested };
    target.min(cap)
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
