//! Core Zinder domain values shared across storage and service boundaries.
//!
//! This crate intentionally owns chain vocabulary such as [`ChainEpoch`],
//! [`BlockHeaderArtifact`], and [`CompactBlockArtifact`] without depending on a
//! storage engine, node client, or wallet protocol crate.

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("Zinder supports only 32-bit and wider targets.");

pub mod artifact_family;
mod block_artifact;
mod block_header;
mod block_id;
mod chain_epoch;
mod chain_value_pools;
pub mod explorer_reasons;
pub mod explorer_search;
mod mempool;
mod network_upgrade_activations;
mod subtree_root;
mod transaction;
mod transaction_public_facts;
mod transparent_address_balance;
mod transparent_address_tx_index;
mod transparent_output;
mod transparent_utxo_set_summary;
mod tree_state;
pub mod wire;

pub use block_artifact::{
    BlockBlobArtifact, BlockHeaderArtifact, BlockTransactionIndexArtifact, CompactBlockArtifact,
};
pub use block_header::BlockHeaderInfo;
pub use block_id::{BlockId, BlockSelector};
pub use chain_epoch::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, BlockHeightRangeIter,
    ChainEpoch, ChainEpochId, ChainTipMetadata, Network, UnixTimestampMillis,
};
pub use chain_value_pools::{ChainValuePool, ChainValuePools, ChainValuePoolsAtTip};
pub use mempool::{
    MempoolEntry, MempoolEvictionReason, TransparentMempoolOutput,
    TransparentMempoolOutputsRequest, TransparentMempoolSpend,
};
pub use network_upgrade_activations::{
    ConsensusBranchId, NetworkUpgradeActivation, NetworkUpgradeActivations,
    NetworkUpgradeActivationsError,
};
pub use subtree_root::{
    SUBTREE_LEAF_COUNT, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex,
    SubtreeRootRange, SubtreeRootRangeIter,
};
pub use transaction::{
    AuthDigest, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastQueued,
    BroadcastRejected, BroadcastRejectionReason, BroadcastUnknown, MAX_RAW_TRANSACTION_BYTES,
    MinedDetails, MinedTransaction, RawTransactionBytes, TransactionBlobArtifact,
    TransactionBroadcastResult, TransactionFactsArtifact, TransactionId, TransactionLocation,
    TxStatus,
};
pub use transaction_public_facts::{
    LockTime, PrivacyShape, TransactionComponentCounts, TransactionPublicFacts, TransactionVersion,
    UnsupportedSection, Wtxid, classify_privacy_shape,
};
pub use transparent_address_balance::TransparentAddressBalance;
pub use transparent_address_tx_index::TransparentAddressTxIndexArtifact;
pub use transparent_output::{
    MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, TransparentAddressScriptHash, TransparentInputFact,
    TransparentOutPoint, TransparentOutput, TransparentOutputArtifact, TransparentOutputEntry,
    TransparentOutputFact, TransparentOutputsByOutpointResponse, TransparentSpendEntry,
    TransparentSpendFact, TransparentSpendsByOutpointResponse, TransparentUnspentOutput,
    TransparentUnspentOutputsByOutpointResponse,
};
pub use transparent_utxo_set_summary::TransparentUtxoSetSummary;
pub use tree_state::TreeStateArtifact;
