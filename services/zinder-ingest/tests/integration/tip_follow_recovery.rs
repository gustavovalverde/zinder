//! Source-recovery contract coverage for the tip-follow loop.
//!
//! Each test pins one half of the [ADR-0013] contract: source-shaped errors
//! drain readiness and continue, structural errors stay alive in an
//! operator-action readiness state, and only storage/reorg failures exit.
//!
//! [ADR-0013]: ../../../../docs/adrs/0013-source-failure-recovery-topology.md

#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use eyre::{Result, eyre};
use parking_lot::Mutex;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHash, BlockHeight, BlockId, Network, ShieldedProtocol, SubtreeRootIndex};
use zinder_ingest::{TipFollowConfig, tip_follow_with_primary_store};
use zinder_runtime::{Readiness, ReadinessCause};
use zinder_source::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeCapabilities, NodeSource, NodeTarget,
    SourceBlock, SourceError, SourceFailureClass, SourceSubtreeRoots,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::ChainFixture;
use zinder_testkit::sample_regtest_upgrade_activations;

/// `BlockUnavailable` with a best-chain view-change reason keeps the writer
/// alive and reports the typed readiness class.
#[tokio::test]
async fn tip_follow_survives_block_unavailable_from_unknown_json_rpc_code() -> Result<()> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let view_changing_source = ViewChangingSource::new(chain);
    let storage_path = tempdir()?.path().join("tip-follow-view-stale");
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let config = sample_tip_follow_config(&storage_path);
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();

    let loop_handle = {
        let readiness = readiness.clone();
        let cancel = cancel.clone();
        let source = view_changing_source.clone();
        tokio::spawn(async move {
            tip_follow_with_primary_store(&config, &source, store, &readiness, None, None, cancel)
                .await
        })
    };

    wait_until_node_unavailable(&readiness).await?;

    let cause = readiness.report().cause;
    let ReadinessCause::NodeUnavailable(ref detail) = cause else {
        return Err(eyre!("expected NodeUnavailable, got {cause:?}"));
    };
    assert_eq!(detail.failure_class, "upstream_view_changed");
    assert!(
        detail
            .last_reason
            .contains("block height not in best chain")
    );

    cancel.cancel();
    loop_handle.await??;
    assert!(view_changing_source.fetch_attempts() >= 1);

    Ok(())
}

/// Operator-action failure (protocol mismatch) keeps the writer alive but
/// reports a different failure class so dashboards can route a different alert.
#[tokio::test]
async fn tip_follow_stays_alive_under_protocol_mismatch() -> Result<()> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let mismatching_source = ProtocolMismatchSource::new(chain);
    let storage_path = tempdir()?.path().join("tip-follow-protocol-mismatch");
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let config = sample_tip_follow_config(&storage_path);
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();

    let loop_handle = {
        let readiness = readiness.clone();
        let cancel = cancel.clone();
        let source = mismatching_source.clone();
        tokio::spawn(async move {
            tip_follow_with_primary_store(&config, &source, store, &readiness, None, None, cancel)
                .await
        })
    };

    wait_until_node_unavailable(&readiness).await?;
    let cause = readiness.report().cause;
    let ReadinessCause::NodeUnavailable(ref detail) = cause else {
        return Err(eyre!("expected NodeUnavailable, got {cause:?}"));
    };
    assert_eq!(detail.failure_class, "protocol_mismatch");

    cancel.cancel();
    loop_handle.await??;
    Ok(())
}

/// Outage tracker advances `consecutive_failures` across multiple failed
/// iterations, then resets when the upstream view recovers.
///
/// Every `fetch_block_at` first returns a view-stale `BlockUnavailable`, then
/// the upstream node settles back to genesis so `tip_follow_plan` returns
/// `Ok(None)` without another fetch. That path clears the outage tracker and
/// transitions readiness out of `NodeUnavailable`.
#[tokio::test]
async fn tip_follow_advances_outage_counter_then_clears_on_recovery() -> Result<()> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let node = ControllableTipSource::new(chain);
    node.set_tip_height(BlockHeight::new(1));
    let storage_path = tempdir()?.path().join("tip-follow-outage-counter");
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let config = sample_tip_follow_config(&storage_path);
    let readiness = Readiness::default();
    let cancel = CancellationToken::new();

    let loop_handle = {
        let readiness = readiness.clone();
        let cancel = cancel.clone();
        let source = node.clone();
        tokio::spawn(async move {
            tip_follow_with_primary_store(&config, &source, store, &readiness, None, None, cancel)
                .await
        })
    };

    wait_until_node_unavailable(&readiness).await?;
    wait_until_consecutive_failures_reaches(&readiness, 2).await?;

    // Simulate the upstream view collapsing back to genesis: with an empty
    // store and `observed_tip_id.height == 0`, the planner returns
    // `Ok(None)` and the loop resets the outage tracker.
    node.set_tip_height(BlockHeight::new(0));

    wait_until_recovered(&readiness).await?;
    assert!(node.fetch_attempts() >= 1);

    cancel.cancel();
    loop_handle.await??;
    Ok(())
}

