//! Typed Rust client surface for Zinder chain-index consumers.
//!
//! `zinder-client` is the public Rust API that wallet daemons and application
//! code import. It keeps `RocksDB`, `tonic`, and generated protobuf types behind
//! typed domain methods so downstream consumers do not need to couple to
//! Zinder service internals.

mod chain_index;
mod error;
mod local;
mod remote;

pub use chain_index::{
    ChainEpochCommitted, ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventStream,
    ChainIndex, ChainRangeReverted, IndexStream, MempoolEvent, MempoolEventCursor,
    MempoolEventEnvelope, MempoolEventStream, MempoolSnapshotCursor, MempoolSnapshotRequest,
    MempoolSnapshotView, TransparentAddressTxIdsQuery, TransparentAddressTxIdsStream,
    TransparentAddressTxIdsStreamItem, TransparentAddressUtxoStream,
    TransparentAddressUtxoStreamItem, TransparentAddressUtxosQuery, TransparentAddressUtxosView,
    TransparentHistoryCursor, TransparentUtxoCursor,
};
pub use error::{IndexerError, RetryPolicy};
pub use local::{LocalChainIndex, LocalOpenOptions};
pub use remote::{RemoteChainIndex, RemoteOpenOptions};
pub use zinder_core::{
    BlockArtifact, BlockHash, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockId,
    BlockSelector, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding,
    BroadcastRejected, BroadcastUnknown, ChainEpoch, ChainEpochId, CompactBlockArtifact,
    MAX_TRANSPARENT_PREVOUTS_PER_REQUEST, MempoolEntry, MempoolEvictionReason, MinedDetails,
    MinedTransaction, Network, RawTransactionBytes, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootIndex, SubtreeRootRange, TransactionArtifact, TransactionBroadcastResult,
    TransactionId, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentAddressUtxoArtifact, TransparentMempoolOutput, TransparentMempoolOutputsRequest,
    TransparentMempoolSpend, TransparentOutPoint, TreeStateArtifact, TxStatus,
};
pub use zinder_proto::ZINDER_CAPABILITIES;
pub use zinder_proto::capabilities::{
    EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_MEMPOOL_ACTIVITY_V1,
    EXPLORER_MEMPOOL_SUMMARY_V1, EXPLORER_SEARCH_V1, EXPLORER_SERVER_INFO_V1,
    EXPLORER_TRANSACTION_DETAIL_V1, EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1,
    EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1, WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
    WALLET_ADDRESS_TRANSPARENT_HISTORY_V1, WALLET_ADDRESS_TRANSPARENT_UTXOS_V1,
    WALLET_BROADCAST_TRANSACTION_V1, WALLET_EVENTS_CHAIN_V1, WALLET_EVENTS_MEMPOOL_V1,
    WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1, WALLET_MEMPOOL_TRANSPARENT_PREVOUTS_V1,
    WALLET_MEMPOOL_TRANSPARENT_SPEND_BY_OUTPOINT_V1, WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1,
    WALLET_READ_BLOCK_ID_BY_SELECTOR_V1, WALLET_READ_COMPACT_BLOCK_AT_V1,
    WALLET_READ_COMPACT_BLOCK_RANGE_V1, WALLET_READ_FULL_BLOCK_AT_V1, WALLET_READ_LATEST_BLOCK_V1,
    WALLET_READ_LATEST_TREE_STATE_V1, WALLET_READ_SERVER_INFO_V1,
    WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1, WALLET_READ_TRANSACTION_BY_ID_V1,
    WALLET_READ_TRANSPARENT_PREVOUTS_V1, WALLET_READ_TREE_STATE_AT_V1, WALLET_SNAPSHOT_MEMPOOL_V1,
};
pub use zinder_proto::v1::ops::ErrorReason;
pub use zinder_proto::v1::wallet::WalletServerInfo;
pub use zinder_store::ChainEventStreamFamily;
