//! Derive-consumer trait, typed event wrappers, and subscription helpers.
//!
//! Every derive consumer implements [`DeriveConsumer`]. The trait is the seam
//! between the consumer-agnostic infrastructure in this crate (store,
//! [`BlockSource`], `ChainEvents` subscriber, mempool subscriber) and the
//! consumer-specific aggregation logic that lives in each consumer module.
//!
//! Two submodules layer subscriber primitives on top of the trait:
//!
//! - The chain-events subscriber drives a `WalletQuery.ChainEvents`
//!   subscription with cursor persistence and dispatches each envelope to
//!   the consumer's `apply_chain_committed` / `apply_chain_reorged` hooks.
//!   Public entry point: [`crate::run_chain_events_subscriber`].
//! - The mempool-events subscriber drives a `WalletQuery.MempoolEvents`
//!   subscription with cursor persistence for consumers that observe
//!   unconfirmed activity. Public entry point:
//!   [`crate::run_mempool_events_subscriber`].
//!
//! A consumer that needs the parsed block in `apply_block` does NOT fetch
//! `WalletQuery.FullBlock` itself. It pulls the parsed block from
//! [`BlockSource::block`]; the source caches per-height contexts so the
//! four-consumer fan-out parses each block once per commit.

pub(crate) mod block_commit_context;
pub(crate) mod block_source;
pub(crate) mod block_summary;
pub(crate) mod chain_events;
pub(crate) mod mempool_event_counts;
pub(crate) mod mempool_events;
pub(crate) mod recent_transactions;
pub(crate) mod transaction_fees;
pub(crate) mod transparent_address_activity;

use async_trait::async_trait;
use futures_util::StreamExt;
use rust_rocksdb::WriteBatch;
use zinder_core::{BlockHeight, ChainEpoch};

pub use block_commit_context::{BlockCommitContext, BlockCommitContextError, PrevoutResolver};
pub use block_source::BlockSource;

use crate::store::DeriveStore;

/// Stable name of a derive consumer used to scope cursor and metadata rows.
///
/// The name is part of the on-disk key prefix in the `cursor` column family;
/// renaming a consumer between releases is a schema migration, not a config
/// change. Names are short, lowercase, snake-case, and stable across binary
/// versions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeriveConsumerName(&'static str);

impl DeriveConsumerName {
    /// Creates a derive-consumer name from a static string.
    ///
    /// The caller must ensure the name is stable across releases; renaming a
    /// consumer between deployments orphans its persisted cursor.
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self(name)
    }

    /// Returns the underlying string value used in cursor and metadata keys.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl AsRef<[u8]> for DeriveConsumerName {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Boxed application error returned by consumer apply methods.
///
/// The SDK does not constrain how consumers report failures inside their
/// `apply_*` hooks: a consumer that fails to write to its own column family
/// returns a typed error whose shape it controls. The boxed form lets the
/// SDK surface that failure verbatim through [`crate::DeriveError::Consumer`]
/// without coupling the SDK to any one consumer's error enum.
pub type DeriveConsumerError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Per-event consumer context.
///
/// Carries a borrow of the [`DeriveStore`] (for read-only lookups during
/// apply) and a borrow of the [`WriteBatch`] the SDK will commit. Consumers
/// stage their writes into the batch; the SDK appends the cursor advance to
/// the same batch and commits atomically. A crash between
/// `apply_chain_committed` and the commit therefore replays the event on next
/// startup; a crash after the commit advances both cursor and consumer state
/// together.
pub struct DeriveConsumerCtx<'a> {
    /// Store the consumer reads from while applying events.
    pub store: &'a DeriveStore,
    /// Write batch the consumer stages its data writes into.
    pub batch: &'a mut WriteBatch,
}

/// Typed wrapper for a `ChainCommitted` chain event delivered to a consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChainCommittedEvent {
    /// Monotonic event sequence the SDK uses for cursor accounting.
    pub event_sequence: u64,
    /// Chain epoch visible after the commit.
    pub chain_epoch: ChainEpoch,
    /// Finalized block height that was true at delivery time.
    pub finalized_height: BlockHeight,
    /// First committed block height (inclusive).
    pub start_height: BlockHeight,
    /// Last committed block height (inclusive).
    pub end_height: BlockHeight,
}

/// Typed wrapper for a `ChainReorged` chain event delivered to a consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChainReorgedEvent {
    /// Monotonic event sequence the SDK uses for cursor accounting.
    pub event_sequence: u64,
    /// Chain epoch visible after the reorg replacement commits.
    pub chain_epoch: ChainEpoch,
    /// Finalized block height that was true at delivery time.
    pub finalized_height: BlockHeight,
    /// Range invalidated by the reorg.
    pub reverted: RevertedRange,
    /// Replacement range committed by the reorg.
    pub replacement: CommittedRange,
}

