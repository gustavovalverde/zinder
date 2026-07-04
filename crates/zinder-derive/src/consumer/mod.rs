//! Derive-consumer traits, typed event wrappers, and in-process dispatch helpers.
//!
//! Every derive consumer implements [`DeriveConsumer`]. The trait is the seam
//! between the consumer-agnostic infrastructure in this crate (store and
//! typed event wrappers) and the consumer-specific aggregation logic that
//! lives in each consumer module.
//!
//! A consumer that needs the parsed block in `apply_block` does NOT fetch
//! raw block data itself. `zinder-ingest` constructs a shared
//! [`BlockCommitContext`] from the canonical commit and passes it to every
//! consumer observing that height.

pub(crate) mod address_value_event;
pub(crate) mod block_commit_context;
pub(crate) mod block_summary;
pub(crate) mod mempool_event_counts;
pub(crate) mod recent_transactions;
pub(crate) mod transaction_fees;
pub(crate) mod transparent_address_activity;
pub(crate) mod transparent_address_deltas;
pub(crate) mod transparent_address_transaction_history;
pub(crate) mod transparent_outpoint_spend;

use std::collections::HashMap;

use rust_rocksdb::WriteBatch;
use zinder_core::{BlockHeight, ChainEpoch};

pub use block_commit_context::{
    BlockCommitContext, BlockCommitContextError, BlockCommitPayload, TransparentSpendFacts,
};

use crate::store::DeriveStore;

/// Stable name of a derive consumer used to scope cursor and metadata rows.
///
/// The name is part of the on-disk key in the derive cursor column families;
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

/// A derive consumer's on-disk schema declaration.
///
/// One declaration binds a consumer's stable [`DeriveConsumerName`] to the
/// version of its column-family layout and the set of column families it
/// owns. The derive store records the declared version per consumer and
/// scopes wipe-and-rebuild to the single consumer whose declared version no
/// longer matches the recorded one, leaving every other consumer's rows and
/// cursor untouched. Bumping [`schema_version`](Self::schema_version) is the
/// signal to drop and rebuild this consumer's column families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DeriveConsumerSchema {
    /// Stable consumer identity, shared with the consumer's cursor rows.
    pub name: DeriveConsumerName,
    /// Version of this consumer's column-family layout and payload encoding.
    pub schema_version: u16,
    /// Column families this consumer reads and writes.
    pub column_families: &'static [&'static str],
}

impl DeriveConsumerSchema {
    /// Declares a consumer's schema from its name, version, and owned column
    /// families. Requiring the version here makes it impossible to register a
    /// column family without one.
    #[must_use]
    pub const fn new(
        name: DeriveConsumerName,
        schema_version: u16,
        column_families: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            schema_version,
            column_families,
        }
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
    /// Safe tip block height that was true at delivery time.
    pub safe_tip_height: BlockHeight,
    /// First committed block height (inclusive).
    pub start_height: BlockHeight,
    /// Last committed block height (inclusive).
    pub end_height: BlockHeight,
}

impl ChainCommittedEvent {
    /// Builds a chain-committed event from its component fields.
    #[must_use]
    pub const fn new(
        event_sequence: u64,
        chain_epoch: ChainEpoch,
        safe_tip_height: BlockHeight,
        start_height: BlockHeight,
        end_height: BlockHeight,
    ) -> Self {
        Self {
            event_sequence,
            chain_epoch,
            safe_tip_height,
            start_height,
            end_height,
        }
    }
}

/// Typed wrapper for a `ChainReorged` chain event delivered to a consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChainReorgedEvent {
    /// Monotonic event sequence the SDK uses for cursor accounting.
    pub event_sequence: u64,
    /// Chain epoch visible after the reorg replacement commits.
    pub chain_epoch: ChainEpoch,
    /// Safe tip block height that was true at delivery time.
    pub safe_tip_height: BlockHeight,
    /// Range invalidated by the reorg.
    pub reverted: RevertedRange,
    /// Replacement range committed by the reorg.
    pub replacement: CommittedRange,
}

impl ChainReorgedEvent {
    /// Builds a chain-reorged event from its component fields.
    #[must_use]
    pub const fn new(
        event_sequence: u64,
        chain_epoch: ChainEpoch,
        safe_tip_height: BlockHeight,
        reverted: RevertedRange,
        replacement: CommittedRange,
    ) -> Self {
        Self {
            event_sequence,
            chain_epoch,
            safe_tip_height,
            reverted,
            replacement,
        }
    }
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

impl RevertedRange {
    /// Builds a reverted range from its component fields.
    #[must_use]
    pub const fn new(
        chain_epoch: ChainEpoch,
        start_height: BlockHeight,
        end_height: BlockHeight,
    ) -> Self {
        Self {
            chain_epoch,
            start_height,
            end_height,
        }
    }
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

impl CommittedRange {
    /// Builds a committed range from its component fields.
    #[must_use]
    pub const fn new(
        chain_epoch: ChainEpoch,
        start_height: BlockHeight,
        end_height: BlockHeight,
    ) -> Self {
        Self {
            chain_epoch,
            start_height,
            end_height,
        }
    }
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
pub trait DeriveConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;

