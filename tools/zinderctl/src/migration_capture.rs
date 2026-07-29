//! Fixed-range capture: pull raw source payloads from a live Zebra node and
//! write them into a deterministic migration archive directory.

use std::{
    future::Future,
    num::{NonZeroU32, NonZeroU64},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    migration_archive::{
        ActivationRecord, CanonicalBlockFactsDigestEvidence, MIGRATION_ARCHIVE_FORMAT_VERSION,
        MIGRATION_ARCHIVE_IDENTITY, MigrationArchiveManifest, SegmentDescriptor, SubtreeRootRecord,
        SubtreeRootSet, WorkloadDensity, write_segment,
    },
    migration_error::MigrationError,
};
use futures_util::StreamExt as _;
use zinder_core::{
    BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion,
    CanonicalBlockFactsSequenceDigest, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, Network, NetworkUpgradeActivations, ShieldedProtocol,
    SubtreeRootIndex, wire::encode_zinder_native_chain_name,
};
use zinder_ingest::{RawBlobPolicy, prepare_canonical_block};
use zinder_source::{
    NodeAuth, NodeSource, SourceBlock, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};

const SUBTREE_ROOT_PAGE_SIZE: u32 = 1_000;

/// Inputs for one fixed-range capture.
#[derive(Clone, Debug)]
pub(crate) struct CaptureConfig {
    /// Network the node answers for.
    pub(crate) network: Network,
    /// Node JSON-RPC base URL.
    pub(crate) json_rpc_addr: String,
    /// Node authentication mode.
    pub(crate) node_auth: NodeAuth,
    /// First block height to capture.
    pub(crate) from_height: BlockHeight,
    /// Last block height to capture.
    pub(crate) to_height: BlockHeight,
    /// Physical canonical schema that supplied the export fence.
    pub(crate) source_canonical_schema_version: u16,
    /// Blocks written per segment file.
    pub(crate) segment_blocks: NonZeroU32,
    /// Concurrent block fetches issued against the node.
    pub(crate) fetch_concurrency: NonZeroU32,
    /// Concurrent block-local preparations used to measure captured blocks.
    pub(crate) prepare_concurrency: NonZeroU32,
    /// Per-request source timeout.
    pub(crate) request_timeout: Duration,
    /// Maximum JSON-RPC response body size.
    pub(crate) max_response_bytes: NonZeroU64,
    /// Destination migration archive directory.
    pub(crate) output_directory: PathBuf,
}

/// Captures the configured block range into a migration archive directory and returns the
/// manifest that was written.
pub(crate) async fn capture_fixed_range(
    config: CaptureConfig,
) -> Result<MigrationArchiveManifest, MigrationError> {
    if config.from_height.value() > config.to_height.value() {
        return Err(MigrationError::invalid_argument(
            "from_height must be less than or equal to to_height",
        ));
    }
    match std::fs::create_dir(&config.output_directory) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(MigrationError::invalid_argument(format!(
                "migration archive output must be absent: {}",
                config.output_directory.display()
            )));
        }
        Err(source) => return Err(MigrationError::io(&config.output_directory, source)),
    }

    let source = ZebraJsonRpcSource::with_options(
        config.network,
        &config.json_rpc_addr,
        config.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: config.request_timeout,
            max_response_bytes: config.max_response_bytes,
            broadcast_timeout: None,
        },
    )?;

    let activations = source
        .discover_network_upgrade_activations("zinderctl")
        .await?;
    let activation_records = activations
        .activations()
        .iter()
        .map(|activation| ActivationRecord {
            branch_id: activation.branch_id.value(),
            activation_height: activation.activation_height.value(),
            name: activation.name.clone(),
        })
        .collect();

    let CapturedSegments {
        segments,
        tip_hash_hex,
        workload_density,
        canonical_block_facts_digest_evidence,
    } = capture_segments(&source, &config, &activations).await?;

    let subtree_roots = SubtreeRootSet {
        sapling: capture_subtree_roots(&source, ShieldedProtocol::Sapling, config.to_height)
            .await?,
        orchard: capture_subtree_roots(&source, ShieldedProtocol::Orchard, config.to_height)
            .await?,
        ironwood: capture_subtree_roots(&source, ShieldedProtocol::Ironwood, config.to_height)
            .await?,
    };

    let block_count = config
        .to_height
        .value()
        .saturating_sub(config.from_height.value())
        .saturating_add(1);
    let manifest = MigrationArchiveManifest {
        contract_identity: MIGRATION_ARCHIVE_IDENTITY.to_owned(),
        archive_format_version: MIGRATION_ARCHIVE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(config.network).to_owned(),
        from_height: config.from_height.value(),
        to_height: config.to_height.value(),
        block_count,
        workload_density,
        source_canonical_schema_version: config.source_canonical_schema_version,
        canonical_block_facts_digest_evidence,
        tip_hash_hex,
        network_upgrade_activations: activation_records,
        segments,
        subtree_roots,
    };
    manifest.write(&config.output_directory)?;
    Ok(manifest)
}

