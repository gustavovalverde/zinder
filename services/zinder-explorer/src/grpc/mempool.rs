//! `ExplorerQuery` mempool summary, activity, and coherent snapshot views.
//!
//! Each handler composes `WalletQuery.MempoolSnapshot` at request time;
//! no materialized-view consumer is required. `MempoolSummary` walks every
//! page under one stable wallet snapshot identity and aggregates with bounded
//! memory. `MempoolSnapshot` derives global summary facts and its requested
//! page from one wallet response. The activity feed projects the same entries
//! into typed rows ordered by newest-first observation time and paginates with
//! an opaque cursor that encodes `(first_seen_unix_millis, transaction_id)`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use tonic::{Request, Response, Status};
use zinder_core::wire::{decode_rpc_transaction_id_hex, encode_rpc_transaction_id_hex};
use zinder_core::{
    NetworkUpgradeActivations, TransactionPublicFacts as CoreFacts,
    TransactionVersion as CoreTransactionVersion,
};
use zinder_proto::capabilities::{EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_SUMMARY_V2};
use zinder_proto::v1::explorer::{
    MempoolActivityEntry, MempoolActivityRequest, MempoolActivityResponse, MempoolSnapshotRequest,
    MempoolSnapshotResponse, MempoolSnapshotSummary, MempoolSummaryRequest, MempoolSummaryResponse,
    PrivacyShapeCount, TransactionVersion as WireVersion, TransactionVersionCount,
    TransactionVersionKind,
};
use zinder_proto::v1::wallet::{
    self, MempoolEntry, MempoolSnapshotRequest as WalletMempoolSnapshotRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_proto::wire::encode_privacy_shape;
use zinder_runtime::AuthenticatedChannel;

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use super::transaction_detail::encode_component_counts;
use zinder_materialized_views::MaterializedViewStore;

/// Hard cap requested from the Wallet endpoint for each snapshot page.
///
/// The complete observation may walk up to [`MAX_MEMPOOL_SNAPSHOT_PAGES`]
/// pages. Bounding each page keeps each gRPC frame within tonic's per-message
/// limit even when every entry hydrates the raw transaction bytes.
const MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_PAGE: u32 = 4_096;

/// Hard cap on pages in a complete mempool observation.
///
/// The source already bounds entries per page. This second fence prevents a
/// malformed peer from keeping an explorer request alive by emitting an
/// endless sequence of distinct cursors.
const MAX_MEMPOOL_SUMMARY_PAGES: usize = 1_024;

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

#[derive(Default)]
struct MempoolSummaryBuilder {
    total_size_bytes: u64,
    transaction_count: u32,
    privacy_counts: BTreeMap<i32, u32>,
    version_counts: BTreeMap<i32, u32>,
    oldest_first_seen: Option<u64>,
    newest_first_seen: Option<u64>,
}

impl MempoolSummaryBuilder {
    fn observe(&mut self, entry: &MempoolEntry, facts: &CoreFacts) {
        self.transaction_count = self.transaction_count.saturating_add(1);
        self.total_size_bytes = self
            .total_size_bytes
            .saturating_add(u64::from(facts.size_bytes));
        *self
            .privacy_counts
            .entry(encode_privacy_shape(facts.privacy_shape) as i32)
            .or_insert(0) += 1;
        *self
            .version_counts
            .entry(encode_transaction_version_kind(facts.version) as i32)
            .or_insert(0) += 1;

        let first_seen = entry.first_seen_unix_millis;
        self.oldest_first_seen = Some(
            self.oldest_first_seen
                .map_or(first_seen, |prior| prior.min(first_seen)),
        );
        self.newest_first_seen = Some(
            self.newest_first_seen
                .map_or(first_seen, |prior| prior.max(first_seen)),
        );
    }

    fn finish(self, now_unix_millis: u64) -> MempoolSnapshotSummary {
        MempoolSnapshotSummary {
            transaction_count: self.transaction_count,
            total_size_bytes: self.total_size_bytes,
            privacy_shape_distribution: self
                .privacy_counts
                .into_iter()
                .map(|(shape, count)| PrivacyShapeCount { shape, count })
                .collect(),
            version_distribution: self
                .version_counts
                .into_iter()
                .map(|(kind, count)| TransactionVersionCount { kind, count })
                .collect(),
            oldest_entry_age_millis: self
                .oldest_first_seen
                .map_or(0, |first_seen| now_unix_millis.saturating_sub(first_seen)),
            newest_entry_age_millis: self
                .newest_first_seen
                .map_or(0, |first_seen| now_unix_millis.saturating_sub(first_seen)),
        }
    }
}

struct ParsedMempoolEntry<'entry> {
    entry: &'entry MempoolEntry,
    facts: CoreFacts,
}

