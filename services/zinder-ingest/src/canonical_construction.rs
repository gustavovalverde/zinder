//! Fresh version-1 canonical construction from one ordered node-source pass.

pub(crate) mod abort_on_drop;
pub(crate) mod source_fetch;
pub(crate) mod watermark;

use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::Duration,
};

use futures_util::{
    Stream, StreamExt,
    stream::{self, BoxStream},
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use zinder_core::{
    BlockHeight, Network, NetworkUpgradeActivations, NetworkUpgradeActivationsFingerprint,
    NetworkUpgradeActivationsFingerprintVersion,
};
use zinder_source::{NodeSource, SourceBlock};
use zinder_store::{
    CanonicalBlockLoadEvidence, CanonicalBuildBlock, CanonicalStoreBuildError, CanonicalStoreError,
    CanonicalStoreWorkload, RocksDbCanonicalBuilder,
};

use crate::{
    RawBlobPolicy,
    artifact_builder::{
        CommitmentTreeSizes, PositionedCanonicalBlock, PreparedCanonicalBlock, RetainedRawBlobs,
        position_canonical_block, prepare_canonical_block,
    },
    chain_ingest::IngestError,
};
use source_fetch::{
    CanonicalSourceFetchConfig, SourceBlockChunk, SourceSegmentSizer, build_source_block_stream,
};

const CANONICAL_PREPARE_MEMORY_UNIT_BYTES: u64 = 1_024;
const CANONICAL_PREPARE_RAW_BLOCK_MULTIPLIER: u64 = 16;
const CANONICAL_PREPARE_FIXED_BYTES: u64 = 64 * 1_024;

/// Bounded source-fetch and preparation settings for fresh canonical construction.
#[derive(Clone, Debug)]
pub struct CanonicalConstructionConfig {
    /// Timeout applied to one upstream source request.
    pub request_timeout: Duration,
    /// Maximum bytes accepted from one source response.
    pub max_response_bytes: NonZeroU64,
    /// Adaptive target for one source response payload.
    pub source_segment_target_response_bytes: NonZeroU64,
    /// Maximum blocks requested in one source segment.
    pub source_segment_max_blocks: NonZeroU32,
    /// Maximum concurrent source-segment requests.
    pub source_fetch_max_in_flight_requests: NonZeroU32,
    /// Aggregate byte watermark for in-flight source responses.
    pub source_fetch_max_in_flight_bytes: NonZeroU64,
    /// Maximum canonical block preparations in flight.
    pub block_prepare_concurrency: NonZeroU32,
    /// Aggregate byte watermark for block preparation and queued canonical blocks.
    pub block_prepare_memory_watermark_bytes: NonZeroU64,
    /// Node-discovered consensus upgrade activations used to parse transaction facts.
    pub network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

impl CanonicalConstructionConfig {
    /// Returns bounded settings suitable for deterministic local integration tests.
    #[must_use]
    pub fn for_local_tests(
        request_timeout: Duration,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        Self {
            request_timeout,
            max_response_bytes: NonZeroU64::new(16 * 1_024 * 1_024).unwrap_or(NonZeroU64::MIN),
            source_segment_target_response_bytes: NonZeroU64::new(8 * 1_024 * 1_024)
                .unwrap_or(NonZeroU64::MIN),
            source_segment_max_blocks: NonZeroU32::new(8).unwrap_or(NonZeroU32::MIN),
            source_fetch_max_in_flight_requests: NonZeroU32::new(2).unwrap_or(NonZeroU32::MIN),
            source_fetch_max_in_flight_bytes: NonZeroU64::new(32 * 1_024 * 1_024)
                .unwrap_or(NonZeroU64::MIN),
            block_prepare_concurrency: NonZeroU32::new(2).unwrap_or(NonZeroU32::MIN),
            block_prepare_memory_watermark_bytes: NonZeroU64::new(32 * 1_024 * 1_024)
                .unwrap_or(NonZeroU64::MIN),
            network_upgrade_activations,
        }
    }
}

/// Failure while constructing a fresh version-1 canonical store.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalConstructionError {
    /// Source fetch or canonical block preparation failed.
    #[error("canonical construction source pipeline failed")]
    Source {
        /// Concrete ingest failure preserved for diagnosis.
        #[source]
        source: IngestError,
    },
    /// Canonical storage construction failed.
    #[error(transparent)]
    Store(#[from] CanonicalStoreError),
    /// Configured upgrade activations describe another network.
    #[error(
        "canonical construction activations use {configured_network:?}, expected {store_network:?}"
    )]
    NetworkMismatch {
        /// Network persisted by the fresh canonical builder.
        store_network: Network,
        /// Network advertised by the activation table.
        configured_network: Network,
    },
    /// Configured upgrade activations differ from the immutable store identity.
    #[error(
        "canonical construction activation fingerprint {configured_fingerprint:?} does not match store identity {store_fingerprint:?}"
    )]
    NetworkUpgradeActivationsMismatch {
        /// Fingerprint persisted before canonical source work begins.
        store_fingerprint: NetworkUpgradeActivationsFingerprint,
        /// Fingerprint derived from the construction activation table.
        configured_fingerprint: NetworkUpgradeActivationsFingerprint,
    },
    /// A source block is labeled for another network than the fresh store.
    #[error(
        "source block {height:?} uses {source_network:?}, expected canonical store network {store_network:?}"
    )]
    SourceBlockNetworkMismatch {
        /// Height reported by the mismatched source block.
        height: BlockHeight,
        /// Network persisted by the fresh canonical builder.
        store_network: Network,
        /// Network declared by the source block.
        source_network: Network,
    },
    /// The blocking SST loader task could not complete.
    #[error("canonical loader task failed: {reason}")]
    LoaderTaskFailed {
        /// Tokio blocking-task failure.
        reason: String,
    },
}

