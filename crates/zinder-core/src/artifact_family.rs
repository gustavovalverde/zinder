//! Canonical artifact-family labels for the [`IndexerError::ArtifactUnavailable`]
//! variant declared in `zinder_client::IndexerError`.
//!
//! Constants live in `zinder-core` so every crate that constructs or matches
//! on the typed error variant uses the same string literal. The CI walker at
//! `crates/zinder-proto/tests/integration/gap_doc_walker.rs` verifies the
//! variant carries the canonical refusal of the `IndexerError::NotFound`
//! generic-resource collapse.
//!
//! [`IndexerError::ArtifactUnavailable`]: https://docs.rs/zinder-client/latest/zinder_client/enum.IndexerError.html#variant.ArtifactUnavailable

/// Chain-epoch metadata.
pub const CHAIN_EPOCH: &str = "chain_epoch";
/// Chain-event envelope.
pub const CHAIN_EVENT: &str = "chain_event";
/// Finalized full-block artifact.
pub const FINALIZED_BLOCK: &str = "finalized_block";
/// Compact-block artifact.
pub const COMPACT_BLOCK: &str = "compact_block";
/// Mined transaction artifact.
pub const MINED_TRANSACTION: &str = "mined_transaction";
/// Commitment tree-state artifact.
pub const TREE_STATE: &str = "tree_state";
/// Commitment subtree-root artifact.
pub const SUBTREE_ROOT: &str = "subtree_root";
/// Transparent address UTXO artifact.
pub const TRANSPARENT_ADDRESS_UTXO: &str = "transparent_address_utxo";
/// Transparent UTXO spend artifact.
pub const TRANSPARENT_UTXO_SPEND: &str = "transparent_utxo_spend";
/// Transparent prevout (output referenced by an outpoint).
pub const TRANSPARENT_PREVOUT: &str = "transparent_prevout";
/// Transparent address tx-history index artifact.
pub const TRANSPARENT_ADDRESS_TX_INDEX: &str = "transparent_address_tx_index";
/// Best-chain block-hash to height index entry.
pub const BLOCK_HASH_INDEX: &str = "block_hash_index";
/// Live mempool entry.
pub const MEMPOOL_ENTRY: &str = "mempool_entry";
/// Mempool event envelope.
pub const MEMPOOL_EVENT: &str = "mempool_event";
/// Block header read model.
pub const BLOCK_HEADER: &str = "block_header";
