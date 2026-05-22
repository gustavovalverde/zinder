//! Integration coverage for the unified `run_ingest_loop`.
//!
//! The testkit mock returns synthetic block bytes that the artifact
//! builder cannot parse, so these tests focus on observable loop
//! behavior that does not require a successful commit: phase
//! classification, phase stamping on readiness, the
//! `target_height` modifier, and the spawn-once gate for the
//! `FollowingTip` subsystems. Commit and transition coverage lives in
//! the store, backfill, tip-follow, and live suites.

#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use eyre::{Result, eyre};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHash, BlockHeight, BlockId, Network, ShieldedProtocol, SubtreeRootIndex};
use zinder_ingest::{
    BulkCatchupConfig, IngestDeriveConfig, IngestLoopConfig, IngestModifiers, NodeSourceKind,
    PhasesConfig, TipFollowPhaseConfig, TipFollowSubsystems, TipFollowSubsystemsLauncher,
    run_ingest_loop,
};
use zinder_runtime::{IngestPhase, Readiness};
use zinder_source::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeCapabilities, NodeSource, NodeTarget,
    SourceBlock, SourceError, SourceSubtreeRoots,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::ChainFixture;

#[tokio::test]
async fn ingest_loop_awaits_upstream_when_upstream_tip_is_zero() -> Result<()> {
    let storage_path = tempdir()?.path().join("ingest-loop-awaiting-store");
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_store = test_derive_store(&storage_path)?;
    let source = ControllableTipSource::new(ChainFixture::new(Network::ZcashRegtest));
    source.set_tip_height(BlockHeight::new(0));

    let config = sample_loop_config(&storage_path)?;
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();
    let launcher = noop_launcher();

    let loop_handle = {
        let readiness = readiness.clone();
        let store = store.clone();
        let derive_store = derive_store.clone();
        let source = source.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            run_ingest_loop(
                &config,
                Arc::new(source),
                store,
                derive_store,
                &readiness,
                cancel,
                Some(launcher),
            )
            .await
        })
    };

    wait_until_phase(&readiness, IngestPhase::AwaitingUpstream).await?;
    assert_eq!(source.fetch_attempts(), 0);

    cancel.cancel();
    loop_handle.await??;
    Ok(())
}

#[tokio::test]
async fn ingest_loop_stamps_following_tip_on_readiness_when_gap_is_small() -> Result<()> {
    let storage_path = tempdir()?.path().join("ingest-loop-tip-store");
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_store = test_derive_store(&storage_path)?;
    let source = ControllableTipSource::new(ChainFixture::new(Network::ZcashRegtest));
    source.set_tip_height(BlockHeight::new(1));

    let config = sample_loop_config(&storage_path)?;
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();
    let launcher = recording_launcher();
    let launcher_calls = launcher.calls.clone();

    let loop_handle = {
        let readiness = readiness.clone();
        let store = store.clone();
        let derive_store = derive_store.clone();
        let source = source.clone();
        let cancel = cancel.clone();
        let launcher = launcher.into_launcher();
        tokio::spawn(async move {
            run_ingest_loop(
                &config,
                Arc::new(source),
                store,
                derive_store,
                &readiness,
                cancel,
                Some(launcher),
            )
            .await
        })
    };

    wait_until_phase(&readiness, IngestPhase::FollowingTip).await?;
    // The spawn-once gate must have fired exactly once on first
    // FollowingTip entry, regardless of how many later iterations run.
    assert_eq!(launcher_calls.load(Ordering::SeqCst), 1);

    cancel.cancel();
    loop_handle.await??;
    Ok(())
}

#[tokio::test]
async fn ingest_loop_exits_when_target_height_already_covered() -> Result<()> {
    let storage_path = tempdir()?.path().join("ingest-loop-target-store");
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_store = test_derive_store(&storage_path)?;
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let artifacts = chain
        .chain_epoch_artifacts(zinder_core::ChainEpochId::new(1))
        .ok_or_else(|| eyre!("fixture missing chain epoch 1"))?;
    store.commit_chain_epoch(artifacts)?;

    let source = ControllableTipSource::new(chain);
    source.set_tip_height(BlockHeight::new(1));

    let mut config = sample_loop_config(&storage_path)?;
    config.modifiers.target_height = Some(BlockHeight::new(1));

    let readiness = Readiness::default();
    let cancel = CancellationToken::new();
    let launcher = noop_launcher();

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_ingest_loop(
            &config,
            Arc::new(source),
            store,
            derive_store,
            &readiness,
            cancel,
            Some(launcher),
        ),
    )
    .await;

    let loop_result = outcome.map_err(|_| eyre!("loop did not exit at target_height"))?;
    loop_result?;
    Ok(())
}

