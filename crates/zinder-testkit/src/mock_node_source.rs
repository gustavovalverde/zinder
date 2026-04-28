//! [`NodeSource`] fakes built from a [`ChainFixture`].

use std::num::NonZeroU32;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use parking_lot::Mutex;
use zinder_core::{
    BlockHash, BlockHeight, BlockId, ShieldedProtocol, SubtreeRootHash, SubtreeRootIndex,
};
use zinder_source::{
    NodeCapabilities, NodeCapability, NodeSource, SourceBlock, SourceError, SourceSubtreeRoot,
    SourceSubtreeRoots,
};

use crate::chain_fixture::ChainFixture;

const MOCK_SUBTREE_ROOT_HASH_BYTE: u8 = 0x33;

/// A [`NodeSource`] fake backed by a [`ChainFixture`].
///
/// Construct with [`MockNodeSource::from_chain`]; tune the capability set
/// or pre-program failures via the builder methods. After handoff to the
/// system under test, mutate the tip height with
/// [`MockNodeSource::set_tip_height`] and observe call counts via
/// [`MockNodeSource::fetch_attempts`].
///
/// `MockNodeSource` is `Clone`; clones share the same tip height,
/// failure script, and fetch counters via interior `Arc<...>` handles, so
/// tests can pass an owned value to ingestion while retaining a control
/// handle.
#[derive(Clone, Debug)]
pub struct MockNodeSource {
    chain_fixture: ChainFixture,
    capabilities: NodeCapabilities,
    tip_height: Arc<AtomicU32>,
    failure_script: Arc<Mutex<NodeFailureScript>>,
    fetch_attempts: Arc<AtomicU32>,
    subtree_root_hash_byte: u8,
}

/// Pre-programmed failure responses for a [`MockNodeSource`].
///
/// A script is consumed call-by-call: each `fetch_block_by_height` returns a
/// failure until `pending_fetch_failures` reaches zero, after which the mock
/// returns the corresponding fixture block.
#[derive(Clone, Debug, Default)]
pub struct NodeFailureScript {
    pending_fetch_failures: u32,
    pending_fetch_failure_template: Option<FetchFailureTemplate>,
}

#[derive(Clone, Debug)]
enum FetchFailureTemplate {
    NodeUnavailable { reason: String, is_retryable: bool },
    BlockUnavailable { reason: String, is_retryable: bool },
}

impl NodeFailureScript {
    /// Returns an empty script that produces no failures.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            pending_fetch_failures: 0,
            pending_fetch_failure_template: None,
        }
    }

    /// Fail the next `count` block fetches with [`SourceError::NodeUnavailable`].
    #[must_use]
    pub fn fail_next_fetches_with_node_unavailable(
        count: u32,
        reason: impl Into<String>,
        is_retryable: bool,
    ) -> Self {
        Self {
            pending_fetch_failures: count,
            pending_fetch_failure_template: Some(FetchFailureTemplate::NodeUnavailable {
                reason: reason.into(),
                is_retryable,
            }),
        }
    }

    /// Fail the next `count` block fetches with [`SourceError::BlockUnavailable`].
    #[must_use]
    pub fn fail_next_fetches_with_block_unavailable(
        count: u32,
        reason: impl Into<String>,
        is_retryable: bool,
    ) -> Self {
        Self {
            pending_fetch_failures: count,
            pending_fetch_failure_template: Some(FetchFailureTemplate::BlockUnavailable {
                reason: reason.into(),
                is_retryable,
            }),
        }
    }

    fn consume_one_fetch_failure(&mut self, height: BlockHeight) -> Option<SourceError> {
        if self.pending_fetch_failures == 0 {
            return None;
        }
        self.pending_fetch_failures = self.pending_fetch_failures.saturating_sub(1);
        let template = self.pending_fetch_failure_template.as_ref()?;
        Some(match template {
            FetchFailureTemplate::NodeUnavailable {
                reason,
                is_retryable,
            } => SourceError::NodeUnavailable {
                reason: reason.clone(),
                is_retryable: *is_retryable,
            },
            FetchFailureTemplate::BlockUnavailable {
                reason,
                is_retryable,
            } => SourceError::BlockUnavailable {
                height,
                reason: reason.clone(),
                is_retryable: *is_retryable,
            },
        })
    }
}

impl MockNodeSource {
    /// Builds a mock node source over `chain_fixture` with the default
    /// capability set.
    #[must_use]
    pub fn from_chain(chain_fixture: ChainFixture) -> Self {
        let tip_height_value = chain_fixture.tip_height().map_or(0, BlockHeight::value);
        Self {
            chain_fixture,
            capabilities: default_capabilities(),
            tip_height: Arc::new(AtomicU32::new(tip_height_value)),
            failure_script: Arc::new(Mutex::new(NodeFailureScript::empty())),
            fetch_attempts: Arc::new(AtomicU32::new(0)),
            subtree_root_hash_byte: MOCK_SUBTREE_ROOT_HASH_BYTE,
        }
    }

    /// Replaces the advertised capability set.
    #[must_use]
    pub const fn with_capabilities(mut self, capabilities: NodeCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Installs a pre-programmed failure script.
    #[must_use]
    pub fn with_failure_script(self, failure_script: NodeFailureScript) -> Self {
        *self.failure_script.lock() = failure_script;
        self
    }

    /// Overrides the deterministic subtree-root hash byte.
    #[must_use]
    pub const fn with_subtree_root_hash_byte(mut self, byte: u8) -> Self {
        self.subtree_root_hash_byte = byte;
        self
    }

    /// Sets the node's reported tip height.
    pub fn set_tip_height(&self, height: BlockHeight) {
        self.tip_height.store(height.value(), Ordering::SeqCst);
    }

    /// Returns the count of `fetch_block_by_height` calls observed so far.
    #[must_use]
    pub fn fetch_attempts(&self) -> u32 {
        self.fetch_attempts.load(Ordering::SeqCst)
    }

    /// Returns the count of failures still pending in the failure script.
    #[must_use]
    pub fn pending_fetch_failures(&self) -> u32 {
        self.failure_script.lock().pending_fetch_failures
    }
}

#[async_trait]
impl NodeSource for MockNodeSource {
    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities
    }

