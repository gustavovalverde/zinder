//! Compact-block range measurements through the admitted canonical secondary API.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};
use zinder_bench::{BenchError, fixture::FixtureManifest};
use zinder_core::{BlockHeight, BlockHeightRange, CompactBlockArtifact};
use zinder_store::{
    CanonicalReorgPolicy, CanonicalStoreReadyEvidence, CanonicalStoreWorkload,
    RocksDbCanonicalSecondary, RocksDbResourceBudget,
};

const RANGE_BLOCK_COUNTS: [u32; 3] = [128, 512, 1_024];
const DEFAULT_SUPPORTED_REORG_DEPTH: u32 = 100;
const OUTPUT_DIGEST_DOMAIN: &[u8] = b"zinder-bench-compact-block-range-output-v1\0";

/// CLI contract for measuring compact-block range reads from one READY store.
#[derive(Args)]
pub(crate) struct RocksDbCompactBlockRangeArgs {
    /// Directory containing the fixture manifest used to construct the store.
    #[arg(long)]
    fixture: PathBuf,
    /// Existing READY canonical-v1 primary store.
    #[arg(long = "canonical-store")]
    canonical_store: PathBuf,
    /// Absent directory used for process-local secondary metadata.
    #[arg(long = "secondary-root")]
    secondary_root: PathBuf,
    /// Exact source revision of the measured binary.
    #[arg(long = "software-revision")]
    software_revision: String,
    /// Persisted canonical replacement depth.
    #[arg(long, default_value_t = DEFAULT_SUPPORTED_REORG_DEPTH)]
    supported_reorg_depth: u32,
    /// Write the JSON report to this path instead of stdout.
    #[arg(long)]
    report: Option<PathBuf>,
}

/// Report and optional output path produced by compact-block measurements.
pub(crate) struct RocksDbCompactBlockRangeOutput {
    pub(crate) report: RocksDbCompactBlockRangeReport,
    pub(crate) report_path: Option<PathBuf>,
}

/// Exact benchmark evidence for all requested compact-block ranges.
#[derive(Serialize)]
pub(crate) struct RocksDbCompactBlockRangeReport {
    contract_identity: &'static str,
    report_format_version: u16,
    software_revision: String,
    fixture_manifest_digest_sha256: String,
    canonical_store: String,
    ready_fence: ReadyFenceSummary,
    cases: Vec<CompactBlockRangeCase>,
}

#[derive(Serialize)]
struct ReadyFenceSummary {
    first_retained_height: u32,
    visible_tip_height: u32,
    visible_tip_hash_hex: String,
    visible_epoch_id: u64,
    visible_event_sequence: u64,
    settled_tip_height: u32,
    settled_tip_hash_hex: String,
    visible_sequence_digest_sha256: String,
}

#[derive(Serialize)]
struct CompactBlockRangeCase {
    requested_block_count: u32,
    range_start_height: u32,
    range_end_height: u32,
    fresh_secondary_open_seconds: f64,
    cold: CompactBlockRangeMeasurement,
    warm: CompactBlockRangeMeasurement,
}

