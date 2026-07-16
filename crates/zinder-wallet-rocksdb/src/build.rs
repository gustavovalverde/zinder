//! Fresh wallet construction from one authenticated canonical replay scan.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use zinder_core::CanonicalBlockFactsSequenceDigest;
use zinder_store::{RocksDbCanonicalStore, RocksDbResourceBudget};
use zinder_wallet_projection::{
    WalletCanonicalSourceIdentity, WalletProjectionDigest, WalletProjectionFamilyRowCounts,
    WalletProjectionReadyEvidence, WalletProjectionSourcePosition, WalletUtxoSetSummary,
};

use crate::{
    RocksDbWalletError, RocksDbWalletStore,
    sort_merge::{WalletSortMergeError, prepare_wallet_projection},
    store::RocksDbWalletBuilder,
};

/// Explicit resource limits for the bounded in-memory wallet build tracer.
///
/// This tracer proves the complete logical and physical lifecycle on bounded
/// histories. It fails before its accounted preparation limit instead of
/// silently growing to full-chain memory. Mainnet construction will replace
/// the preparation stage with disk-backed external runs while preserving this
/// store and publication contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RocksDbWalletBuildOptions {
    /// Bounded resources applied to the wallet `RocksDB` instance.
    pub resource_budget: RocksDbResourceBudget,
    /// Hard ceiling for explicitly accounted preparation rows and payloads.
    pub max_accounted_preparation_bytes: u64,
    /// Hard ceiling for explicitly accounted retained relationship keys and values.
    pub max_accounted_validation_relation_bytes: u64,
    /// Hard logical key-and-value byte ceiling for one WAL-free write batch.
    pub max_write_batch_bytes: u64,
    /// Number of exact tip undo records retained for reorg handling.
    pub supported_reorg_depth: u32,
}

impl RocksDbWalletBuildOptions {
    /// Small deterministic limits for unit and deployment-smoke fixtures.
    #[must_use]
    pub const fn for_local_tests() -> Self {
        Self {
            resource_budget: RocksDbResourceBudget::for_local_tests(),
            max_accounted_preparation_bytes: 16 * 1024 * 1024,
            max_accounted_validation_relation_bytes: 16 * 1024 * 1024,
            max_write_batch_bytes: 1024 * 1024,
            supported_reorg_depth: 100,
        }
    }
}

/// Wall-clock duration of each observable fresh-build phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalletBuildPhaseDurations {
    /// Fresh path creation and durable BUILDING publication.
    pub store_initialization: Duration,
    /// Authenticated canonical replay scan and event staging.
    pub canonical_scan: Duration,
    /// Sorting and duplicate validation of output and spend events.
    pub outpoint_sort: Duration,
    /// Ordered output/spend merge into current and historical output state.
    pub outpoint_merge: Duration,
    /// Address, balance, and retained-undo row derivation.
    pub secondary_row_derivation: Duration,
    /// UTXO summary, row counts, and deterministic projection digest.
    pub logical_evidence: Duration,
    /// Bounded WAL-free writes of all six logical families.
    pub row_load: Duration,
    /// Blocking flush, close, and cold BUILDING reopen.
    pub flush_and_cold_reopen: Duration,
    /// Full semantic readback against the candidate READY evidence.
    pub cold_validation: Duration,
    /// Synchronous READY control publication and readback.
    pub ready_publication: Duration,
    /// Complete call duration.
    pub total: Duration,
}