struct CapturedSegments {
    segments: Vec<SegmentDescriptor>,
    tip_hash_hex: String,
    workload_density: WorkloadDensity,
    canonical_block_facts_digest_evidence: CanonicalBlockFactsDigestEvidence,
}

/// Density and canonical-fact digest evidence computed while parsing migration archive blocks once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MigrationBlockMeasurements {
    /// Workload density derived from the prepared block-local facts.
    pub(crate) workload_density: WorkloadDensity,
    block_digests: Vec<CanonicalBlockFactsDigest>,
}

struct MigrationBlockMeasurement {
    workload_density: WorkloadDensity,
    block_digest: CanonicalBlockFactsDigest,
}

async fn capture_segments(
    source: &ZebraJsonRpcSource,
    config: &CaptureConfig,
    activations: &NetworkUpgradeActivations,
) -> Result<CapturedSegments, MigrationError> {
    let mut segments = Vec::new();
    let mut tip_hash_hex = String::new();
    let mut segment_index = 0_u32;
    let mut segment_from = config.from_height.value();
    let mut workload_density = WorkloadDensity::default();
    let mut sequence_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
        CanonicalBlockFactsSequenceDigestVersion::CURRENT,
    );
    while segment_from <= config.to_height.value() {
        let segment_started = Instant::now();
        let segment_to = segment_end_height(segment_from, config.segment_blocks, config.to_height);
        let fetch_started = Instant::now();
        let blocks: Arc<[SourceBlock]> =
            fetch_segment_blocks(source, segment_from, segment_to, config.fetch_concurrency)
                .await?
                .into();
        let fetch_seconds = fetch_started.elapsed().as_secs_f64();
        let prepare_started = Instant::now();
        let measurements = measure_migration_archive_blocks_bounded(
            Arc::clone(&blocks),
            activations,
            config.prepare_concurrency,
        )
        .await?;
        let prepare_seconds = prepare_started.elapsed().as_secs_f64();
        workload_density.merge(measurements.workload_density);
        for (block, digest) in blocks.iter().zip(measurements.block_digests) {
            if block.height != BlockHeight::new(0) {
                append_sequence_digest(&mut sequence_builder, digest)?;
            }
        }
        if let Some(last) = blocks.last()
            && last.height.value() == config.to_height.value()
        {
            tip_hash_hex = hex::encode(last.hash.as_bytes());
        }
        let segment_write_started = Instant::now();
        let descriptor = write_segment(&config.output_directory, segment_index, &blocks)?;
        let segment_write_seconds = segment_write_started.elapsed().as_secs_f64();
        tracing::info!(
            target: "zinder::migration",
            event = "segment_captured",
            index = descriptor.index,
            from_height = descriptor.from_height,
            to_height = descriptor.to_height,
            block_count = descriptor.block_count,
            fetch_seconds,
            prepare_seconds,
            segment_write_seconds,
            segment_seconds = segment_started.elapsed().as_secs_f64(),
            prepare_concurrency = config.prepare_concurrency.get(),
            "captured source segment"
        );
        segments.push(descriptor);
        segment_index = segment_index.saturating_add(1);
        segment_from = segment_to.saturating_add(1);
    }
    Ok(CapturedSegments {
        segments,
        tip_hash_hex,
        workload_density,
        canonical_block_facts_digest_evidence: digest_evidence_from_sequence(
            sequence_builder.finish(),
        ),
    })
}