struct MempoolSnapshotPage {
    summary: MempoolSnapshotSummary,
    entries: Vec<MempoolActivityEntry>,
    next_cursor: Vec<u8>,
}

/// One complete, identity-checked walk of the wallet mempool snapshot.
pub(crate) struct CompleteMempoolObservation {
    pub(crate) chain_epoch: wallet::ChainEpoch,
    pub(crate) source_tip: wallet::BlockTip,
    pub(crate) snapshot_age_millis: u64,
    pub(crate) summary: MempoolSnapshotSummary,
}

/// Executes one `ExplorerQuery.MempoolSummary` request.
pub(crate) async fn query_mempool_summary(
    materialized_view_store: Option<&MaterializedViewStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: zinder_core::Network,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<MempoolSummaryRequest>,
) -> Result<Response<MempoolSummaryResponse>, Status> {
    let _inner = request.into_inner();
    let observation = fetch_complete_mempool_observation(wallet_client, network).await?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            materialized_view_store,
            EXPLORER_MEMPOOL_SUMMARY_V2,
            Some(observation.chain_epoch),
            observation.snapshot_age_millis,
        )?,
    )
    .await;
    let summary = observation.summary;

    Ok(Response::new(MempoolSummaryResponse {
        freshness: Some(freshness),
        transaction_count: summary.transaction_count,
        total_size_bytes: summary.total_size_bytes,
        privacy_shape_distribution: summary.privacy_shape_distribution,
        version_distribution: summary.version_distribution,
        oldest_entry_age_millis: summary.oldest_entry_age_millis,
        newest_entry_age_millis: summary.newest_entry_age_millis,
    }))
}

/// Executes one `ExplorerQuery.MempoolSnapshot` request.
pub(crate) async fn query_mempool_snapshot(
    materialized_view_store: Option<&MaterializedViewStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: zinder_core::Network,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<MempoolSnapshotRequest>,
) -> Result<Response<MempoolSnapshotResponse>, Status> {
    let request = request.into_inner();
    let max_entries = clamp_max_entries(
        request.max_entries,
        DEFAULT_MEMPOOL_ACTIVITY_ENTRIES,
        MAX_MEMPOOL_ACTIVITY_ENTRIES_PER_REQUEST,
    );
    let cursor = decode_activity_cursor(&request.from_cursor)?;
    let snapshot = fetch_mempool_snapshot(wallet_client).await?;
    let now_unix_millis = current_unix_millis();
    let activations = NetworkUpgradeActivations::empty(network);
    let page = build_mempool_snapshot_page(
        &snapshot,
        &activations,
        max_entries,
        cursor,
        now_unix_millis,
    )?;
    let chain_epoch = snapshot
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("MempoolSnapshotResponse.chain_view.chain_epoch missing")
        })?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            materialized_view_store,
            zinder_proto::capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1,
            Some(chain_epoch),
            snapshot.snapshot_age_millis,
        )?,
    )
    .await;

    Ok(Response::new(MempoolSnapshotResponse {
        freshness: Some(freshness),
        summary: Some(page.summary),
        entries: page.entries,
        next_cursor: page.next_cursor,
    }))
}

