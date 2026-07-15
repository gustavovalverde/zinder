//! Canonical artifact-family labels for the [`IndexerError::ArtifactUnavailable`]
//! variant declared in `zinder_client::IndexerError`.
//!
//! Constants live in `zinder-core` so every crate that constructs or matches
//! on the typed error variant uses the same string literal.
//!
//! [`IndexerError::ArtifactUnavailable`]: https://docs.rs/zinder-client/latest/zinder_client/enum.IndexerError.html#variant.ArtifactUnavailable

/// Chain-epoch metadata.
pub const CHAIN_EPOCH: &str = "chain_epoch";
/// Chain-event envelope.
pub const CHAIN_EVENT: &str = "chain_event";
/// Canonical block-header facts.
pub const BLOCK_HEADER_ARTIFACT: &str = "block_header";
/// Complete semantic facts needed to replay one canonical block.
pub const BLOCK_REPLAY: &str = "block_replay";
/// Optional raw block blob.
pub const BLOCK_BLOB: &str = "block_blob";
/// Compact-block artifact.
pub const COMPACT_BLOCK: &str = "compact_block";
/// Block-local transaction id index.
pub const BLOCK_TRANSACTION_INDEX: &str = "block_transaction_index";
/// Mined transaction location.
pub const TRANSACTION_LOCATION: &str = "transaction_location";
/// Canonical transaction facts.
pub const TRANSACTION_FACTS: &str = "transaction_facts";
/// Optional transaction-intrinsic shielded value balances.
pub const TRANSACTION_INTRINSIC_VALUE_BALANCES: &str = "transaction_intrinsic_value_balances";
/// Optional cumulative value-pool balances after a canonical block.
pub const BLOCK_VALUE_POOL_BALANCES: &str = "block_value_pool_balances";
/// Optional raw transaction blob.
pub const TRANSACTION_BLOB: &str = "transaction_blob";
/// Commitment tree-state artifact.
pub const TREE_STATE: &str = "tree_state";
/// Commitment subtree-root artifact.
pub const SUBTREE_ROOT: &str = "subtree_root";
/// Transparent address output artifact.
pub const ADDRESS_OUTPUT_INDEX: &str = "address_output_index";
/// Resolved transparent spend fact.
pub const TRANSPARENT_SPEND_FACT: &str = "transparent_spend_fact";
/// Transparent output referenced by an outpoint.
pub const TRANSPARENT_OUTPUT: &str = "transparent_output";
/// Transparent address tx-history index artifact.
pub const TRANSPARENT_ADDRESS_TX_INDEX: &str = "transparent_address_tx_index";
/// Best-chain block-hash to height index entry.
pub const BLOCK_HASH_INDEX: &str = "block_hash_index";
/// Block displaced from the canonical branch.
pub const DISPLACED_BLOCK: &str = "displaced_block";
/// Live mempool entry.
pub const MEMPOOL_ENTRY: &str = "mempool_entry";
/// Mempool event envelope.
pub const MEMPOOL_EVENT: &str = "mempool_event";
/// Block header read model.
pub const BLOCK_HEADER: &str = "block_header";