/// Parses captured blocks once to compute workload density and canonical-fact digests.
#[cfg(test)]
pub(crate) fn measure_migration_archive_blocks(
    blocks: &[SourceBlock],
    activations: &NetworkUpgradeActivations,
) -> Result<MigrationBlockMeasurements, MigrationError> {
    let mut density = WorkloadDensity::default();
    let mut block_digests = Vec::with_capacity(blocks.len());
    for block in blocks {
        let measurement = measure_migration_archive_block(block, activations)?;
        density.merge(measurement.workload_density);
        block_digests.push(measurement.block_digest);
    }
    Ok(MigrationBlockMeasurements {
        workload_density: density,
        block_digests,
    })
}

/// Parses captured blocks concurrently while preserving their source order in
/// the canonical-fact sequence digest.
async fn measure_migration_archive_blocks_bounded(
    blocks: Arc<[SourceBlock]>,
    activations: &NetworkUpgradeActivations,
    prepare_concurrency: NonZeroU32,
) -> Result<MigrationBlockMeasurements, MigrationError> {
    let activations = Arc::new(activations.clone());
    let block_count = blocks.len();
    let tasks = (0..block_count).map(|block_index| {
        let blocks = Arc::clone(&blocks);
        let activations = Arc::clone(&activations);
        async move {
            tokio::task::spawn_blocking(move || {
                measure_migration_archive_block(&blocks[block_index], &activations)
            })
            .await
            .map_err(|source| {
                MigrationError::canonical_replay_storage_preparation_task(source.to_string())
            })?
        }
    });
    collect_migration_archive_block_measurements(tasks, prepare_concurrency, block_count).await
}

async fn collect_migration_archive_block_measurements<F>(
    tasks: impl IntoIterator<Item = F>,
    prepare_concurrency: NonZeroU32,
    block_count: usize,
) -> Result<MigrationBlockMeasurements, MigrationError>
where
    F: Future<Output = Result<MigrationBlockMeasurement, MigrationError>>,
{
    let concurrency = usize::try_from(prepare_concurrency.get()).unwrap_or(usize::MAX);
    let outcomes = futures_util::stream::iter(tasks).buffered(concurrency);
    futures_util::pin_mut!(outcomes);
    let mut workload_density = WorkloadDensity::default();
    let mut block_digests = Vec::with_capacity(block_count);
    while let Some(measurement) = outcomes.next().await {
        let measurement = measurement?;
        workload_density.merge(measurement.workload_density);
        block_digests.push(measurement.block_digest);
    }
    Ok(MigrationBlockMeasurements {
        workload_density,
        block_digests,
    })
}

fn measure_migration_archive_block(
    block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
) -> Result<MigrationBlockMeasurement, MigrationError> {
    let prepared = prepare_canonical_block(block, activations, RawBlobPolicy::None)?;
    let transaction_count = u32::try_from(prepared.facts.transactions.len()).map_err(|_| {
        MigrationError::archive_format("block transaction count exceeds u32".to_owned())
    })?;
    let transparent_input_count = prepared
        .facts
        .transactions
        .iter()
        .map(|transaction| transaction.transparent_inputs.len())
        .sum::<usize>();
    let transparent_input_count = u32::try_from(transparent_input_count).map_err(|_| {
        MigrationError::archive_format("block transparent input count exceeds u32".to_owned())
    })?;
    let transparent_output_count = u32::try_from(
        prepared
            .facts
            .transactions
            .iter()
            .map(|transaction| transaction.transparent_outputs.len())
            .sum::<usize>(),
    )
    .map_err(|_| {
        MigrationError::archive_format("block transparent output count exceeds u32".to_owned())
    })?;
    let raw_block_bytes = u64::try_from(block.raw_block_bytes.len()).unwrap_or(u64::MAX);
    let reference_encoding = prepared
        .facts
        .reference_encoding(CanonicalBlockFactsDigestVersion::CURRENT);
    Ok(MigrationBlockMeasurement {
        workload_density: WorkloadDensity {
            block_count: 1,
            raw_block_bytes,
            transaction_count: u64::from(transaction_count),
            transparent_input_count: u64::from(transparent_input_count),
            transparent_output_count: u64::from(transparent_output_count),
            blocks_with_transparent_inputs: u32::from(transparent_input_count > 0),
            blocks_with_transparent_outputs: u32::from(transparent_output_count > 0),
            max_raw_block_bytes_per_block: raw_block_bytes,
            max_transactions_per_block: transaction_count,
            max_transparent_inputs_per_block: transparent_input_count,
            max_transparent_outputs_per_block: transparent_output_count,
        },
        block_digest: reference_encoding.digest(),
    })
}