fn sample_loop_config(storage_path: &std::path::Path) -> Result<IngestLoopConfig> {
    Ok(IngestLoopConfig {
        node: NodeTarget::new(
            Network::ZcashRegtest,
            "http://127.0.0.1:0".to_owned(),
            NodeAuth::None,
            Duration::from_secs(5),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        storage_path: storage_path.to_path_buf(),
        reorg_window_blocks: 100,
        phases: PhasesConfig {
            catchup_threshold_blocks: 100,
        },
        derive: IngestDeriveConfig {
            concurrency: NonZeroU32::new(4).ok_or_else(|| eyre!("nonzero"))?,
        },
        bulk_catchup: BulkCatchupConfig {
            commit_batch_blocks: NonZeroU32::new(1_000).ok_or_else(|| eyre!("nonzero"))?,
            max_transparent_prevout_store_lookups_per_batch: NonZeroU32::new(250_000)
                .ok_or_else(|| eyre!("nonzero"))?,
            fetch_concurrency: NonZeroU32::new(32).ok_or_else(|| eyre!("nonzero"))?,
            flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("nonzero"))?,
        },
        tip_follow: TipFollowPhaseConfig {
            poll_interval: Duration::from_millis(10),
            lag_threshold_blocks: 1,
        },
        modifiers: IngestModifiers::default(),
    })
}

fn test_derive_store(storage_path: &std::path::Path) -> Result<zinder_derive::DeriveStore> {
    Ok(zinder_derive::DeriveStore::open(
        zinder_derive::DeriveStore::path_for_canonical(storage_path),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: &[],
            tuning: zinder_store::StorageTuning::for_local_tests(),
        },
    )?)
}

async fn wait_until_phase(readiness: &Readiness, target: IngestPhase) -> Result<()> {
    let deadline = Duration::from_secs(5);
    tokio::time::timeout(deadline, async {
        loop {
            if readiness.report().phase == Some(target) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| {
        eyre!(
            "readiness never reached phase {target:?}; last report = {:?}",
            readiness.report()
        )
    })?;
    Ok(())
}

fn noop_launcher() -> TipFollowSubsystemsLauncher {
    Box::new(|| TipFollowSubsystems {
        mempool_ready_gate: None,
        chain_tip_source: None,
        spawned_tasks: Vec::new(),
    })
}

struct RecordingLauncher {
    calls: Arc<AtomicUsize>,
}

fn recording_launcher() -> RecordingLauncher {
    RecordingLauncher {
        calls: Arc::new(AtomicUsize::new(0)),
    }
}

impl RecordingLauncher {
    fn into_launcher(self) -> TipFollowSubsystemsLauncher {
        let calls = self.calls;
        Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            TipFollowSubsystems {
                mempool_ready_gate: None,
                chain_tip_source: None,
                spawned_tasks: Vec::new(),
            }
        })
    }
}

/// A `NodeSource` with a controllable tip height; every
/// `fetch_block_at` returns the production view-stale failure so the
/// loop never tries to commit synthetic fixture bytes.
#[derive(Clone)]
struct ControllableTipSource {
    chain: ChainFixture,
    tip_height: Arc<AtomicU32>,
    fetch_attempts: Arc<AtomicU32>,
}

impl ControllableTipSource {
    fn new(chain: ChainFixture) -> Self {
        Self {
            chain,
            tip_height: Arc::new(AtomicU32::new(0)),
            fetch_attempts: Arc::new(AtomicU32::new(0)),
        }
    }

    fn set_tip_height(&self, height: BlockHeight) {
        self.tip_height.store(height.value(), Ordering::SeqCst);
    }

    fn fetch_attempts(&self) -> u32 {
        self.fetch_attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl NodeSource for ControllableTipSource {
    fn capabilities(&self) -> NodeCapabilities {
        zinder_source::ZebraJsonRpcSource::baseline_capabilities()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.fetch_attempts.fetch_add(1, Ordering::SeqCst);
        Err(SourceError::BlockUnavailable {
            height,
            reason: "controllable source never commits fixture bytes".to_owned(),
        })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        let height = BlockHeight::new(self.tip_height.load(Ordering::SeqCst));
        let hash = self
            .chain
            .block_at(height)
            .map_or_else(|| BlockHash::from_bytes([0; 32]), |block| block.hash);
        Ok(BlockId::new(height, hash))
    }

    async fn fetch_subtree_roots(
        &self,
        _protocol: ShieldedProtocol,
        _start_index: SubtreeRootIndex,
        _max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        Err(SourceError::NodeCapabilityMissing {
            capability: zinder_source::NodeCapability::SubtreeRoots,
        })
    }
}
