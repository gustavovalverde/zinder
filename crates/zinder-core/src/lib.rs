//! Core Zinder domain values shared across storage and service boundaries.
//!
//! This crate intentionally owns chain vocabulary such as [`ChainEpoch`],
//! [`BlockArtifact`], and [`CompactBlockArtifact`] without depending on a
//! storage engine, node client, or wallet protocol crate.

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("Zinder supports only 32-bit and wider targets.");

pub mod artifact_family;
mod block_artifact;
mod block_header;
mod block_id;
mod chain_epoch;
mod mempool;
mod subtree_root;
mod transaction;
mod transparent_address_balance;
mod transparent_address_tx_index;
mod transparent_prevout;
mod transparent_utxo;
mod tree_state;

pub use block_artifact::{BlockArtifact, CompactBlockArtifact};
pub use block_header::BlockHeaderInfo;
pub use block_id::{BlockId, BlockSelector};
pub use chain_epoch::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, BlockHeightRangeIter,
    ChainEpoch, ChainEpochId, ChainTipMetadata, Network, UnixTimestampMillis,
};
pub use mempool::{
    MempoolEntry, MempoolEvictionReason, TransparentMempoolOutput,
    TransparentMempoolOutputsRequest, TransparentMempoolSpend,
};
pub use subtree_root::{
    SUBTREE_LEAF_COUNT, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex,
    SubtreeRootRange, SubtreeRootRangeIter,
};
pub use transaction::{
    AuthDigest, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastRejected,
    BroadcastUnknown, MinedDetails, MinedTransaction, RawTransactionBytes, TransactionArtifact,
    TransactionBroadcastResult, TransactionId, TxStatus,
};
pub use transparent_address_balance::TransparentAddressBalance;
pub use transparent_address_tx_index::TransparentAddressTxIndexArtifact;
pub use transparent_prevout::{
    MAX_TRANSPARENT_PREVOUTS_PER_REQUEST, TransparentPrevout, TransparentPrevoutEntry,
    TransparentPrevoutsResponse,
};
pub use transparent_utxo::{
    TransparentAddressScriptHash, TransparentAddressUtxoArtifact, TransparentOutPoint,
    TransparentUtxoSpendArtifact,
};
pub use tree_state::TreeStateArtifact;