fn build_mempool_snapshot_page(
    snapshot: &wallet::MempoolSnapshotResponse,
    activations: &NetworkUpgradeActivations,
    max_entries: u32,
    cursor: Option<(u64, u32)>,
    now_unix_millis: u64,
) -> Result<MempoolSnapshotPage, Status> {
    let mut summary = MempoolSummaryBuilder::default();
    let mut parsed_entries = Vec::with_capacity(snapshot.entries.len());

    for entry in &snapshot.entries {
        let facts = parse_facts(entry, activations)?;
        summary.observe(entry, &facts);
        parsed_entries.push(ParsedMempoolEntry { entry, facts });
    }

    parsed_entries.sort_by(|left, right| {
        right
            .entry
            .first_seen_unix_millis
            .cmp(&left.entry.first_seen_unix_millis)
            .then_with(|| {
                transaction_id_tail(&right.entry.transaction_id)
                    .cmp(&transaction_id_tail(&left.entry.transaction_id))
            })
    });

    let mut entries = Vec::with_capacity(max_entries as usize);
    let mut last_emitted: Option<(u64, u32)> = None;
    for parsed in parsed_entries {
        let position = (
            parsed.entry.first_seen_unix_millis,
            transaction_id_tail(&parsed.entry.transaction_id),
        );
        if let Some((cursor_millis, cursor_tail)) = cursor
            && (position.0, position.1) >= (cursor_millis, cursor_tail)
        {
            continue;
        }
        entries.push(build_mempool_activity_entry(parsed.entry, &parsed.facts));
        last_emitted = Some(position);
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= max_entries {
            break;
        }
    }

    Ok(MempoolSnapshotPage {
        summary: summary.finish(now_unix_millis),
        entries,
        next_cursor: last_emitted
            .map(|(millis, tail)| encode_activity_cursor(millis, tail))
            .unwrap_or_default(),
    })
}

/// Executes one `ExplorerQuery.MempoolActivity` request.
pub(crate) async fn query_mempool_activity(
    materialized_view_store: Option<&MaterializedViewStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: zinder_core::Network,
    upstream_observation_cache: &UpstreamObservationCache,
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
        entries.push(build_mempool_activity_entry(
            entry,
            &parse_facts(entry, &activations)?,
        ));
        last_emitted = Some(position);
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= max_entries {
            break;
        }
    }

    let next_cursor = last_emitted
        .map(|(millis, tail)| encode_activity_cursor(millis, tail))
        .unwrap_or_default();
    let chain_epoch = snapshot
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("MempoolSnapshotResponse.chain_view.chain_epoch missing")
        })?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            materialized_view_store,
            EXPLORER_MEMPOOL_ACTIVITY_V1,
            Some(chain_epoch),
            snapshot.snapshot_age_millis,
        )?,
    )
    .await;

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
        .mempool_snapshot(Request::new(WalletMempoolSnapshotRequest {
            max_entries: MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_PAGE,
            from_cursor: Vec::new(),
        }))
        .await?
        .into_inner())
}

trait MempoolSnapshotPageSource {
    async fn fetch_mempool_snapshot_page(
        &mut self,
        from_cursor: Vec<u8>,
    ) -> Result<wallet::MempoolSnapshotResponse, Status>;
}

impl MempoolSnapshotPageSource for WalletQueryClient<AuthenticatedChannel> {
    async fn fetch_mempool_snapshot_page(
        &mut self,
        from_cursor: Vec<u8>,
    ) -> Result<wallet::MempoolSnapshotResponse, Status> {
        Ok(self
            .mempool_snapshot(Request::new(WalletMempoolSnapshotRequest {
                max_entries: MAX_MEMPOOL_SNAPSHOT_ENTRIES_PER_PAGE,
                from_cursor,
            }))
            .await?
            .into_inner())
    }
}

pub(crate) async fn fetch_complete_mempool_observation(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: zinder_core::Network,
) -> Result<CompleteMempoolObservation, Status> {
    fetch_complete_mempool_observation_from(wallet_client, network).await
}