/// Result of staging and ingesting block-local families into a fresh store.
pub struct CanonicalBlockLoadOutcome {
    /// Exclusive builder retained for subsequent source and publication families.
    pub builder: RocksDbCanonicalBuilder,
    /// Prepared row counts and SST measurements accepted by `RocksDB`.
    ///
    /// Cache-bypassing family readback remains a separate prerequisite for
    /// publishing this build as `READY`.
    pub evidence: CanonicalBlockLoadEvidence,
}

/// Loads every block-local canonical family from one ordered source pass.
///
/// Parallel preparation parses each source block exactly once. The blocking
/// store consumer performs ordered commitment-tree positioning immediately
/// before it fans the owned build block into the canonical column families.
/// The store remains `BUILDING` until the remaining source-observed families
/// and baseline publication records are loaded and validated.
pub async fn load_fresh_canonical_blocks<Source>(
    builder: RocksDbCanonicalBuilder,
    source: &Source,
    config: CanonicalConstructionConfig,
) -> Result<CanonicalBlockLoadOutcome, CanonicalConstructionError>
where
    Source: NodeSource + Clone,
{
    let build_plan = builder.build_plan();
    let workload = builder.workload();
    validate_construction_identity(build_plan, &config)?;
    let block_queue_capacity = usize::try_from(config.block_prepare_concurrency.get())
        .unwrap_or(usize::MAX)
        .max(1);
    let prepared_blocks = build_prepared_block_stream(
        source,
        build_plan,
        &config,
        raw_blob_policy_for_workload(workload),
    );
    drive_block_loader(builder, prepared_blocks, block_queue_capacity).await
}

const fn raw_blob_policy_for_workload(workload: CanonicalStoreWorkload) -> RawBlobPolicy {
    match workload {
        CanonicalStoreWorkload::Wallet => RawBlobPolicy::Transactions,
        CanonicalStoreWorkload::Explorer => RawBlobPolicy::All,
    }
}

fn validate_construction_identity(
    build_plan: zinder_store::CanonicalStoreBuildPlan,
    config: &CanonicalConstructionConfig,
) -> Result<(), CanonicalConstructionError> {
    let store_network = build_plan.network();
    let configured_network = config.network_upgrade_activations.network();
    if configured_network != store_network {
        return Err(CanonicalConstructionError::NetworkMismatch {
            store_network,
            configured_network,
        });
    }
    let store_fingerprint = build_plan.network_upgrade_activations_fingerprint();
    let configured_fingerprint = config
        .network_upgrade_activations
        .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
    if configured_fingerprint != store_fingerprint {
        return Err(
            CanonicalConstructionError::NetworkUpgradeActivationsMismatch {
                store_fingerprint,
                configured_fingerprint,
            },
        );
    }
    Ok(())
}

struct AdmittedSourceBlock {
    source_block: SourceBlock,
    memory_permit: OwnedSemaphorePermit,
}

struct SourceBlockAdmissionState<'source> {
    source_chunks: BoxStream<'source, Result<SourceBlockChunk, IngestError>>,
    pending_chunk: Option<SourceBlockChunk>,
    prepare_memory: Arc<Semaphore>,
    prepare_memory_permits: u32,
    expected_network: Network,
}

