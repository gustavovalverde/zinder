//! Public seam for the out-of-tree fixed-range benchmark harness.
//!
//! The `zinder-bench` crate replays the real bulk-catchup pipeline over
//! captured source payloads and a cloned canonical store. Everything the
//! replay drives (`run_bulk_catchup_with_store`, `catch_up_derive_store_to_canonical`,
//! `open_primary_derive_store_for_canonical`) is already part of the crate's
//! public surface; this module adds only the one thing the harness cannot
//! assemble on its own: a [`BulkCatchupRunConfig`] filled with
//! production-representative bulk-catchup defaults so the benchmark varies only
//! the knobs under measurement (prepare concurrency, cache size, range).
//!
//! The fixed replay knobs are grouped in [`CanonicalPipelineLimits`], the same
//! value consumed by runtime bulk catchup. Benchmark-specific values remain
//! explicit because fixture sweeps are diagnostic rather than deployment
//! certification.

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use zinder_core::{BlockHeight, Network, NetworkUpgradeActivations};
use zinder_source::{NodeAuth, NodeTarget};
use zinder_store::RocksDbResourceBudget;

use crate::{
    BulkCatchupRunConfig, CanonicalPipelineLimits,
    DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
    DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE, DeriveReplayPolicy,
    IngestDeriveConfig, NodeSourceKind, RawBlobPolicy,
};

/// Maximum blocks committed in one bulk-catchup epoch.
pub const BENCH_CANONICAL_BATCH_MAX_BLOCKS: u32 = 1_000;
/// Maximum in-memory canonical artifact bytes accumulated before commit.
pub const BENCH_CANONICAL_BATCH_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum connected blocks requested from the source in one segment.
pub const BENCH_SOURCE_SEGMENT_MAX_BLOCKS: u32 = 16;
/// Target source response payload bytes for adaptive segment sizing.
pub const BENCH_SOURCE_SEGMENT_TARGET_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum concurrent source segment fetches.
pub const BENCH_SOURCE_FETCH_MAX_IN_FLIGHT_REQUESTS: u32 = 12;
/// Maximum reserved response bytes across source fetches.
pub const BENCH_SOURCE_FETCH_MAX_IN_FLIGHT_BYTES: u64 = 384 * 1024 * 1024;
/// Admission watermark for active prepare peaks and completed resident data.
pub const BENCH_BLOCK_PREPARE_MEMORY_WATERMARK_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum safe-tip artifact bytes queued while the previous batch commits.
pub const BENCH_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
/// Force a `RocksDB` flush every N committed epochs.
pub const BENCH_FLUSH_INTERVAL_EPOCHS: u32 = 5;
/// Per-request source timeout applied to the placeholder node target.
pub const BENCH_NODE_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Maximum response body size applied to the placeholder node target.
pub const BENCH_MAX_RESPONSE_BYTES: u64 = 384 * 1024 * 1024;
/// Derive-replay batch size used when the harness drives derive replay.
pub const BENCH_DERIVE_REPLAY_BATCH_BLOCKS: u32 = 100;
/// Smallest derive-replay batch under memory pressure.
pub const BENCH_DERIVE_MIN_REPLAY_BATCH_BLOCKS: u32 = 10;
/// Memory ratio at which derive replay starts shrinking its batch.
pub const BENCH_DERIVE_MEMORY_DEGRADE_RATIO: f64 = 0.90;
/// Memory ratio at which derive replay pauses.
pub const BENCH_DERIVE_MEMORY_PAUSE_RATIO: f64 = 0.99;
/// Memory ratio below which derive replay resumes normal batching.
pub const BENCH_DERIVE_MEMORY_RESUME_RATIO: f64 = 0.80;
/// Residual derive lag at which a production boot hands replay to the tailer;
/// inert here because the harness always drains derive replay to completion.
pub const BENCH_DERIVE_STARTUP_HANDOFF_LAG_BLOCKS: u64 = 1_000;

const BENCH_PLACEHOLDER_JSON_RPC_ADDR: &str = "http://127.0.0.1:0";

const fn nz32(candidate: u32) -> NonZeroU32 {
    match NonZeroU32::new(candidate) {
        Some(bound) => bound,
        None => NonZeroU32::MIN,
    }
}

const fn nz64(candidate: u64) -> NonZeroU64 {
    match NonZeroU64::new(candidate) {
        Some(bound) => bound,
        None => NonZeroU64::MIN,
    }
}