async fn fetch_complete_mempool_observation_from(
    page_source: &mut impl MempoolSnapshotPageSource,
    network: zinder_core::Network,
) -> Result<CompleteMempoolObservation, Status> {
    let activations = NetworkUpgradeActivations::empty(network);
    let mut summary = MempoolSummaryBuilder::default();
    let mut from_cursor = Vec::new();
    // One cursor per admitted page keeps cycle detection bounded by the same
    // hard page cap as the walk itself.
    let mut seen_continuation_cursors = BTreeSet::new();
    let mut identity: Option<(wallet::ChainEpoch, wallet::BlockTip, Vec<u8>)> = None;
    let mut snapshot_age_millis = 0_u64;

    for page_index in 0..MAX_MEMPOOL_SUMMARY_PAGES {
        let response = page_source
            .fetch_mempool_snapshot_page(from_cursor.clone())
            .await?;
        let page_identity = mempool_snapshot_identity(&response)?;
        match &identity {
            Some((expected_epoch, expected_source_tip, expected_events_cursor))
                if &page_identity.0 != expected_epoch
                    || &page_identity.1 != expected_source_tip
                    || page_identity.2.as_slice() != expected_events_cursor.as_slice() =>
            {
                return Err(ExplorerError::unsatisfied_precondition(format!(
                    "mempool snapshot identity changed on page {}",
                    page_index.saturating_add(1)
                ))
                .into());
            }
            None => {
                identity = Some(page_identity);
            }
            Some(_) => {}
        }
        snapshot_age_millis = snapshot_age_millis.max(response.snapshot_age_millis);
        for entry in &response.entries {
            let facts = parse_facts(entry, &activations)?;
            summary.observe(entry, &facts);
        }

        if response.next_cursor.is_empty() {
            let expected_identity = identity.ok_or_else(|| {
                ExplorerError::internal("complete mempool observation has no identity")
            })?;
            let final_probe = page_source.fetch_mempool_snapshot_page(Vec::new()).await?;
            let final_identity = mempool_snapshot_identity(&final_probe)?;
            if final_identity != expected_identity {
                return Err(ExplorerError::unsatisfied_precondition(
                    "mempool snapshot identity changed before the final head fence",
                )
                .into());
            }
            snapshot_age_millis = snapshot_age_millis.max(final_probe.snapshot_age_millis);
            return Ok(CompleteMempoolObservation {
                chain_epoch: expected_identity.0,
                source_tip: expected_identity.1,
                snapshot_age_millis,
                summary: summary.finish(current_unix_millis()),
            });
        }
        if response.next_cursor == from_cursor {
            return Err(ExplorerError::unsatisfied_precondition(
                "mempool snapshot next_cursor did not advance",
            )
            .into());
        }
        if !seen_continuation_cursors.insert(response.next_cursor.clone()) {
            return Err(ExplorerError::unsatisfied_precondition(
                "mempool snapshot repeated a continuation cursor",
            )
            .into());
        }
        if response.entries.is_empty() {
            return Err(ExplorerError::unsatisfied_precondition(
                "mempool snapshot returned an empty page with a continuation cursor",
            )
            .into());
        }
        from_cursor = response.next_cursor;
    }

    Err(ExplorerError::unsatisfied_precondition(format!(
        "mempool snapshot exceeded the {MAX_MEMPOOL_SUMMARY_PAGES}-page safety bound"
    ))
    .into())
}

fn mempool_snapshot_identity(
    response: &wallet::MempoolSnapshotResponse,
) -> Result<(wallet::ChainEpoch, wallet::BlockTip, Vec<u8>), Status> {
    let chain_epoch = response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| {
            ExplorerError::internal("MempoolSnapshotResponse.chain_view.chain_epoch missing")
        })?;
    let visible_tip = chain_epoch.visible_tip.as_ref().ok_or_else(|| {
        ExplorerError::internal("MempoolSnapshotResponse chain epoch visible_tip missing")
    })?;
    let source_tip = response
        .source_tip
        .as_ref()
        .ok_or_else(|| ExplorerError::internal("MempoolSnapshotResponse.source_tip missing"))?;
    if source_tip != visible_tip {
        return Err(ExplorerError::unsatisfied_precondition(
            "mempool snapshot source_tip does not match its chain epoch visible_tip",
        )
        .into());
    }
    Ok((
        chain_epoch.clone(),
        source_tip.clone(),
        response.events_resume_cursor.clone(),
    ))
}