fn append_sequence_digest(
    sequence_builder: &mut CanonicalBlockFactsSequenceDigestBuilder,
    digest: CanonicalBlockFactsDigest,
) -> Result<(), MigrationError> {
    sequence_builder.try_append(digest).map_err(|source| {
        MigrationError::archive_format(format!(
            "canonical block facts sequence digest failed: {source}"
        ))
    })
}

fn digest_evidence_from_sequence(
    sequence_digest: CanonicalBlockFactsSequenceDigest,
) -> CanonicalBlockFactsDigestEvidence {
    CanonicalBlockFactsDigestEvidence {
        block_digest_version: CanonicalBlockFactsDigestVersion::CURRENT.value(),
        sequence_digest_version: sequence_digest.version().value(),
        block_count: sequence_digest.block_count(),
        sequence_digest_sha256: hex::encode(sequence_digest.as_bytes()),
    }
}

const fn segment_end_height(
    segment_from: u32,
    segment_blocks: NonZeroU32,
    to_height: BlockHeight,
) -> u32 {
    let last = segment_from.saturating_add(segment_blocks.get().saturating_sub(1));
    if last < to_height.value() {
        last
    } else {
        to_height.value()
    }
}

async fn fetch_segment_blocks(
    source: &ZebraJsonRpcSource,
    segment_from: u32,
    segment_to: u32,
    fetch_concurrency: NonZeroU32,
) -> Result<Vec<SourceBlock>, MigrationError> {
    let heights = segment_from..=segment_to;
    let ordered = futures_util::stream::iter(heights)
        .map(|height| source.fetch_block_at(BlockHeight::new(height)))
        .buffered(fetch_concurrency.get() as usize)
        .collect::<Vec<_>>()
        .await;
    let mut blocks = Vec::with_capacity(ordered.len());
    for outcome in ordered {
        blocks.push(outcome?);
    }
    Ok(blocks)
}