/// Measured-and-varied inputs for a fixed-range benchmark run.
///
/// Every field the benchmark sweeps is explicit here; the rest of the
/// bulk-catchup configuration is filled from the `BENCH_*` defaults by
/// [`bench_bulk_catchup_run_config`].
#[derive(Clone, Debug)]
pub struct BenchBulkCatchupParams {
    /// Network the captured fixture belongs to.
    pub network: Network,
    /// Canonical store path (a writable clone of the captured start state).
    pub storage_path: PathBuf,
    /// First block height to replay.
    pub from_height: BlockHeight,
    /// Last block height to replay.
    pub to_height: BlockHeight,
    /// Parallel canonical block-prepare slots (the primary sweep knob).
    pub block_prepare_concurrency: NonZeroU32,
    /// Bounded `RocksDB` budget applied to the canonical store (the cache-size
    /// sweep knob).
    pub canonical_rocksdb_budget: RocksDbResourceBudget,
    /// Raw-block blob write policy.
    pub raw_blob_policy: RawBlobPolicy,
    /// Consensus activations captured with the fixture.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

/// Assembles a [`BulkCatchupRunConfig`] for a fixed-range benchmark replay.
///
/// The returned configuration finalizes the entire captured range in one call
/// (`upstream_tip_hint` pinned to `to_height`, `allow_near_tip_finalize`
/// enabled) because the fixture is immutable history with no live reorg window.
#[must_use]
pub fn bench_bulk_catchup_run_config(params: BenchBulkCatchupParams) -> BulkCatchupRunConfig {
    let node = NodeTarget::new(
        params.network,
        BENCH_PLACEHOLDER_JSON_RPC_ADDR.to_owned(),
        NodeAuth::None,
        Duration::from_secs(BENCH_NODE_REQUEST_TIMEOUT_SECS),
        nz64(BENCH_MAX_RESPONSE_BYTES),
    );
    BulkCatchupRunConfig {
        node,
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path: params.storage_path,
        canonical_rocksdb_budget: params.canonical_rocksdb_budget,
        reorg_window_blocks: 100,
        raw_blob_policy: params.raw_blob_policy,
        network_upgrade_activations: params.network_upgrade_activations,
        from_height: params.from_height,
        to_height: params.to_height,
        canonical_batch_max_blocks: nz32(BENCH_CANONICAL_BATCH_MAX_BLOCKS),
        canonical_batch_max_artifact_bytes: nz64(BENCH_CANONICAL_BATCH_MAX_ARTIFACT_BYTES),
        canonical_batch_max_estimated_write_bytes: nz64(
            DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        ),
        canonical_batch_min_blocks_before_estimated_write_close: nz32(
            DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        ),
        pipeline_limits: CanonicalPipelineLimits {
            max_response_bytes: nz64(BENCH_MAX_RESPONSE_BYTES),
            source_segment_max_blocks: nz32(BENCH_SOURCE_SEGMENT_MAX_BLOCKS),
            source_segment_target_response_bytes: nz64(BENCH_SOURCE_SEGMENT_TARGET_RESPONSE_BYTES),
            source_fetch_max_in_flight_requests: nz32(BENCH_SOURCE_FETCH_MAX_IN_FLIGHT_REQUESTS),
            source_fetch_max_in_flight_bytes: nz64(BENCH_SOURCE_FETCH_MAX_IN_FLIGHT_BYTES),
            block_prepare_concurrency: params.block_prepare_concurrency,
            block_prepare_memory_watermark_bytes: nz64(BENCH_BLOCK_PREPARE_MEMORY_WATERMARK_BYTES),
        },
        commit_reassembly_max_queued_artifact_bytes: nz64(
            BENCH_COMMIT_REASSEMBLY_MAX_QUEUED_ARTIFACT_BYTES,
        ),
        flush_interval_epochs: nz32(BENCH_FLUSH_INTERVAL_EPOCHS),
        upstream_tip_hint: Some(params.to_height),
        allow_near_tip_finalize: true,
        checkpoint: None,
    }
}

/// Returns the derive-replay configuration the harness drives under `--derive`.
///
/// `CanonicalFirst` matches the default indexing posture; the harness runs the
/// catch-up to completion regardless, so the policy only shapes memory-pressure
/// throttling during the run.
#[must_use]
pub fn bench_derive_config() -> IngestDeriveConfig {
    IngestDeriveConfig {
        replay_batch_blocks: nz32(BENCH_DERIVE_REPLAY_BATCH_BLOCKS),
        replay_policy: DeriveReplayPolicy::CanonicalFirst,
        memory_budget_bytes: None,
        memory_degrade_ratio: BENCH_DERIVE_MEMORY_DEGRADE_RATIO,
        memory_pause_ratio: BENCH_DERIVE_MEMORY_PAUSE_RATIO,
        memory_resume_ratio: BENCH_DERIVE_MEMORY_RESUME_RATIO,
        min_replay_batch_blocks: nz32(BENCH_DERIVE_MIN_REPLAY_BATCH_BLOCKS),
        startup_handoff_lag_blocks: BENCH_DERIVE_STARTUP_HANDOFF_LAG_BLOCKS,
    }
}