fn parse_facts(
    entry: &MempoolEntry,
    activations: &NetworkUpgradeActivations,
) -> Result<CoreFacts, Status> {
    zinder_source::parse_transaction_public_facts(&entry.raw_transaction_bytes, None, activations)
        .map_err(|error| ExplorerError::internal(error.to_string()).into())
}

fn build_mempool_activity_entry(entry: &MempoolEntry, facts: &CoreFacts) -> MempoolActivityEntry {
    let counts = facts.counts;
    MempoolActivityEntry {
        transaction_id: encode_rpc_transaction_id_hex(facts.transaction_id),
        first_seen_unix_millis: entry.first_seen_unix_millis,
        size_bytes: facts.size_bytes,
        privacy_shape: encode_privacy_shape(facts.privacy_shape) as i32,
        version: Some(encode_transaction_version(facts.version)),
        zip317_conventional_fee_zat: counts.zip317_conventional_fee_zat(),
        paid_fee_zat: None,
        logical_actions: counts.logical_actions(),
        component_counts: Some(encode_component_counts(counts)),
        transparent_output_total_zat: entry
            .transparent_outputs
            .iter()
            .fold(0_u64, |total, output| {
                total.saturating_add(output.value_zat)
            }),
    }
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
        CoreTransactionVersion::V6 => TransactionVersionKind::V6,
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
        | CoreTransactionVersion::V5
        | CoreTransactionVersion::V6 => None,
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
        return Err(ExplorerError::invalid_request(format!(
            "MempoolActivity cursor must be {CURSOR_BYTES} bytes; got {}",
            cursor.len()
        ))
        .into());
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

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use std::collections::VecDeque;

    use super::{
        MAX_MEMPOOL_SUMMARY_PAGES, MempoolSnapshotPageSource, build_mempool_activity_entry,
        build_mempool_snapshot_page, fetch_complete_mempool_observation_from,
    };
    use tonic::{Code, Status};
    use zinder_core::{Network, TransactionId, wire::encode_rpc_transaction_id_hex};
    use zinder_proto::v1::wallet;
    use zinder_source::parse_transaction_public_facts;

    struct ScriptedMempoolSnapshotPageSource {
        pages: VecDeque<wallet::MempoolSnapshotResponse>,
        requested_cursors: Vec<Vec<u8>>,
    }

    impl ScriptedMempoolSnapshotPageSource {
        fn new(pages: Vec<wallet::MempoolSnapshotResponse>) -> Self {
            Self {
                pages: pages.into(),
                requested_cursors: Vec::new(),
            }
        }
    }

    impl MempoolSnapshotPageSource for ScriptedMempoolSnapshotPageSource {
        async fn fetch_mempool_snapshot_page(
            &mut self,
            from_cursor: Vec<u8>,
        ) -> Result<wallet::MempoolSnapshotResponse, Status> {
            self.requested_cursors.push(from_cursor);
            self.pages
                .pop_front()
                .ok_or_else(|| Status::internal("scripted mempool page source exhausted"))
        }
    }

    #[test]
    fn mempool_activity_entry_uses_parsed_counts_and_live_output_total()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let raw_transaction_bytes = transparent_transaction_bytes();
        let facts = parse_transaction_public_facts(&raw_transaction_bytes, None, &activations)?;
        let transaction_id = facts.transaction_id;
        let entry = wallet::MempoolEntry {
            transaction_id: encode_rpc_transaction_id_hex(transaction_id),
            auth_digest: String::new(),
            raw_transaction_bytes,
            compact_transaction_data: Some(wallet::CompactTransactionData::default()),
            first_seen_unix_millis: 1_700_000_000_000,
            first_seen_chain_epoch: None,
            transparent_outputs: vec![
                transparent_output(transaction_id, 0, 1_000, vec![0x51]),
                transparent_output(transaction_id, 1, 2_500, vec![0x52]),
            ],
            transparent_spends: Vec::new(),
        };

        let activity = build_mempool_activity_entry(&entry, &facts);
        let counts = activity.component_counts.ok_or_else(|| {
            std::io::Error::other("mempool activity entry must carry component counts")
        })?;

        assert_eq!(counts.transparent_input_count, 1);
        assert_eq!(counts.transparent_output_count, 2);
        assert_eq!(counts.sapling_spend_count, 0);
        assert_eq!(counts.sapling_output_count, 0);
        assert_eq!(counts.orchard_action_count, 0);
        assert_eq!(counts.ironwood_action_count, 0);
        assert_eq!(activity.transparent_output_total_zat, 3_500);
        Ok(())
    }

    #[test]
    fn mempool_snapshot_page_keeps_full_totals_with_a_bounded_page()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let first = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let mut newer_bytes = transparent_transaction_bytes();
        let lock_time_offset = newer_bytes.len().saturating_sub(4);
        newer_bytes[lock_time_offset..].copy_from_slice(&1_u32.to_le_bytes());
        let newer = mempool_entry(newer_bytes, 1_700_000_000_100, &activations)?;

        let page = build_mempool_snapshot_page(
            &wallet::MempoolSnapshotResponse {
                entries: vec![first, newer],
                ..Default::default()
            },
            &activations,
            1,
            None,
            1_700_000_001_000,
        )?;

        assert_eq!(page.summary.transaction_count, 2);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].first_seen_unix_millis, 1_700_000_000_100);
        assert_eq!(
            page.summary.total_size_bytes,
            u64::from(page.entries[0].size_bytes).saturating_mul(2)
        );
        assert_eq!(
            page.summary.oldest_entry_age_millis, 1_000,
            "the summary must include entries excluded from the bounded page",
        );
        assert!(!page.next_cursor.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn complete_mempool_observation_walks_every_page_under_one_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let first = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let mut newer_bytes = transparent_transaction_bytes();
        let lock_time_offset = newer_bytes.len().saturating_sub(4);
        newer_bytes[lock_time_offset..].copy_from_slice(&1_u32.to_le_bytes());
        let newer = mempool_entry(newer_bytes, 1_700_000_000_100, &activations)?;
        let continuation = b"page-2".to_vec();
        let mut source = ScriptedMempoolSnapshotPageSource::new(vec![
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"events".to_vec(),
                vec![first.clone()],
                continuation.clone(),
                11,
            ),
            wallet_snapshot_page(7, 20, 0x33, b"events".to_vec(), vec![newer], Vec::new(), 17),
            wallet_snapshot_page(7, 20, 0x33, b"events".to_vec(), vec![first], Vec::new(), 17),
        ]);

        let observation =
            fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest).await?;

        assert_eq!(
            source.requested_cursors,
            vec![Vec::<u8>::new(), continuation, Vec::<u8>::new()]
        );
        assert_eq!(observation.summary.transaction_count, 2);
        assert_eq!(observation.snapshot_age_millis, 17);
        assert_eq!(observation.chain_epoch.chain_epoch_id, 7);
        assert_eq!(observation.source_tip.height, 20);
        Ok(())
    }

    #[tokio::test]
    async fn complete_mempool_observation_rejects_a_changed_final_head_probe()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let entry = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let mut source = ScriptedMempoolSnapshotPageSource::new(vec![
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"original-events".to_vec(),
                vec![entry.clone()],
                b"page-2".to_vec(),
                0,
            ),
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"original-events".to_vec(),
                vec![entry.clone()],
                Vec::new(),
                0,
            ),
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"new-head-events".to_vec(),
                vec![entry],
                Vec::new(),
                0,
            ),
        ]);

        let error = fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest)
            .await
            .err()
            .ok_or_else(|| {
                std::io::Error::other("changed current mempool head must invalidate the walk")
            })?;

        assert_eq!(error.code(), Code::FailedPrecondition);
        assert!(error.message().contains("final head fence"));
        assert_eq!(
            source.requested_cursors,
            vec![Vec::<u8>::new(), b"page-2".to_vec(), Vec::<u8>::new()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn complete_mempool_observation_rejects_nonprogress_and_empty_continuation()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let entry = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let continuation = b"page-2".to_vec();
        let first = wallet_snapshot_page(
            7,
            20,
            0x33,
            b"events".to_vec(),
            vec![entry.clone()],
            continuation.clone(),
            0,
        );
        let nonprogress = wallet_snapshot_page(
            7,
            20,
            0x33,
            b"events".to_vec(),
            vec![entry],
            continuation,
            0,
        );
        let mut source = ScriptedMempoolSnapshotPageSource::new(vec![first, nonprogress]);
        let error = fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest)
            .await
            .err()
            .ok_or_else(|| std::io::Error::other("non-progressing cursor must fail"))?;
        assert_eq!(error.code(), Code::FailedPrecondition);
        assert!(error.message().contains("did not advance"));

        let empty_continuation = wallet_snapshot_page(
            7,
            20,
            0x33,
            b"events".to_vec(),
            Vec::new(),
            b"page-2".to_vec(),
            0,
        );
        let mut source = ScriptedMempoolSnapshotPageSource::new(vec![empty_continuation]);
        let error = fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest)
            .await
            .err()
            .ok_or_else(|| {
                std::io::Error::other("empty page with continuation cursor must fail")
            })?;
        assert_eq!(error.code(), Code::FailedPrecondition);
        assert!(error.message().contains("empty page"));
        Ok(())
    }

    #[tokio::test]
    async fn complete_mempool_observation_rejects_a_continuation_cursor_cycle()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let entry = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let first_cursor = b"page-a".to_vec();
        let second_cursor = b"page-b".to_vec();
        let pages = [
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"events".to_vec(),
                vec![entry.clone()],
                first_cursor.clone(),
                0,
            ),
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"events".to_vec(),
                vec![entry.clone()],
                second_cursor.clone(),
                0,
            ),
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"events".to_vec(),
                vec![entry],
                first_cursor.clone(),
                0,
            ),
        ];
        let mut source = ScriptedMempoolSnapshotPageSource::new(pages.into());

        let error = fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest)
            .await
            .err()
            .ok_or_else(|| std::io::Error::other("continuation cursor cycle must fail"))?;

        assert_eq!(error.code(), Code::FailedPrecondition);
        assert!(error.message().contains("repeated a continuation cursor"));
        assert_eq!(
            source.requested_cursors,
            vec![Vec::<u8>::new(), first_cursor, second_cursor]
        );
        Ok(())
    }

    #[tokio::test]
    async fn complete_mempool_observation_rejects_identity_changes_between_pages()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let entry = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let mut changed_source_tip = wallet_snapshot_page(
            7,
            20,
            0x33,
            b"events".to_vec(),
            vec![entry.clone()],
            Vec::new(),
            0,
        );
        changed_source_tip.source_tip = Some(wallet::BlockTip {
            height: 19,
            hash: "22".repeat(32),
        });
        let changed_identity_pages = [
            wallet_snapshot_page(
                8,
                20,
                0x33,
                b"events".to_vec(),
                vec![entry.clone()],
                Vec::new(),
                0,
            ),
            wallet_snapshot_page(
                7,
                21,
                0x22,
                b"events".to_vec(),
                vec![entry.clone()],
                Vec::new(),
                0,
            ),
            wallet_snapshot_page(
                7,
                20,
                0x33,
                b"different-events".to_vec(),
                vec![entry.clone()],
                Vec::new(),
                0,
            ),
            changed_source_tip,
        ];

        for changed_page in changed_identity_pages {
            let first = wallet_snapshot_page(
                7,
                20,
                0x33,
                b"events".to_vec(),
                vec![entry.clone()],
                b"page-2".to_vec(),
                0,
            );
            let mut source = ScriptedMempoolSnapshotPageSource::new(vec![first, changed_page]);
            let error = fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest)
                .await
                .err()
                .ok_or_else(|| std::io::Error::other("changed mempool page identity must fail"))?;
            assert_eq!(error.code(), Code::FailedPrecondition);
        }
        Ok(())
    }

    #[tokio::test]
    async fn complete_mempool_observation_enforces_page_safety_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let activations = zinder_core::NetworkUpgradeActivations::empty(Network::ZcashRegtest);
        let entry = mempool_entry(
            transparent_transaction_bytes(),
            1_700_000_000_000,
            &activations,
        )?;
        let pages = (1..=MAX_MEMPOOL_SUMMARY_PAGES)
            .map(|page| {
                wallet_snapshot_page(
                    7,
                    20,
                    0x33,
                    b"events".to_vec(),
                    vec![entry.clone()],
                    page.to_be_bytes().to_vec(),
                    0,
                )
            })
            .collect();
        let mut source = ScriptedMempoolSnapshotPageSource::new(pages);

        let error = fetch_complete_mempool_observation_from(&mut source, Network::ZcashRegtest)
            .await
            .err()
            .ok_or_else(|| std::io::Error::other("unbounded mempool page walk must fail"))?;

        assert_eq!(error.code(), Code::FailedPrecondition);
        assert!(error.message().contains("page safety bound"));
        assert_eq!(source.requested_cursors.len(), MAX_MEMPOOL_SUMMARY_PAGES);
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Each argument controls one independently varied snapshot-identity or page field."
    )]
    fn wallet_snapshot_page(
        chain_epoch_id: u64,
        tip_height: u32,
        tip_hash_byte: u8,
        events_resume_cursor: Vec<u8>,
        entries: Vec<wallet::MempoolEntry>,
        next_cursor: Vec<u8>,
        snapshot_age_millis: u64,
    ) -> wallet::MempoolSnapshotResponse {
        let tip = wallet::BlockTip {
            height: tip_height,
            hash: format!("{tip_hash_byte:02x}").repeat(32),
        };
        wallet::MempoolSnapshotResponse {
            chain_view: Some(wallet::ChainView {
                chain_epoch: Some(wallet::ChainEpoch {
                    chain_epoch_id,
                    visible_tip: Some(tip.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            events_resume_cursor,
            snapshot_age_millis,
            entries,
            next_cursor,
            source_tip: Some(tip),
        }
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

    fn mempool_entry(
        raw_transaction_bytes: Vec<u8>,
        first_seen_unix_millis: u64,
        activations: &zinder_core::NetworkUpgradeActivations,
    ) -> Result<wallet::MempoolEntry, Box<dyn std::error::Error>> {
        let facts = parse_transaction_public_facts(&raw_transaction_bytes, None, activations)?;
        Ok(wallet::MempoolEntry {
            transaction_id: encode_rpc_transaction_id_hex(facts.transaction_id),
            auth_digest: String::new(),
            raw_transaction_bytes,
            compact_transaction_data: Some(wallet::CompactTransactionData::default()),
            first_seen_unix_millis,
            first_seen_chain_epoch: None,
            transparent_outputs: Vec::new(),
            transparent_spends: Vec::new(),
        })
    }

    fn transparent_output(
        transaction_id: TransactionId,
        output_index: u32,
        value_zat: u64,
        script_pub_key: Vec<u8>,
    ) -> wallet::TransparentMempoolOutput {
        wallet::TransparentMempoolOutput {
            address_script_hash: vec![0xA5; 32],
            script_pub_key,
            outpoint: Some(wallet::OutPoint {
                transaction_id: encode_rpc_transaction_id_hex(transaction_id),
                output_index,
            }),
            value_zat,
        }
    }
}