/// Measured evidence from one completed fixed-tip wallet build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RocksDbWalletBuildReport {
    /// Exact canonical epoch, tip, and event sequence represented by the store.
    pub source_position: WalletProjectionSourcePosition,
    /// Authenticated digest of every canonical facts row through the source tip.
    pub source_sequence_digest: CanonicalBlockFactsSequenceDigest,
    /// Digest of all six logical wallet row families.
    pub projection_digest: WalletProjectionDigest,
    /// Exact row counts published by READY.
    pub row_counts: WalletProjectionFamilyRowCounts,
    /// Exact current transparent UTXO aggregate.
    pub utxo_summary: WalletUtxoSetSummary,
    /// Canonical blocks consumed by the single replay scan.
    pub scanned_block_count: u64,
    /// Canonical transactions inspected during preparation.
    pub scanned_transaction_count: u64,
    /// Transparent outputs staged for the outpoint merge.
    pub staged_output_count: u64,
    /// Non-coinbase transparent spends staged for the outpoint merge.
    pub staged_spend_count: u64,
    /// Historical canonical-store prevout reads; version 1 requires zero.
    pub historical_prevout_read_count: u64,
    /// Highest explicitly accounted preparation footprint.
    pub peak_accounted_preparation_bytes: u64,
    /// Caller-supplied preparation ceiling used by this build.
    pub max_accounted_preparation_bytes: u64,
    /// Highest explicitly accounted retained relationship key and value bytes.
    pub peak_accounted_validation_relation_bytes: u64,
    /// Caller-supplied ceiling for the accounted retained relationship bytes.
    pub max_accounted_validation_relation_bytes: u64,
    /// Logical durable key and value bytes written to `RocksDB`.
    pub logical_row_bytes: u64,
    /// Number of bounded WAL-free data write batches.
    pub write_batch_count: u64,
    /// Random point reads performed by cold cross-family validation.
    pub cold_validation_random_read_count: u64,
    /// Phase-level wall-clock timings.
    pub phase_durations: WalletBuildPhaseDurations,
}

impl RocksDbWalletBuildReport {
    /// Returns the exact source identity required to reopen this wallet store.
    #[must_use]
    pub const fn canonical_source_identity(&self) -> WalletCanonicalSourceIdentity {
        WalletCanonicalSourceIdentity::new(self.source_position, self.source_sequence_digest)
    }
}

/// Ready store and its measured fresh-build evidence.
pub struct RocksDbWalletBuildOutcome {
    /// Admitted READY wallet store, usable immediately by the caller.
    pub store: RocksDbWalletStore,
    /// Exact logical, physical, and timing evidence from construction.
    pub report: RocksDbWalletBuildReport,
}

/// Builds and publishes a fresh version-1 wallet store at one canonical fence.
///
/// The target path must not exist. Any failure after initialization deliberately
/// leaves a non-queryable BUILDING store for diagnosis and explicit cleanup.
#[allow(
    clippy::too_many_lines,
    reason = "the public orchestrator keeps phase timing and report evidence in one visible order"
)]
pub fn build_wallet_from_canonical(
    canonical_store: &RocksDbCanonicalStore,
    wallet_path: impl AsRef<Path>,
    options: RocksDbWalletBuildOptions,
) -> Result<RocksDbWalletBuildOutcome, RocksDbWalletError> {
    let total_started = Instant::now();
    let canonical_ready = canonical_store.ready_evidence();
    let source_position = WalletProjectionSourcePosition::new(
        canonical_ready.visible_epoch,
        canonical_ready.visible_tip,
        canonical_ready.visible_event_sequence,
    );

    let phase_started = Instant::now();
    let builder = RocksDbWalletBuilder::create_fresh(
        wallet_path,
        canonical_store.network(),
        source_position,
        options.supported_reorg_depth,
        options.resource_budget,
    )?;
    let store_initialization = phase_started.elapsed();

    let prepared = prepare_wallet_projection(
        canonical_store.network(),
        options.supported_reorg_depth,
        options.max_accounted_preparation_bytes,
        canonical_store
            .scan_canonical_replay()
            .map_err(|source| RocksDbWalletError::CanonicalReplay { source })?,
    )
    .map_err(map_sort_merge_error)?;
    validate_canonical_fence(&prepared, canonical_ready)?;

    let phase_started = Instant::now();
    let load_evidence = builder.load_prepared(&prepared, options.max_write_batch_bytes)?;
    let row_load = phase_started.elapsed();

    let ready_evidence = WalletProjectionReadyEvidence {
        source_position,
        source_sequence_digest: prepared.source_sequence_digest,
        projection_digest: prepared.projection_digest,
        row_counts: prepared.row_counts,
        utxo_summary: prepared.utxo_summary.clone(),
    };

    let phase_started = Instant::now();
    let cold_build = builder.reopen_for_validation()?;
    let flush_and_cold_reopen = phase_started.elapsed();

    let phase_started = Instant::now();
    let validated = cold_build.validate_rows(
        ready_evidence,
        options.max_accounted_validation_relation_bytes,
    )?;
    let cold_validation = phase_started.elapsed();

    let validation_evidence = validated.validation_evidence();
    let phase_started = Instant::now();
    let store = validated.publish_ready()?;
    let ready_publication = phase_started.elapsed();
    let total = total_started.elapsed();

    let counters = prepared.counters;
    let preparation_durations = prepared.phase_durations;
    let report = RocksDbWalletBuildReport {
        source_position,
        source_sequence_digest: prepared.source_sequence_digest,
        projection_digest: prepared.projection_digest,
        row_counts: prepared.row_counts,
        utxo_summary: prepared.utxo_summary,
        scanned_block_count: counters.scanned_block_count,
        scanned_transaction_count: counters.scanned_transaction_count,
        staged_output_count: counters.staged_output_count,
        staged_spend_count: counters.staged_spend_count,
        historical_prevout_read_count: counters.historical_prevout_read_count,
        peak_accounted_preparation_bytes: counters.peak_accounted_bytes,
        max_accounted_preparation_bytes: counters.max_accounted_bytes,
        peak_accounted_validation_relation_bytes: validation_evidence.peak_accounted_bytes,
        max_accounted_validation_relation_bytes: options.max_accounted_validation_relation_bytes,
        logical_row_bytes: load_evidence.logical_row_bytes,
        write_batch_count: load_evidence.write_batch_count,
        cold_validation_random_read_count: validation_evidence.random_read_count,
        phase_durations: WalletBuildPhaseDurations {
            store_initialization,
            canonical_scan: preparation_durations.canonical_scan,
            outpoint_sort: preparation_durations.outpoint_sort,
            outpoint_merge: preparation_durations.outpoint_merge,
            secondary_row_derivation: preparation_durations.secondary_row_derivation,
            logical_evidence: preparation_durations.logical_evidence,
            row_load,
            flush_and_cold_reopen,
            cold_validation,
            ready_publication,
            total,
        },
    };
    Ok(RocksDbWalletBuildOutcome { store, report })
}