fn sample_tip_follow_config(storage_path: &std::path::Path) -> TipFollowConfig {
    TipFollowConfig {
        node: NodeTarget::new(
            Network::ZcashRegtest,
            "http://127.0.0.1:0".to_owned(),
            NodeAuth::None,
            Duration::from_secs(5),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        storage_path: storage_path.to_path_buf(),
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        raw_blob_policy: zinder_ingest::RawBlobPolicy::None,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        reorg_window_blocks: 100,
        poll_interval: Duration::from_millis(10),
        lag_threshold_blocks: 1,
        phase_exit_lag_blocks: None,
        target_height: None,
    }
}

async fn wait_until_node_unavailable(readiness: &Readiness) -> Result<()> {
    let deadline = Duration::from_secs(5);
    let outcome = tokio::time::timeout(deadline, async {
        loop {
            if matches!(readiness.report().cause, ReadinessCause::NodeUnavailable(_)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    outcome.map_err(|_| eyre!("readiness never transitioned to NodeUnavailable"))?;
    Ok(())
}

async fn wait_until_consecutive_failures_reaches(readiness: &Readiness, target: u32) -> Result<()> {
    let deadline = Duration::from_secs(5);
    let outcome = tokio::time::timeout(deadline, async {
        loop {
            if let ReadinessCause::NodeUnavailable(detail) = readiness.report().cause
                && detail.consecutive_failures >= target
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    outcome.map_err(|_| {
        eyre!(
            "consecutive_failures never reached {target}; last cause: {:?}",
            readiness.report().cause
        )
    })?;
    Ok(())
}

async fn wait_until_recovered(readiness: &Readiness) -> Result<()> {
    let deadline = Duration::from_secs(5);
    let outcome = tokio::time::timeout(deadline, async {
        loop {
            if !matches!(readiness.report().cause, ReadinessCause::NodeUnavailable(_)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    outcome.map_err(|_| {
        eyre!(
            "readiness never recovered from NodeUnavailable; last cause: {:?}",
            readiness.report().cause
        )
    })?;
    Ok(())
}

/// A `NodeSource` that returns Zebra's best-chain view-change failure.
///
/// The loop classifies `"block height not in best chain"` as
/// [`SourceFailureClass::UpstreamViewChanged`] and stays alive.
#[derive(Clone)]
struct ViewChangingSource {
    chain: ChainFixture,
    fetch_attempts: Arc<Mutex<u32>>,
}

impl ViewChangingSource {
    fn new(chain: ChainFixture) -> Self {
        Self {
            chain,
            fetch_attempts: Arc::new(Mutex::new(0)),
        }
    }

    fn fetch_attempts(&self) -> u32 {
        *self.fetch_attempts.lock()
    }
}

#[async_trait]
impl NodeSource for ViewChangingSource {
    fn capabilities(&self) -> NodeCapabilities {
        zinder_source::ZebraJsonRpcSource::baseline_capabilities()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        *self.fetch_attempts.lock() += 1;
        Err(SourceError::BlockUnavailable {
            height,
            reason: "block height not in best chain".to_owned(),
        })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        let tip_height = self.chain.tip_height().unwrap_or(BlockHeight::new(0));
        let hash = self
            .chain
            .block_at(tip_height)
            .map_or_else(|| BlockHash::from_bytes([0; 32]), |block| block.hash);
        Ok(BlockId::new(tip_height, hash))
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

/// A `NodeSource` whose `fetch_block_at` always returns the production
/// view-stale failure and whose `tip_id` height is controllable.
///
/// Tests use this to drive the outage tracker through several failed
/// iterations (`tip_height > 0`, fetch keeps failing) and then trigger
/// recovery by lowering the tip to zero (`tip_follow_plan` returns
/// `Ok(None)` without touching `fetch_block_at`, so the loop clears the
/// outage tracker without needing parseable block bytes).
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
            reason: "block height not in best chain".to_owned(),
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

/// A `NodeSource` whose tip lookup returns a structural protocol mismatch.
///
/// Verifies that operator-action failures (the runbook's "fail closed for
/// inspection" path) stay alive in readiness rather than exiting the
/// process; the writer drains readiness with a `protocol_mismatch` class so
/// operators can route a distinct alert.
#[derive(Clone)]
struct ProtocolMismatchSource {
    chain: ChainFixture,
}

impl ProtocolMismatchSource {
    fn new(chain: ChainFixture) -> Self {
        Self { chain }
    }
}

#[async_trait]
impl NodeSource for ProtocolMismatchSource {
    fn capabilities(&self) -> NodeCapabilities {
        zinder_source::ZebraJsonRpcSource::baseline_capabilities()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.chain
            .source_block_at(height)
            .ok_or(SourceError::SourceProtocolMismatch {
                reason: "fixture exhausted",
            })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        Err(SourceError::SourceProtocolMismatch {
            reason: "best block header hash does not match tip hash",
        })
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

#[test]
fn source_failure_class_labels_match_runbook_table() {
    // Pin the labels the runbook table promises operators. Renaming any of
    // these requires updating dashboards and alert rules in lock-step.
    assert_eq!(
        SourceFailureClass::NodeUnreachable.label(),
        "node_unreachable"
    );
    assert_eq!(
        SourceFailureClass::UpstreamViewChanged.label(),
        "upstream_view_changed"
    );
    assert_eq!(
        SourceFailureClass::StreamDisconnected.label(),
        "stream_disconnected"
    );
    assert_eq!(
        SourceFailureClass::ProtocolMismatch.label(),
        "protocol_mismatch"
    );
    assert_eq!(
        SourceFailureClass::CapabilityMissing.label(),
        "capability_missing"
    );
    assert_eq!(SourceFailureClass::Malformed.label(), "malformed");
    assert_eq!(SourceFailureClass::Configuration.label(), "configuration");
}