    async fn fetch_block_by_height(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.fetch_attempts.fetch_add(1, Ordering::SeqCst);

        let scheduled_failure = self.failure_script.lock().consume_one_fetch_failure(height);
        if let Some(scheduled_failure) = scheduled_failure {
            return Err(scheduled_failure);
        }

        self.chain_fixture
            .source_block_at(height)
            .ok_or_else(|| SourceError::BlockUnavailable {
                height,
                reason: "mock chain fixture does not contain this height".to_owned(),
                is_retryable: false,
            })
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        let height = BlockHeight::new(self.tip_height.load(Ordering::SeqCst));
        let hash = self
            .chain_fixture
            .block_at(height)
            .map_or_else(|| BlockHash::from_bytes([0; 32]), |block| block.hash);
        Ok(BlockId::new(height, hash))
    }

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        if !self.capabilities.supports(NodeCapability::SubtreeRoots) {
            return Err(SourceError::NodeCapabilityMissing {
                capability: NodeCapability::SubtreeRoots,
            });
        }

        let completing_block_height = self
            .chain_fixture
            .tip_height()
            .unwrap_or(BlockHeight::new(1));
        let subtree_roots = (0..max_entries.get())
            .map(|offset| {
                start_index
                    .value()
                    .checked_add(offset)
                    .map(SubtreeRootIndex::new)
                    .map(|index| {
                        SourceSubtreeRoot::new(
                            index,
                            SubtreeRootHash::from_bytes([self.subtree_root_hash_byte; 32]),
                            completing_block_height,
                        )
                    })
                    .ok_or(SourceError::SourceProtocolMismatch {
                        reason: "mock subtree roots response exceeds the SubtreeRootIndex range",
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(SourceSubtreeRoots::new(
            protocol,
            start_index,
            subtree_roots,
        ))
    }
}

fn default_capabilities() -> NodeCapabilities {
    NodeCapabilities::new([
        NodeCapability::BestChainBlocks,
        NodeCapability::TipId,
        NodeCapability::TreeState,
        NodeCapability::SubtreeRoots,
        NodeCapability::TransactionBroadcast,
    ])
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{MockNodeSource, NodeFailureScript};
    use crate::chain_fixture::ChainFixture;
    use zinder_core::{BlockHeight, Network, ShieldedProtocol, SubtreeRootIndex};
    use zinder_source::{NodeSource, SourceError};

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_block_returns_fixture_block() -> Result<(), Box<dyn Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
        let node_source = MockNodeSource::from_chain(chain_fixture);
        let block = node_source
            .fetch_block_by_height(BlockHeight::new(2))
            .await?;
        assert_eq!(block.height, BlockHeight::new(2));
        assert_eq!(node_source.fetch_attempts(), 1);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failure_script_replays_pending_failures_then_succeeds() -> Result<(), Box<dyn Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
        let node_source = MockNodeSource::from_chain(chain_fixture).with_failure_script(
            NodeFailureScript::fail_next_fetches_with_node_unavailable(2, "planned outage", true),
        );

        let outcomes = [
            node_source.fetch_block_by_height(BlockHeight::new(1)).await,
            node_source.fetch_block_by_height(BlockHeight::new(1)).await,
            node_source.fetch_block_by_height(BlockHeight::new(1)).await,
        ];
        assert!(matches!(
            outcomes[0],
            Err(SourceError::NodeUnavailable { .. })
        ));
        assert!(matches!(
            outcomes[1],
            Err(SourceError::NodeUnavailable { .. })
        ));
        assert!(outcomes[2].is_ok());
        assert_eq!(node_source.fetch_attempts(), 3);
        assert_eq!(node_source.pending_fetch_failures(), 0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_height_returns_block_unavailable() -> Result<(), Box<dyn Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
        let node_source = MockNodeSource::from_chain(chain_fixture);
        let outcome = node_source
            .fetch_block_by_height(BlockHeight::new(99))
            .await;
        assert!(matches!(
            outcome,
            Err(SourceError::BlockUnavailable {
                height,
                ..
            }) if height == BlockHeight::new(99)
        ));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tip_id_reports_height_and_fixture_hash() -> Result<(), Box<dyn Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(5);
        let expected_hash = chain_fixture
            .block_at(BlockHeight::new(3))
            .ok_or("missing block 3 in fixture")?
            .hash;
        let node_source = MockNodeSource::from_chain(chain_fixture);
        node_source.set_tip_height(BlockHeight::new(3));

        let observed = node_source.tip_id().await?;
        assert_eq!(observed.height, BlockHeight::new(3));
        assert_eq!(observed.hash, expected_hash);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subtree_roots_returns_max_entries() -> Result<(), Box<dyn Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
        let node_source = MockNodeSource::from_chain(chain_fixture);
        let response = node_source
            .fetch_subtree_roots(
                ShieldedProtocol::Sapling,
                SubtreeRootIndex::new(0),
                std::num::NonZeroU32::new(3).ok_or("3 is non-zero")?,
            )
            .await?;
        assert_eq!(response.subtree_roots.len(), 3);
        Ok(())
    }
}
