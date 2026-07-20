//! Per-block canonical fact view shared across materialized-view consumers.
//!
//! [`BlockCommitContext`] is hydrated from the canonical store:
//! block-header facts, ordered transaction facts, and resolved transparent
//! outputs for the block's transparent inputs. Materialized-view replay no longer depends
//! on raw block blobs or `zebra-chain` parsing, so startup replay and
//! steady-state tailing use the same typed input shape.

use std::collections::HashMap;
use std::sync::Arc;

use zinder_core::{
    BlockFinalNoteCommitmentRoots, BlockHash, BlockHeight, TransactionFactsArtifact, TransactionId,
    TransactionIntrinsicValueBalances, TransparentOutPoint, TransparentSpendFact, ValuePoolBalance,
};

/// Hydrated transparent spend facts available to materialized-view consumers.
///
/// `Offline` short-circuits to `None` so consumers can emit explicit
/// `UNAVAILABLE` records when transparent-spend hydration is disabled. `Static`
/// is the writer-owned path: ingest resolves every event-scoped spend from the
/// canonical store and supplies the map directly.
#[derive(Clone)]
pub enum TransparentSpendFacts {
    /// Transparent-spend hydration is not available.
    Offline,
    /// In-process hydration: the caller provides the precomputed spend map.
    Static(Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>),
}

/// Canonical intrinsic value-balance artifacts available to materialized-view consumers.
///
/// Row absence is distinct from an all-zero value balance. Consumers that
/// require exact fees must preserve that distinction as explicit unavailable
/// output rather than assuming zero balances.
#[derive(Clone)]
pub enum TransactionIntrinsicValueBalanceFacts {
    /// Intrinsic-value-balance hydration is not available.
    Offline,
    /// In-process hydration keyed by canonical transaction identifier.
    Static(Arc<HashMap<TransactionId, TransactionIntrinsicValueBalances>>),
}

impl TransactionIntrinsicValueBalanceFacts {
    /// Wraps a precomputed intrinsic-value-balance map into the `Static` variant.
    #[must_use]
    pub fn from_map(map: Arc<HashMap<TransactionId, TransactionIntrinsicValueBalances>>) -> Self {
        Self::Static(map)
    }
}

/// Canonical chain-wide value-pool snapshot available to materialized-view consumers.
///
/// The list remains dynamic and preserves upstream order. `Offline` is
/// distinct from an authoritative empty snapshot.
#[derive(Clone)]
pub enum BlockValuePoolBalanceFacts {
    /// Chain value-pool hydration is not available.
    Offline,
    /// In-process hydration from the authoritative snapshot for this block.
    Static(Arc<Vec<ValuePoolBalance>>),
}

impl BlockValuePoolBalanceFacts {
    /// Wraps an authoritative list-shaped chain value-pool snapshot.
    #[must_use]
    pub fn from_pools(pools: Vec<ValuePoolBalance>) -> Self {
        Self::Static(Arc::new(pools))
    }
}

impl std::fmt::Debug for BlockValuePoolBalanceFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => formatter.write_str("BlockValuePoolBalanceFacts::Offline"),
            Self::Static(pools) => {
                write!(
                    formatter,
                    "BlockValuePoolBalanceFacts::Static({})",
                    pools.len()
                )
            }
        }
    }
}

impl std::fmt::Debug for TransactionIntrinsicValueBalanceFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => formatter.write_str("TransactionIntrinsicValueBalanceFacts::Offline"),
            Self::Static(map) => write!(
                formatter,
                "TransactionIntrinsicValueBalanceFacts::Static({})",
                map.len()
            ),
        }
    }
}

impl TransparentSpendFacts {
    /// Wraps a precomputed spend-fact map into the `Static` variant.
    #[must_use]
    pub fn from_map(map: Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>) -> Self {
        Self::Static(map)
    }
}

impl std::fmt::Debug for TransparentSpendFacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => formatter.write_str("TransparentSpendFacts::Offline"),
            Self::Static(map) => write!(formatter, "TransparentSpendFacts::Static({})", map.len()),
        }
    }
}

/// Per-block typed fact view threaded through consumer `apply_block` calls.
pub struct BlockCommitContext {
    /// Height the context describes.
    pub height: BlockHeight,
    /// Canonical block hash.
    pub block_hash: BlockHash,
    /// Canonical previous-block hash.
    pub previous_block_hash: BlockHash,
    /// Block-time as Unix seconds.
    pub block_time_unix_seconds: i64,
    /// Serialized consensus block size in bytes.
    pub block_size_bytes: u64,
    /// Ordered transaction facts for every transaction in the block.
    pub transactions: Vec<TransactionFactsArtifact>,
    /// Typed final note-commitment roots when canonical enrichment is present.
    pub final_note_commitment_roots: Option<BlockFinalNoteCommitmentRoots>,
    transparent_spends: TransparentSpendFacts,
    transaction_intrinsic_value_balances: TransactionIntrinsicValueBalanceFacts,
    block_value_pool_balances: BlockValuePoolBalanceFacts,
}

