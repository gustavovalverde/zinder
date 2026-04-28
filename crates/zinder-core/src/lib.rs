//! Core Zinder domain values shared across storage and service boundaries.
//!
//! This crate intentionally owns chain vocabulary such as [`ChainEpoch`],
//! [`BlockArtifact`], and [`CompactBlockArtifact`] without depending on a
//! storage engine, node client, or wallet protocol crate.

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("Zinder supports only 32-bit and wider targets.");

mod block_artifact;
mod block_id;
mod chain_epoch;
mod subtree_root;
mod transaction;
mod transparent_utxo;
mod tree_state;

pub use block_artifact::{BlockArtifact, CompactBlockArtifact};
pub use block_id::BlockId;
pub use chain_epoch::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, BlockHeightRangeIter,
    ChainEpoch, ChainEpochId, ChainTipMetadata, Network, UnixTimestampMillis,
};
pub use subtree_root::{
    SUBTREE_LEAF_COUNT, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex,
    SubtreeRootRange, SubtreeRootRangeIter,
};
pub use transaction::{
    BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastRejected,
    BroadcastUnknown, RawTransactionBytes, TransactionArtifact, TransactionBroadcastResult,
    TransactionId,
};
pub use transparent_utxo::{
    TransparentAddressScriptHash, TransparentAddressUtxoArtifact, TransparentOutPoint,
    TransparentUtxoSpendArtifact,
};
pub use tree_state::TreeStateArtifact;