#[derive(Serialize)]
struct CompactBlockRangeMeasurement {
    seconds: f64,
    blocks_per_second: f64,
    output: CompactBlockRangeOutputEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CompactBlockRangeOutputEvidence {
    block_count: u32,
    payload_bytes: u64,
    digest_sha256: String,
}

/// Measures nested ranges ending at one immutable admitted READY tip.
pub(crate) fn run_rocksdb_compact_block_range(
    args: RocksDbCompactBlockRangeArgs,
) -> Result<RocksDbCompactBlockRangeOutput, BenchError> {
    if args.software_revision.trim().is_empty() {
        return Err(BenchError::invalid_argument(
            "--software-revision must not be empty",
        ));
    }
    let manifest = FixtureManifest::read(&args.fixture)?;
    let activations = manifest.activations_typed()?;
    let reorg_policy = CanonicalReorgPolicy::new(args.supported_reorg_depth)?;
    let canonical_store = fs::canonicalize(&args.canonical_store)
        .map_err(|source| BenchError::io(&args.canonical_store, source))?;
    prepare_secondary_root(&args.secondary_root, &canonical_store)?;

    let mut admitted_ready = None;
    let mut cases = Vec::with_capacity(RANGE_BLOCK_COUNTS.len());
    for block_count in RANGE_BLOCK_COUNTS {
        let secondary_path = args.secondary_root.join(format!("range-{block_count}"));
        let open_started = Instant::now();
        let secondary = RocksDbCanonicalSecondary::open_ready(
            &canonical_store,
            &secondary_path,
            &activations,
            CanonicalStoreWorkload::Wallet,
            reorg_policy,
            RocksDbResourceBudget::canonical_reader_defaults(),
        )?;
        let open_elapsed = open_started.elapsed();
        let ready = secondary.ready_evidence();
        require_same_ready_fence(admitted_ready.as_ref(), &ready)?;
        admitted_ready = Some(ready);

        let range = range_ending_at_ready_tip(&ready, block_count)?;
        let cold = measure_range(&secondary, range, block_count)?;
        let warm = measure_range(&secondary, range, block_count)?;
        if cold.output != warm.output {
            return Err(BenchError::report_format(format!(
                "cold and warm compact-block outputs differ for {block_count} blocks"
            )));
        }
        cases.push(CompactBlockRangeCase {
            requested_block_count: block_count,
            range_start_height: range.start.value(),
            range_end_height: range.end.value(),
            fresh_secondary_open_seconds: elapsed_seconds(open_elapsed),
            cold,
            warm,
        });
    }

    let ready = admitted_ready.ok_or_else(|| {
        BenchError::report_format("compact-block benchmark produced no admitted READY fence")
    })?;
    let report = RocksDbCompactBlockRangeReport {
        contract_identity: "rocksdb-compact-block-range",
        report_format_version: 1,
        software_revision: args.software_revision,
        fixture_manifest_digest_sha256: manifest.digest_sha256()?,
        canonical_store: canonical_store.to_string_lossy().into_owned(),
        ready_fence: ReadyFenceSummary::from(ready),
        cases,
    };
    Ok(RocksDbCompactBlockRangeOutput {
        report,
        report_path: args.report,
    })
}

fn prepare_secondary_root(
    secondary_root: &std::path::Path,
    canonical_store: &std::path::Path,
) -> Result<(), BenchError> {
    if secondary_root.exists() {
        return Err(BenchError::invalid_argument(
            "--secondary-root must name an absent directory",
        ));
    }
    let parent = secondary_root.parent().ok_or_else(|| {
        BenchError::invalid_argument("--secondary-root must have an existing parent directory")
    })?;
    let parent = fs::canonicalize(parent).map_err(|source| BenchError::io(parent, source))?;
    let resolved = parent.join(
        secondary_root
            .file_name()
            .ok_or_else(|| BenchError::invalid_argument("--secondary-root lacks a file name"))?,
    );
    if resolved.starts_with(canonical_store) || canonical_store.starts_with(&resolved) {
        return Err(BenchError::invalid_argument(
            "--secondary-root must be disjoint from --canonical-store",
        ));
    }
    fs::create_dir(&resolved).map_err(|source| BenchError::io(&resolved, source))
}

fn require_same_ready_fence(
    expected: Option<&CanonicalStoreReadyEvidence>,
    observed: &CanonicalStoreReadyEvidence,
) -> Result<(), BenchError> {
    if expected.is_some_and(|expected| expected != observed) {
        return Err(BenchError::report_format(
            "canonical READY fence changed between compact-block cases",
        ));
    }
    Ok(())
}

fn range_ending_at_ready_tip(
    ready: &CanonicalStoreReadyEvidence,
    block_count: u32,
) -> Result<BlockHeightRange, BenchError> {
    let start = ready
        .visible_tip
        .height
        .value()
        .checked_add(1)
        .and_then(|exclusive_end| exclusive_end.checked_sub(block_count))
        .ok_or_else(|| BenchError::invalid_argument("requested range underflows block height"))?;
    if start < ready.first_retained_block.height.value() {
        return Err(BenchError::invalid_argument(format!(
            "requested {block_count}-block range starts before retained canonical history"
        )));
    }
    Ok(BlockHeightRange::inclusive(
        BlockHeight::new(start),
        ready.visible_tip.height,
    ))
}

fn measure_range(
    secondary: &RocksDbCanonicalSecondary,
    range: BlockHeightRange,
    expected_block_count: u32,
) -> Result<CompactBlockRangeMeasurement, BenchError> {
    let started = Instant::now();
    let blocks = secondary.compact_blocks_in_range(range)?;
    let seconds = elapsed_seconds(started.elapsed());
    let output = compact_block_range_output(&blocks, range, expected_block_count)?;
    Ok(CompactBlockRangeMeasurement {
        seconds,
        blocks_per_second: f64::from(expected_block_count) / seconds,
        output,
    })
}

fn compact_block_range_output(
    blocks: &[CompactBlockArtifact],
    range: BlockHeightRange,
    expected_block_count: u32,
) -> Result<CompactBlockRangeOutputEvidence, BenchError> {
    let observed_count = u32::try_from(blocks.len())
        .map_err(|_| BenchError::report_format("compact-block range exceeds u32::MAX rows"))?;
    if observed_count != expected_block_count {
        return Err(BenchError::report_format(format!(
            "compact-block range returned {observed_count} rows; expected {expected_block_count}"
        )));
    }

    let mut hasher = Sha256::new();
    hasher.update(OUTPUT_DIGEST_DOMAIN);
    let mut payload_bytes = 0_u64;
    for (height, block) in range.into_iter().zip(blocks) {
        if block.height != height {
            return Err(BenchError::report_format(format!(
                "compact-block range returned height {} where {} was expected",
                block.height.value(),
                height.value()
            )));
        }
        let payload_len = u64::try_from(block.payload_bytes.len()).map_err(|_| {
            BenchError::report_format("compact-block payload length exceeds u64::MAX")
        })?;
        payload_bytes = payload_bytes.checked_add(payload_len).ok_or_else(|| {
            BenchError::report_format("compact-block payload byte total exceeds u64::MAX")
        })?;
        hasher.update(block.height.value().to_be_bytes());
        hasher.update(block.block_hash.as_bytes());
        hasher.update(payload_len.to_be_bytes());
        hasher.update(&block.payload_bytes);
    }

    Ok(CompactBlockRangeOutputEvidence {
        block_count: observed_count,
        payload_bytes,
        digest_sha256: hex::encode(hasher.finalize()),
    })
}

fn elapsed_seconds(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64().max(f64::EPSILON)
}

impl From<CanonicalStoreReadyEvidence> for ReadyFenceSummary {
    fn from(ready: CanonicalStoreReadyEvidence) -> Self {
        let settled = ready.sequence_checkpoint.through();
        Self {
            first_retained_height: ready.first_retained_block.height.value(),
            visible_tip_height: ready.visible_tip.height.value(),
            visible_tip_hash_hex: hex::encode(ready.visible_tip.hash.as_bytes()),
            visible_epoch_id: ready.visible_epoch.value(),
            visible_event_sequence: ready.visible_event_sequence,
            settled_tip_height: settled.height.value(),
            settled_tip_hash_hex: hex::encode(settled.hash.as_bytes()),
            visible_sequence_digest_sha256: hex::encode(ready.visible_sequence_digest),
        }
    }
}

#[cfg(test)]
mod tests {
    use zinder_core::{BlockHash, CompactBlockArtifact};