    /// Apply a committed range. Stage state writes into `ctx.batch`; the SDK
    /// adds the cursor advance and commits atomically.
    fn apply_chain_committed(
        &mut self,
        event: &ChainCommittedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;

    /// Apply a reorged event. Stage state writes into `ctx.batch`; the SDK
    /// adds the cursor advance and commits atomically. Implementations
    /// decide how to revert their derived state for the reverted range and
    /// how to fold in the replacement range.
    fn apply_chain_reorged(
        &mut self,
        event: &ChainReorgedEvent,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;
}

/// Per-block derive consumer.
///
/// The convention every production chain-events consumer follows. The
/// in-process dispatchers
/// ([`apply_chain_committed_in_memory`], [`apply_chain_reorged_in_memory`])
/// walk a height range and call [`apply_block`](Self::apply_block) /
/// [`revert_block`](Self::revert_block) per height with already-parsed
/// [`BlockCommitContext`] inputs the writer holds in memory.
///
/// A consumer that observes range boundaries (or implements something
/// other than "one block in, some rows out") implements [`DeriveConsumer`]
/// directly instead.
pub trait BlockKeyedConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;

    /// Stages per-height writes derived from `block`. Implementations write
    /// into `ctx.batch`; the SDK appends the cursor advance and commits
    /// atomically.
    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;

    /// Stages per-height deletes to revert state previously written for
    /// `height`. Called once per reverted height by
    /// [`apply_chain_reorged_in_memory`].
    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut DeriveConsumerCtx<'_>,
    ) -> Result<(), DeriveConsumerError>;
}

/// Dispatches a committed range against a [`BlockKeyedConsumer`] from
/// already-parsed block contexts.
///
/// Used when the consumer runs colocated with the writer that already has
/// the parsed block in memory. Each height in `event` must have a matching
/// entry in `blocks`; heights without entries are skipped so callers can
/// reuse the helper while assembling partial in-memory windows.
///
/// # Errors
///
/// Surfaces any [`DeriveConsumerError`] the consumer's `apply_block`
/// returns. The caller bundles the SDK's cursor advance into `ctx.batch`
/// and commits the batch atomically.
pub fn apply_chain_committed_in_memory<C, S>(
    consumer: &mut C,
    event: &ChainCommittedEvent,
    ctx: &mut DeriveConsumerCtx<'_>,
    blocks: &HashMap<BlockHeight, std::sync::Arc<BlockCommitContext>, S>,
) -> Result<(), DeriveConsumerError>
where
    C: BlockKeyedConsumer + ?Sized,
    S: std::hash::BuildHasher,
{
    for raw_height in event.start_height.value()..=event.end_height.value() {
        let height = BlockHeight::new(raw_height);
        if let Some(context) = blocks.get(&height) {
            consumer.apply_block(context.as_ref(), ctx)?;
        }
    }
    Ok(())
}

/// Reverts a reorged range, then applies the replacement range from
/// already-parsed contexts; in-process counterpart of the gRPC blanket impl.
///
/// # Errors
///
/// Surfaces any [`DeriveConsumerError`] the consumer's `revert_block` or
/// `apply_block` returns.
pub fn apply_chain_reorged_in_memory<C, S>(
    consumer: &mut C,
    event: &ChainReorgedEvent,
    ctx: &mut DeriveConsumerCtx<'_>,
    replacement_blocks: &HashMap<BlockHeight, std::sync::Arc<BlockCommitContext>, S>,
) -> Result<(), DeriveConsumerError>
where
    C: BlockKeyedConsumer + ?Sized,
    S: std::hash::BuildHasher,
{
    for raw_height in event.reverted.start_height.value()..=event.reverted.end_height.value() {
        consumer.revert_block(BlockHeight::new(raw_height), ctx)?;
    }
    for raw_height in event.replacement.start_height.value()..=event.replacement.end_height.value()
    {
        let height = BlockHeight::new(raw_height);
        if let Some(context) = replacement_blocks.get(&height) {
            consumer.apply_block(context.as_ref(), ctx)?;
        }
    }
    Ok(())
}

/// Mempool-event consumer trait.
///
/// Separate from [`DeriveConsumer`] because mempool events have different
/// retention, ordering, and semantic content than chain events. A consumer
/// can implement both traits if it observes both streams; the explorer
/// transparent-balance handler implements neither because it reads canonical
/// UTXOs and live mempool point lookups at request time.
pub trait DeriveMempoolConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;

    /// Apply a mempool event. Stage state writes into `ctx.batch`; the SDK
    /// adds the cursor advance and commits atomically.
    fn apply_mempool_event(
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

impl<'a> MempoolConsumerEvent<'a> {
    /// Builds a mempool-consumer event from its sequence, timestamp, and
    /// typed payload.
    #[must_use]
    pub const fn new(
        event_sequence: u64,
        source_observed_unix_millis: u64,
        variant: MempoolConsumerEventVariant<'a>,
    ) -> Self {
        Self {
            event_sequence,
            source_observed_unix_millis,
            variant,
        }
    }
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