fn admit_source_blocks(
    source_chunks: BoxStream<'_, Result<SourceBlockChunk, IngestError>>,
    prepare_memory: Arc<Semaphore>,
    prepare_memory_permits: u32,
    expected_network: Network,
) -> impl Stream<Item = Result<AdmittedSourceBlock, CanonicalConstructionError>> + Send + '_ {
    let state = SourceBlockAdmissionState {
        source_chunks,
        pending_chunk: None,
        prepare_memory,
        prepare_memory_permits,
        expected_network,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(source_block) = state
                .pending_chunk
                .as_ref()
                .and_then(SourceBlockChunk::front)
            {
                if source_block.network != state.expected_network {
                    let error = CanonicalConstructionError::SourceBlockNetworkMismatch {
                        height: source_block.height,
                        store_network: state.expected_network,
                        source_network: source_block.network,
                    };
                    state.pending_chunk = None;
                    return Some((Err(error), state));
                }
                let permits = memory_permits_for_block(
                    source_block.raw_block_bytes.capacity(),
                    state.prepare_memory_permits,
                );
                let Ok(memory_permit) = Arc::clone(&state.prepare_memory)
                    .acquire_many_owned(permits)
                    .await
                else {
                    let error = CanonicalConstructionError::Source {
                        source: IngestError::BlockingTaskFailed {
                            reason: "canonical preparation memory gate closed".to_owned(),
                        },
                    };
                    return Some((Err(error), state));
                };
                let Some(source_block) = state
                    .pending_chunk
                    .as_mut()
                    .and_then(SourceBlockChunk::pop_front)
                else {
                    continue;
                };
                if state
                    .pending_chunk
                    .as_ref()
                    .is_some_and(SourceBlockChunk::is_empty)
                {
                    state.pending_chunk = None;
                }
                return Some((
                    Ok(AdmittedSourceBlock {
                        source_block,
                        memory_permit,
                    }),
                    state,
                ));
            }

            match state.source_chunks.next().await {
                Some(Ok(source_chunk)) => state.pending_chunk = Some(source_chunk),
                Some(Err(source)) => {
                    return Some((Err(CanonicalConstructionError::Source { source }), state));
                }
                None => return None,
            }
        }
    })
}

fn build_prepared_block_stream<'source, Source>(
    source: &'source Source,
    build_plan: zinder_store::CanonicalStoreBuildPlan,
    config: &CanonicalConstructionConfig,
    raw_blob_policy: RawBlobPolicy,
) -> impl Stream<Item = Result<QueuedPreparedBlock, CanonicalConstructionError>>
+ Send
+ use<'source, Source>
where
    Source: NodeSource + Clone,
{
    let first_height = build_plan.history_bounds().first_available_height();
    let source_segment_sizer = Arc::new(Mutex::new(SourceSegmentSizer::new(
        config.source_segment_max_blocks,
        config.source_segment_target_response_bytes,
        Arc::clone(&config.network_upgrade_activations),
        first_height,
    )));
    let source_chunks = build_source_block_stream(
        source,
        CanonicalSourceFetchConfig {
            request_timeout: config.request_timeout,
            history_predecessor: Some(build_plan.history_predecessor()),
            from_height: first_height,
            to_height: build_plan.build_tip().height,
            max_response_bytes: config.max_response_bytes,
            target_response_payload_bytes: config.source_segment_target_response_bytes,
            source_fetch_max_in_flight_requests: config.source_fetch_max_in_flight_requests,
            source_fetch_max_in_flight_bytes: config.source_fetch_max_in_flight_bytes,
            source_segment_sizer,
        },
    )
    .boxed();

    let prepare_memory_permits =
        memory_permit_count(config.block_prepare_memory_watermark_bytes.get());
    let prepare_memory = Arc::new(Semaphore::new(
        usize::try_from(prepare_memory_permits).unwrap_or(usize::MAX),
    ));
    let prepare_concurrency = usize::try_from(config.block_prepare_concurrency.get())
        .unwrap_or(usize::MAX)
        .max(1);
    let activations = Arc::clone(&config.network_upgrade_activations);
    admit_source_blocks(
        source_chunks,
        prepare_memory,
        prepare_memory_permits,
        build_plan.network(),
    )
    .map(move |admitted_source_block| {
        let activations = Arc::clone(&activations);
        async move {
            let AdmittedSourceBlock {
                source_block,
                memory_permit,
            } = admitted_source_block?;
            let prepared = tokio::task::spawn_blocking(move || {
                prepare_canonical_block(&source_block, &activations, raw_blob_policy)
                    .map_err(IngestError::from)
            })
            .await
            .map_err(|join_error| CanonicalConstructionError::Source {
                source: IngestError::BlockingTaskFailed {
                    reason: join_error.to_string(),
                },
            })?
            .map_err(|source| CanonicalConstructionError::Source { source })?;
            Ok(QueuedPreparedBlock {
                prepared,
                memory_permit,
            })
        }
    })
    .buffered(prepare_concurrency)
}

struct QueuedPreparedBlock {
    prepared: PreparedCanonicalBlock,
    memory_permit: OwnedSemaphorePermit,
}