    use super::{BlockHeight, BlockHeightRange, compact_block_range_output};

    #[test]
    fn output_digest_binds_order_height_hash_and_payload() -> Result<(), Box<dyn std::error::Error>>
    {
        let first =
            CompactBlockArtifact::new(BlockHeight::new(10), BlockHash::from_bytes([1; 32]), [2, 3]);
        let second = CompactBlockArtifact::new(
            BlockHeight::new(11),
            BlockHash::from_bytes([4; 32]),
            [5, 6, 7],
        );
        let range = BlockHeightRange::inclusive(BlockHeight::new(10), BlockHeight::new(11));

        let evidence = compact_block_range_output(&[first.clone(), second.clone()], range, 2)?;
        let repeated = compact_block_range_output(&[first, second.clone()], range, 2)?;
        let changed = compact_block_range_output(
            &[
                CompactBlockArtifact::new(
                    BlockHeight::new(10),
                    BlockHash::from_bytes([1; 32]),
                    [2, 3],
                ),
                CompactBlockArtifact::new(BlockHeight::new(11), second.block_hash, [5, 6, 8]),
            ],
            range,
            2,
        )?;

        assert_eq!(evidence, repeated);
        assert_ne!(evidence, changed);
        assert_eq!(evidence.payload_bytes, 5);
        Ok(())
    }
}
