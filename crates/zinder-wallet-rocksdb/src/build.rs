//! Fresh wallet construction from one authenticated canonical replay scan.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use zinder_core::CanonicalBlockFactsSequenceDigest;
use zinder_rocksdb::VariableValueSortEvidence;
use zinder_store::{RocksDbCanonicalStore, RocksDbResourceBudget};
use zinder_wallet_projection::{
    WalletCanonicalSourceIdentity, WalletProjectionDigest, WalletProjectionFamilyRowCounts,
    WalletProjectionReadyEvidence, WalletProjectionSourcePosition, WalletUtxoSetSummary,
};

use crate::{
    RocksDbWalletError, RocksDbWalletStore,
    projection_load::{PreparedWalletProjectionLoad, ProjectionLoadConfig, write_projection_ssts},
    store::{RocksDbWalletBuilder, WalletColdValidationConfig},
};

/// Explicit resource limits for one production external wallet build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RocksDbWalletBuildOptions {
    /// Bounded resources applied to the wallet `RocksDB` instance.
    pub resource_budget: RocksDbResourceBudget,
    /// Accounted memory ceiling for the outpoint event sorter.
    pub max_outpoint_sort_memory_bytes: u64,
    /// Accounted memory ceiling applied independently to each secondary sorter.
    pub max_secondary_sort_memory_bytes_per_sorter: u64,
    /// Temporary-run byte ceiling applied independently to each external sorter.
    pub max_temporary_file_bytes_per_sorter: u64,
    /// Target logical key-and-value bytes per generated SST file.
    pub sst_target_logical_bytes: u64,
    /// Accounted memory ceiling for the retained reorg-undo suffix.
    pub max_accounted_reorg_undo_bytes: u64,
    /// Number of exact tip undo records retained for reorg handling.
    pub supported_reorg_depth: u32,
}