async fn drive_block_loader<PreparedBlocks>(
    builder: RocksDbCanonicalBuilder,
    prepared_blocks: PreparedBlocks,
    block_queue_capacity: usize,
) -> Result<CanonicalBlockLoadOutcome, CanonicalConstructionError>
where
    PreparedBlocks: Stream<Item = Result<QueuedPreparedBlock, CanonicalConstructionError>> + Send,
{
    let build_plan = builder.build_plan();
    let (block_sender, block_receiver) = mpsc::channel(block_queue_capacity);
    let loader_task = tokio::task::spawn_blocking(move || {
        let mut builder = builder;
        let mut build_blocks = CanonicalBuildBlockReceiver::new(
            block_receiver,
            CommitmentTreeSizes::from_tip_metadata(build_plan.history_predecessor_tip_metadata()),
        );
        let evidence = builder
            .bulk_load_blocks(&mut build_blocks)
            .map_err(canonical_build_error)?;
        drop(build_blocks);
        Ok(CanonicalBlockLoadOutcome { builder, evidence })
    });

    let mut prepared_blocks = Box::pin(prepared_blocks);
    while let Some(prepared_block) = prepared_blocks.next().await {
        match prepared_block {
            Ok(queued_block) => {
                if block_sender.send(Ok(queued_block)).await.is_err() {
                    break;
                }
            }
            Err(source) => {
                let _ = block_sender.send(Err(source)).await;
                break;
            }
        }
    }
    drop(prepared_blocks);
    drop(block_sender);

    loader_task
        .await
        .map_err(|source| CanonicalConstructionError::LoaderTaskFailed {
            reason: source.to_string(),
        })?
}

struct CanonicalBuildBlockReceiver {
    receiver: mpsc::Receiver<Result<QueuedPreparedBlock, CanonicalConstructionError>>,
    active_memory_permit: Option<OwnedSemaphorePermit>,
    running_tree_sizes: CommitmentTreeSizes,
}

impl CanonicalBuildBlockReceiver {
    fn new(
        receiver: mpsc::Receiver<Result<QueuedPreparedBlock, CanonicalConstructionError>>,
        running_tree_sizes: CommitmentTreeSizes,
    ) -> Self {
        Self {
            receiver,
            active_memory_permit: None,
            running_tree_sizes,
        }
    }
}

impl Iterator for CanonicalBuildBlockReceiver {
    type Item = Result<CanonicalBuildBlock, CanonicalConstructionError>;

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the received block's memory permit moves into queued_block and stays active through the store write"
    )]
    fn next(&mut self) -> Option<Self::Item> {
        self.active_memory_permit = None;
        let received = self.receiver.blocking_recv()?;
        let queued_block = match received {
            Ok(queued_block) => queued_block,
            Err(source) => return Some(Err(source)),
        };
        self.active_memory_permit = Some(queued_block.memory_permit);
        Some(
            position_canonical_block(queued_block.prepared, &mut self.running_tree_sizes)
                .map(canonical_build_block)
                .map_err(|source| CanonicalConstructionError::Source {
                    source: IngestError::from(source),
                }),
        )
    }
}

fn canonical_build_block(positioned: PositionedCanonicalBlock) -> CanonicalBuildBlock {
    let PositionedCanonicalBlock {
        facts,
        replay_envelope,
        retained_raw_blobs,
        compact_block,
        tip_metadata,
    } = positioned;
    let RetainedRawBlobs {
        block_blob,
        transaction_blobs,
    } = retained_raw_blobs;
    CanonicalBuildBlock {
        facts,
        replay_envelope,
        compact_block,
        tip_metadata,
        transaction_blobs,
        block_blob,
    }
}

fn canonical_build_error(
    source: CanonicalStoreBuildError<CanonicalConstructionError>,
) -> CanonicalConstructionError {
    match source {
        CanonicalStoreBuildError::Source { source } => source,
        CanonicalStoreBuildError::Store(source) => CanonicalConstructionError::Store(source),
    }
}

fn memory_permit_count(limit_bytes: u64) -> u32 {
    let units = limit_bytes
        .saturating_add(CANONICAL_PREPARE_MEMORY_UNIT_BYTES.saturating_sub(1))
        .saturating_div(CANONICAL_PREPARE_MEMORY_UNIT_BYTES)
        .max(1);
    u32::try_from(units).unwrap_or(u32::MAX)
}

fn memory_permits_for_block(raw_block_capacity: usize, available_permits: u32) -> u32 {
    let estimated_bytes = u64::try_from(raw_block_capacity)
        .unwrap_or(u64::MAX)
        .max(1)
        .saturating_mul(CANONICAL_PREPARE_RAW_BLOCK_MULTIPLIER)
        .saturating_add(CANONICAL_PREPARE_FIXED_BYTES);
    memory_permit_count(estimated_bytes).min(available_permits)
}