/// Canonical block input [`BlockCommitContext::new`] takes by value.
pub struct BlockCommitInput {
    /// Height of the block.
    pub height: BlockHeight,
    /// Canonical block hash.
    pub block_hash: BlockHash,
    /// Canonical previous-block hash.
    pub previous_block_hash: BlockHash,
    /// Block-time as Unix seconds.
    pub block_time_unix_seconds: i64,
    /// Serialized consensus block size in bytes.
    pub block_size_bytes: u64,
    /// Ordered transaction facts for every transaction in the block.
    pub transactions: Vec<TransactionFactsArtifact>,
    /// Typed final note-commitment roots when canonical enrichment is present.
    pub final_note_commitment_roots: Option<BlockFinalNoteCommitmentRoots>,
}

impl BlockCommitContext {
    /// Builds a context from canonical block and transaction facts.
    #[must_use]
    pub fn new(input: BlockCommitInput, transparent_spends: TransparentSpendFacts) -> Self {
        Self {
            height: input.height,
            block_hash: input.block_hash,
            previous_block_hash: input.previous_block_hash,
            block_time_unix_seconds: input.block_time_unix_seconds,
            block_size_bytes: input.block_size_bytes,
            transactions: input.transactions,
            final_note_commitment_roots: input.final_note_commitment_roots,
            transparent_spends,
            transaction_intrinsic_value_balances: TransactionIntrinsicValueBalanceFacts::Offline,
            block_value_pool_balances: BlockValuePoolBalanceFacts::Offline,
        }
    }

    /// Attaches canonical per-transaction intrinsic value balances.
    #[must_use]
    pub fn with_transaction_intrinsic_value_balances(
        mut self,
        transaction_intrinsic_value_balances: TransactionIntrinsicValueBalanceFacts,
    ) -> Self {
        self.transaction_intrinsic_value_balances = transaction_intrinsic_value_balances;
        self
    }

    /// Attaches the authoritative chain-wide value-pool snapshot for this block.
    #[must_use]
    pub fn with_block_value_pool_balances(
        mut self,
        block_value_pool_balances: BlockValuePoolBalanceFacts,
    ) -> Self {
        self.block_value_pool_balances = block_value_pool_balances;
        self
    }

    /// Returns the hydrated transparent spend map for the block's transparent inputs.
    ///
    /// `None` means the binary configured [`TransparentSpendFacts::Offline`].
    /// `Some(map)` contains every transparent input the store can identify;
    /// the map may be missing individual outpoints when the source block
    /// references an output outside retained canonical facts.
    #[must_use]
    pub fn transparent_spends(
        &self,
    ) -> Option<Arc<HashMap<TransparentOutPoint, TransparentSpendFact>>> {
        match &self.transparent_spends {
            TransparentSpendFacts::Offline => None,
            TransparentSpendFacts::Static(map) => Some(Arc::clone(map)),
        }
    }

    /// Returns canonical intrinsic value balances keyed by transaction id.
    ///
    /// `None` means the caller did not hydrate this artifact family.
    /// `Some(map)` preserves missing per-transaction rows as map absence.
    #[must_use]
    pub fn transaction_intrinsic_value_balances(
        &self,
    ) -> Option<Arc<HashMap<TransactionId, TransactionIntrinsicValueBalances>>> {
        match &self.transaction_intrinsic_value_balances {
            TransactionIntrinsicValueBalanceFacts::Offline => None,
            TransactionIntrinsicValueBalanceFacts::Static(map) => Some(Arc::clone(map)),
        }
    }

    /// Returns the authoritative chain-wide value-pool snapshot for this block.
    ///
    /// `None` means this artifact family was not hydrated. `Some(_)`
    /// preserves an authoritative empty pool list.
    #[must_use]
    pub fn block_value_pool_balances(&self) -> Option<Arc<Vec<ValuePoolBalance>>> {
        match &self.block_value_pool_balances {
            BlockValuePoolBalanceFacts::Offline => None,
            BlockValuePoolBalanceFacts::Static(pools) => Some(Arc::clone(pools)),
        }
    }
}