impl RocksDbWalletBuildOptions {
    /// Small deterministic limits for unit and deployment-smoke fixtures.
    #[must_use]
    pub const fn for_local_tests() -> Self {
        Self {
            resource_budget: RocksDbResourceBudget::for_local_tests(),
            max_outpoint_sort_memory_bytes: 16 * 1024 * 1024,
            max_secondary_sort_memory_bytes_per_sorter: 16 * 1024 * 1024,
            max_temporary_file_bytes_per_sorter: 256 * 1024 * 1024,
            sst_target_logical_bytes: 1024 * 1024,
            max_accounted_reorg_undo_bytes: 16 * 1024 * 1024,
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
    /// External run finalization and merge for outpoint events.
    pub outpoint_sort: Duration,
    /// Ordered output/spend merge and primary-family SST writes.
    pub outpoint_merge: Duration,
    /// Secondary sorting, deduplication, balances, history, and retained undo.
    pub secondary_row_derivation: Duration,
    /// Row-count and interleavable version-1 digest finalization.
    pub logical_evidence: Duration,
    /// External SST ingestion into the unpublished BUILDING store.
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
    /// Canonical transactions inspected by the replay scan.
    pub scanned_transaction_count: u64,
    /// Transparent outputs staged for the outpoint merge.
    pub staged_output_count: u64,
    /// Non-coinbase transparent spends staged for the outpoint merge.
    pub staged_spend_count: u64,
    /// Historical canonical-store prevout reads; version 1 requires zero.
    pub historical_prevout_read_count: u64,
    /// Exact bounded-work evidence for the outpoint event sorter.
    pub outpoint_sort: VariableValueSortEvidence,
    /// Exact bounded-work evidence for the address-unspent secondary sorter.
    pub address_index_sort: VariableValueSortEvidence,
    /// Exact bounded-work evidence for the address-history secondary sorter.
    pub address_transaction_sort: VariableValueSortEvidence,
    /// Highest explicitly accounted retained reorg-undo footprint.
    pub peak_accounted_reorg_undo_bytes: u64,
    /// Caller-supplied retained reorg-undo ceiling.
    pub max_accounted_reorg_undo_bytes: u64,
    /// Exact bounded-work evidence for the cold-validation address-index sorter.
    pub cold_validation_address_index_sort: VariableValueSortEvidence,
    /// Exact bounded-work evidence for the cold-validation address-history sorter.
    pub cold_validation_address_transaction_sort: VariableValueSortEvidence,
    /// Highest accounted cold-validation reorg-undo suffix footprint.
    pub cold_validation_peak_accounted_reorg_undo_bytes: u64,
    /// Caller-supplied cold-validation reorg-undo suffix ceiling.
    pub cold_validation_max_accounted_reorg_undo_bytes: u64,
    /// Logical durable key and value bytes emitted across all six families.
    pub logical_row_bytes: u64,
    /// Physical bytes occupied by all generated SST files before ingestion.
    pub sst_file_bytes: u64,
    /// Number of generated SST files ingested into the BUILDING store.
    pub sst_file_count: u64,
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
/// The target path and its deterministic sibling staging path must not exist.
/// Any failure leaves only a non-queryable BUILDING target; staging artifacts
/// owned by this invocation are removed on every return path.
#[allow(
    clippy::too_many_lines,
    reason = "the public orchestrator keeps phase timing and publication evidence in one order"
)]
pub fn build_wallet_from_canonical(
    canonical_store: &RocksDbCanonicalStore,
    wallet_path: impl AsRef<Path>,
    options: RocksDbWalletBuildOptions,
) -> Result<RocksDbWalletBuildOutcome, RocksDbWalletError> {
    let total_started = Instant::now();
    let wallet_path = wallet_path.as_ref();
    let staging = FreshWalletProjectionStaging::create(projection_staging_path(wallet_path))?;
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

    let data_options = builder.data_options();
    let mut prepared = write_projection_ssts(
        ProjectionLoadConfig {
            staging_path: staging.path(),
            options: &data_options,
            network: canonical_store.network(),
            supported_reorg_depth: options.supported_reorg_depth,
            max_outpoint_sort_memory_bytes: options.max_outpoint_sort_memory_bytes,
            max_secondary_sort_memory_bytes_per_sorter: options
                .max_secondary_sort_memory_bytes_per_sorter,
            max_temporary_file_bytes_per_sorter: options.max_temporary_file_bytes_per_sorter,
            sst_target_logical_bytes: options.sst_target_logical_bytes,
            max_accounted_reorg_undo_bytes: options.max_accounted_reorg_undo_bytes,
        },
        canonical_store
            .scan_canonical_replay()
            .map_err(|source| RocksDbWalletError::CanonicalReplay { source })?,
    )?;
    validate_canonical_fence(&prepared, canonical_ready)?;

    let ready_evidence = WalletProjectionReadyEvidence {
        source_position,
        source_sequence_digest: prepared.source_sequence_digest,
        projection_digest: prepared.projection_digest,
        row_counts: prepared.row_counts,
        utxo_summary: prepared.utxo_summary.clone(),
    };
    let counters = prepared.counters;
    let load_durations = prepared.phase_durations;
    let outpoint_sort = prepared.outpoint_sort_evidence;
    let address_index_sort = prepared.address_index_sort_evidence;
    let address_transaction_sort = prepared.address_transaction_sort_evidence;
    let logical_row_bytes = prepared.logical_row_bytes;
    let sst_file_bytes = prepared.sst_file_bytes;
    let sst_file_count = prepared.sst_file_count;

    let phase_started = Instant::now();
    builder.ingest_projection_ssts(&mut prepared)?;
    let row_load = phase_started.elapsed();

    let phase_started = Instant::now();
    let cold_build = builder.reopen_for_validation()?;
    let flush_and_cold_reopen = phase_started.elapsed();

    let phase_started = Instant::now();
    let validated = cold_build.validate_rows(
        ready_evidence,
        WalletColdValidationConfig {
            staging_path: staging.path(),
            max_sort_memory_bytes_per_sorter: options.max_secondary_sort_memory_bytes_per_sorter,
            max_temporary_file_bytes_per_sorter: options.max_temporary_file_bytes_per_sorter,
            max_accounted_reorg_undo_bytes: options.max_accounted_reorg_undo_bytes,
        },
    )?;
    let cold_validation = phase_started.elapsed();
    let validation_evidence = validated.validation_evidence();
    staging.remove()?;

    let phase_started = Instant::now();
    let store = validated.publish_ready()?;
    let ready_publication = phase_started.elapsed();
    let total = total_started.elapsed();
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
        outpoint_sort,
        address_index_sort,
        address_transaction_sort,
        peak_accounted_reorg_undo_bytes: counters.peak_accounted_reorg_undo_bytes,
        max_accounted_reorg_undo_bytes: counters.max_accounted_reorg_undo_bytes,
        cold_validation_address_index_sort: validation_evidence.address_index_sort,
        cold_validation_address_transaction_sort: validation_evidence.address_transaction_sort,
        cold_validation_peak_accounted_reorg_undo_bytes: validation_evidence
            .peak_accounted_reorg_undo_bytes,
        cold_validation_max_accounted_reorg_undo_bytes: validation_evidence
            .max_accounted_reorg_undo_bytes,
        logical_row_bytes,
        sst_file_bytes,
        sst_file_count,
        cold_validation_random_read_count: validation_evidence.random_read_count,
        phase_durations: WalletBuildPhaseDurations {
            store_initialization,
            canonical_scan: load_durations.canonical_scan,
            outpoint_sort: load_durations.outpoint_sort,
            outpoint_merge: load_durations.outpoint_merge,
            secondary_row_derivation: load_durations.secondary_row_derivation,
            logical_evidence: load_durations.logical_evidence,
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
    prepared: &PreparedWalletProjectionLoad,
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

fn projection_staging_path(wallet_path: &Path) -> PathBuf {
    let file_name = wallet_path.file_name().unwrap_or(wallet_path.as_os_str());
    let mut staging_file_name = OsString::from(file_name);
    staging_file_name.push(".projection-load-staging");
    wallet_path.with_file_name(staging_file_name)
}

struct FreshWalletProjectionStaging {
    path: PathBuf,
    remove_on_drop: bool,
}

impl FreshWalletProjectionStaging {
    fn create(path: PathBuf) -> Result<Self, RocksDbWalletError> {
        match fs::create_dir(&path) {
            Ok(()) => Ok(Self {
                path,
                remove_on_drop: true,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(RocksDbWalletError::ProjectionStagingPathNotFresh { path })
            }
            Err(source) => Err(RocksDbWalletError::PathUnavailable { path, source }),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn remove(mut self) -> Result<(), RocksDbWalletError> {
        fs::remove_dir_all(&self.path).map_err(|source| RocksDbWalletError::PathUnavailable {
            path: self.path.clone(),
            source,
        })?;
        self.remove_on_drop = false;
        Ok(())
    }
}

impl Drop for FreshWalletProjectionStaging {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_staging_path_is_a_sibling_for_trailing_separators() {
        assert_eq!(
            projection_staging_path(Path::new("/var/lib/zinder/wallet/")),
            Path::new("/var/lib/zinder/wallet.projection-load-staging")
        );
    }
}