/// Reverted block range carried by a [`ChainReorgedEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RevertedRange {
    /// Chain epoch that contained the reverted range.
    pub chain_epoch: ChainEpoch,
    /// First reverted block height (inclusive).
    pub start_height: BlockHeight,
    /// Last reverted block height (inclusive).
    pub end_height: BlockHeight,
}

/// Committed block range carried by a [`ChainReorgedEvent`] or
/// [`ChainCommittedEvent`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CommittedRange {
    /// Chain epoch that contains the committed range.
    pub chain_epoch: ChainEpoch,
    /// First committed block height (inclusive).
    pub start_height: BlockHeight,
    /// Last committed block height (inclusive).
    pub end_height: BlockHeight,
}

/// Trait every chain-events derive consumer implements.
///
/// The SDK dispatcher calls [`apply_chain_committed`](Self::apply_chain_committed)
/// and [`apply_chain_reorged`](Self::apply_chain_reorged) per envelope.
/// Consumers stage their state writes through the
/// [`DeriveConsumerCtx::batch`] handle so the SDK can commit consumer
/// writes and the cursor advance atomically.
///
/// Most production consumers implement [`BlockKeyedConsumer`] instead;
/// a blanket impl gives them the per-height range-loop on top of this
/// trait so they only write per-block logic, never the range scaffolding.
#[async_trait]
pub trait DeriveConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;

    /// Apply a committed range. Stage state writes into `ctx.batch`; the SDK
    /// adds the cursor advance and commits atomically.
    async fn apply_chain_committed(
        &mut self,
        event: &ChainCommittedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;

    /// Apply a reorged event. Stage state writes into `ctx.batch`; the SDK
    /// adds the cursor advance and commits atomically. Implementations
    /// decide how to revert their derived state for the reverted range and
    /// how to fold in the replacement range.
    async fn apply_chain_reorged(
        &mut self,
        event: &ChainReorgedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;
}

/// Per-block derive consumer.
///
/// The convention every production chain-events consumer follows. The
/// blanket [`DeriveConsumer`] impl below walks the height range from each
/// envelope and calls [`apply_block`](Self::apply_block) /
/// [`revert_block`](Self::revert_block) per height, pulling parsed blocks
/// from the shared [`BlockSource`] returned by
/// [`block_source`](Self::block_source). Per-envelope fan-out across N
/// consumers fetches and parses each block exactly once because the
/// `BlockSource` cache is shared.
///
/// A consumer that observes range boundaries (or implements something
/// other than "one block in, some rows out") implements [`DeriveConsumer`]
/// directly instead.
#[async_trait]
pub trait BlockKeyedConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;

    /// Shared block source the dispatcher pulls parsed blocks from. Every
    /// consumer in a process should be wired with the same `BlockSource`
    /// clone so the cache dedupes their fan-out.
    fn block_source(&self) -> &BlockSource;

    /// Stages per-height writes derived from `block`. Implementations write
    /// into `ctx.batch`; the SDK appends the cursor advance and commits
    /// atomically.
    async fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;

    /// Stages per-height deletes to revert state previously written for
    /// `height`. Called once per reverted height by the blanket
    /// [`DeriveConsumer::apply_chain_reorged`] impl.
    async fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;

    /// Whether this consumer's `apply_block` reads transparent prevouts.
    ///
    /// When `true`, the blanket pipeline primes
    /// [`BlockCommitContext::prevouts`] inside the fetch lookahead so the
    /// apply path never blocks on a cold `OnceCell`. Default is `false`;
    /// consumers that call `block.prevouts().await` from `apply_block`
    /// override this to `true`. Prefetching when the consumer never
    /// consults prevouts would waste the prevouts channel's RPC budget.
    fn prefetch_prevouts(&self) -> bool {
        false
    }
}

/// Maximum number of in-flight `BlockSource::block` futures the blanket
/// pipeline keeps queued ahead of the apply loop.
///
/// Sized to match `BlockSource`'s cache capacity: prefetching beyond that
/// just evicts entries before the apply loop reaches them.
const PIPELINE_DEPTH: usize = 32;

#[async_trait]
impl<C: BlockKeyedConsumer + ?Sized> DeriveConsumer for C {
    fn name(&self) -> DeriveConsumerName {
        BlockKeyedConsumer::name(self)
    }