fn validate_canonical_fence(
    prepared: &crate::sort_merge::PreparedWalletProjection,
    ready: zinder_store::CanonicalStoreReadyEvidence,
) -> Result<(), RocksDbWalletError> {
    let expected_sequence_digest =
        CanonicalBlockFactsSequenceDigest::from_admitted_checkpoint_parts(
            ready.sequence_digest_version,
            ready.baseline_block_count,
            ready.baseline_sequence_digest,
        );
    if prepared.first_block != ready.first_retained_block {
        return Err(RocksDbWalletError::CanonicalSourceFenceMismatch {
            reason: "prepared first block differs from canonical READY",
        });
    }
    if prepared.tip != ready.visible_tip {
        return Err(RocksDbWalletError::CanonicalSourceFenceMismatch {
            reason: "prepared tip differs from canonical READY",
        });
    }
    if prepared.counters.scanned_block_count != ready.baseline_block_count {
        return Err(RocksDbWalletError::CanonicalSourceFenceMismatch {
            reason: "prepared block count differs from canonical READY",
        });
    }
    if prepared.source_sequence_digest != expected_sequence_digest {
        return Err(RocksDbWalletError::CanonicalSourceFenceMismatch {
            reason: "prepared sequence digest differs from canonical READY",
        });
    }
    Ok(())
}

fn map_sort_merge_error(error: WalletSortMergeError) -> RocksDbWalletError {
    match error {
        WalletSortMergeError::Contract(source) => RocksDbWalletError::Contract(source),
        WalletSortMergeError::EmptyCanonicalHistory => RocksDbWalletError::EmptyCanonicalHistory,
        WalletSortMergeError::SourceSequenceLength(source) => source.into(),
        WalletSortMergeError::CanonicalScan(source) => {
            RocksDbWalletError::CanonicalReplay { source }
        }
        WalletSortMergeError::AccountedMemoryLimit {
            limit_bytes,
            required_bytes,
        } => RocksDbWalletError::AccountedMemoryLimit {
            limit_bytes,
            required_bytes,
        },
        WalletSortMergeError::CounterOverflow => RocksDbWalletError::BuildCounterOverflow,
    }
}