async fn capture_subtree_roots(
    source: &ZebraJsonRpcSource,
    protocol: ShieldedProtocol,
    to_height: BlockHeight,
) -> Result<Vec<SubtreeRootRecord>, MigrationError> {
    let Some(page_size) = NonZeroU32::new(SUBTREE_ROOT_PAGE_SIZE) else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    let mut next_index = 0_u32;
    loop {
        let response = source
            .fetch_subtree_roots(protocol, SubtreeRootIndex::new(next_index), page_size)
            .await?;
        let returned = response.subtree_roots.len();
        if returned == 0 {
            break;
        }
        let mut passed_range = false;
        for root in response.subtree_roots {
            if root.completing_block_height.value() > to_height.value() {
                passed_range = true;
                break;
            }
            records.push(SubtreeRootRecord {
                index: root.subtree_index.value(),
                root_hash_hex: hex::encode(root.root_hash.as_bytes()),
                completing_height: root.completing_block_height.value(),
            });
            next_index = root.subtree_index.value().saturating_add(1);
        }
        if passed_range || returned < page_size.get() as usize {
            break;
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::{error::Error, num::NonZeroU32, sync::Arc};

    use serde_json::Value;
    use zinder_core::{
        BlockHeight, CanonicalBlockFactsDigest, CanonicalBlockFactsDigestVersion, Network,
    };
    use zinder_source::SourceBlock;
    use zinder_testkit::sample_regtest_upgrade_activations;

    use super::{
        MigrationBlockMeasurement, collect_migration_archive_block_measurements,
        measure_migration_archive_blocks, measure_migration_archive_blocks_bounded,
    };

    const REGTEST_BLOCK_1: &str =
        include_str!("../../../services/zinder-ingest/tests/fixtures/z3-regtest-block-1.json");
    const REGTEST_BLOCK_603: &str = include_str!(
        "../../../services/zinder-ingest/tests/fixtures/z3-regtest-ironwood-block-603.json"
    );

    #[tokio::test]
    async fn bounded_measurements_equal_serial_measurements() -> Result<(), Box<dyn Error>> {
        let blocks = vec![
            source_block(REGTEST_BLOCK_1)?,
            source_block(REGTEST_BLOCK_603)?,
        ];
        let activations = sample_regtest_upgrade_activations();
        let serial = measure_migration_archive_blocks(&blocks, &activations)?;
        let concurrency = NonZeroU32::new(2).ok_or("2 must be non-zero")?;

        let bounded =
            measure_migration_archive_blocks_bounded(Arc::from(blocks), &activations, concurrency)
                .await?;

        assert_eq!(bounded, serial);
        Ok(())
    }

    #[tokio::test]
    async fn bounded_measurements_preserve_input_order_after_out_of_order_completion()
    -> Result<(), Box<dyn Error>> {
        let (completion_sender, mut completion_receiver) = tokio::sync::mpsc::unbounded_channel();
        let task_completion_sender = completion_sender.clone();
        let tasks =
            [(0_u8, 30_u64), (1, 1), (2, 10)]
                .into_iter()
                .map(move |(index, delay_millis)| {
                    let completion_sender = task_completion_sender.clone();
                    async move {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_millis)).await;
                        completion_sender.send(index).map_err(|source| {
                            crate::migration_error::MigrationError::invalid_argument(
                                source.to_string(),
                            )
                        })?;
                        Ok(MigrationBlockMeasurement {
                            workload_density: crate::migration_archive::WorkloadDensity {
                                block_count: 1,
                                raw_block_bytes: u64::from(index) + 1,
                                ..crate::migration_archive::WorkloadDensity::default()
                            },
                            block_digest: digest_for_index(index),
                        })
                    }
                });
        drop(completion_sender);
        let concurrency = NonZeroU32::new(3).ok_or("3 must be non-zero")?;

        let measurements =
            collect_migration_archive_block_measurements(tasks, concurrency, 3).await?;
        let mut completion_order = Vec::new();
        while let Some(index) = completion_receiver.recv().await {
            completion_order.push(index);
        }

        assert_eq!(completion_order, [1, 2, 0]);
        assert_eq!(
            measurements.block_digests,
            [
                digest_for_index(0),
                digest_for_index(1),
                digest_for_index(2)
            ]
        );
        assert_eq!(measurements.workload_density.raw_block_bytes, 6);
        Ok(())
    }

    fn digest_for_index(index: u8) -> CanonicalBlockFactsDigest {
        CanonicalBlockFactsDigest::from_reference_encoding(
            CanonicalBlockFactsDigestVersion::CURRENT,
            &[index],
        )
    }

    fn source_block(encoded_migration_archive: &str) -> Result<SourceBlock, Box<dyn Error>> {
        let migration_archive: Value = serde_json::from_str(encoded_migration_archive)?;
        let height = migration_archive
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|height| u32::try_from(height).ok())
            .ok_or("migration_archive height must fit in u32")?;
        let raw_block_hex = migration_archive
            .get("raw_block_hex")
            .and_then(Value::as_str)
            .ok_or("migration_archive raw_block_hex must be a string")?;
        Ok(SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(height),
            hex::decode(raw_block_hex)?,
        )?)
    }
}