    async fn apply_chain_committed(
        &mut self,
        event: &ChainCommittedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        let block_source = self.block_source().clone();
        let prefetch_prevouts = self.prefetch_prevouts();
        let heights = (event.start_height.value()..=event.end_height.value()).map(BlockHeight::new);
        let mut fetch_stream = futures_util::stream::iter(heights)
            .map(|height| {
                let block_source = block_source.clone();
                async move {
                    let context = block_source.block(height).await?;
                    if prefetch_prevouts && let Some(context) = &context {
                        // Prime the OnceCell while the apply loop is still
                        // processing earlier heights. Errors are ignored
                        // here because apply_block's prevouts().await call
                        // will surface them after retrying the cell.
                        let _ = context.prevouts().await;
                    }
                    Ok::<_, BlockCommitContextError>(context)
                }
            })
            .buffered(PIPELINE_DEPTH);
        while let Some(fetched) = fetch_stream.next().await {
            let context = fetched.map_err(|error| Box::new(error) as DeriveConsumerError)?;
            if let Some(context) = context {
                self.apply_block(&context, ctx).await?;
            }
        }
        Ok(())
    }

    async fn apply_chain_reorged(
        &mut self,
        event: &ChainReorgedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError> {
        for raw_height in event.reverted.start_height.value()..=event.reverted.end_height.value() {
            self.revert_block(BlockHeight::new(raw_height), ctx).await?;
        }
        let block_source = self.block_source().clone();
        let prefetch_prevouts = self.prefetch_prevouts();
        let heights = (event.replacement.start_height.value()
            ..=event.replacement.end_height.value())
            .map(BlockHeight::new);
        let mut fetch_stream = futures_util::stream::iter(heights)
            .map(|height| {
                let block_source = block_source.clone();
                async move {
                    let context = block_source.block(height).await?;
                    if prefetch_prevouts && let Some(context) = &context {
                        let _ = context.prevouts().await;
                    }
                    Ok::<_, BlockCommitContextError>(context)
                }
            })
            .buffered(PIPELINE_DEPTH);
        while let Some(fetched) = fetch_stream.next().await {
            let context = fetched.map_err(|error| Box::new(error) as DeriveConsumerError)?;
            if let Some(context) = context {
                self.apply_block(&context, ctx).await?;
            }
        }
        Ok(())
    }
}

/// Mempool-event consumer trait.
///
/// Separate from [`DeriveConsumer`] because mempool events have different
/// retention, ordering, and semantic content than chain events. A consumer
/// can implement both traits if it observes both streams; the explorer
/// transparent-balance handler implements neither because it reads canonical
/// UTXOs and live mempool point lookups at request time.
#[async_trait]
pub trait DeriveMempoolConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;

    /// Apply a mempool event. Stage state writes into `ctx.batch`; the SDK
    /// adds the cursor advance and commits atomically.
    async fn apply_mempool_event(
        &mut self,
        event: &MempoolConsumerEvent<'_>,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;
}

/// Typed wrapper for a single `MempoolEventEnvelope` delivered to a consumer.
#[derive(Debug)]
#[non_exhaustive]
pub struct MempoolConsumerEvent<'a> {
    /// Monotonic mempool-event sequence.
    pub event_sequence: u64,
    /// Wall-clock observation timestamp from the source.
    pub source_observed_unix_millis: u64,
    /// Variant payload borrowed from the wire envelope.
    pub variant: MempoolConsumerEventVariant<'a>,
}

/// Typed payload variant carried by [`MempoolConsumerEvent`].
#[derive(Debug)]
#[non_exhaustive]
pub enum MempoolConsumerEventVariant<'a> {
    /// New mempool transaction observed by the source.
    Added {
        /// Transaction id observed by the source.
        transaction_id: &'a [u8],
        /// Hydrated raw transaction bytes (when the source provides them).
        raw_transaction_bytes: &'a [u8],
    },
    /// Mempool transaction removed without being mined.
    Invalidated {
        /// Transaction id of the invalidated transaction.
        transaction_id: &'a [u8],
    },
    /// Mempool transaction observed mined into a block.
    Mined {
        /// Transaction id of the mined transaction.
        transaction_id: &'a [u8],
        /// Height of the mining block.
        mined_height: BlockHeight,
        /// Hash of the mining block (32 bytes).
        block_hash: &'a [u8],
    },
    /// Upstream node refused admission of the transaction. Reserved for
    /// ZIP-401 `RecentlyEvicted`; source-side emission is pending node-side
    /// visibility as documented by the mempool topology.
    Suppressed {
        /// Transaction id of the suppressed transaction.
        transaction_id: &'a [u8],
    },
}
