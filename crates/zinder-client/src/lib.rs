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
    AddressOutputCursor, AddressOutputIndexQuery, AddressOutputIndexStream,
    AddressOutputIndexStreamItem, AddressOutputIndexView, ChainEpochCommitted, ChainEvent,
    ChainEventCursor, ChainEventEnvelope, ChainEventStream, ChainIndex, ChainRangeReverted,
    IndexStream, MempoolEvent, MempoolEventCursor, MempoolEventEnvelope, MempoolEventStream,
    MempoolSnapshotCursor, MempoolSnapshotRequest, MempoolSnapshotView,
    TransparentAddressTxIdsQuery, TransparentAddressTxIdsStream, TransparentAddressTxIdsStreamItem,
    TransparentHistoryCursor,
};
pub use error::{IndexerError, RetryPolicy};
pub use local::{LocalChainIndex, LocalOpenOptions};
pub use remote::{RemoteChainIndex, RemoteOpenOptions};
pub use zinder_core::{
    AddressOutputIndexArtifact, BlockHash, BlockHeaderInfo, BlockHeight, BlockHeightRange, BlockId,
    BlockSelector, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding,
    BroadcastRejected, BroadcastUnknown, ChainEpoch, ChainEpochId, ChainValuePool, ChainValuePools,
    ChainValuePoolsAtTip, CompactBlockArtifact, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, MempoolEntry,
    MempoolEvictionReason, MinedDetails, MinedTransaction, Network, RawTransactionBytes,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootIndex, SubtreeRootRange,
    TransactionBroadcastResult, TransactionId, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentMempoolOutput, TransparentMempoolOutputsRequest,
    TransparentMempoolSpend, TransparentOutPoint, TreeStateArtifact, TxStatus,
};
pub use zinder_proto::ZINDER_CAPABILITIES;
pub use zinder_proto::capabilities::{
    EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_EVENT_COUNTS_V1, EXPLORER_MEMPOOL_SUMMARY_V1,
    EXPLORER_SEARCH_V1, EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V1,
    EXPLORER_TRANSACTION_FEES_V1, EXPLORER_TRANSACTION_RECENT_V1,
    EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1, EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
    EXPLORER_VALUE_POOL_SUMMARY_V1, INGEST_CONTROL_ALWAYS_ON_CAPABILITIES, INGEST_WRITER_PHASE_V1,
    WALLET_ADDRESS_OUTPUT_INDEX_V1, WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
    WALLET_ADDRESS_TRANSPARENT_HISTORY_V1, WALLET_BROADCAST_TRANSACTION_V1, WALLET_EVENTS_CHAIN_V1,
    WALLET_EVENTS_MEMPOOL_V1, WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1,
    WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_V1, WALLET_MEMPOOL_TRANSPARENT_SPEND_BY_OUTPOINT_V1,
    WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1, WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
    WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1, WALLET_READ_COMPACT_BLOCK_AT_V1,
    WALLET_READ_COMPACT_BLOCK_RANGE_V1, WALLET_READ_LATEST_BLOCK_V1,
    WALLET_READ_LATEST_TREE_STATE_CHECKPOINT_V1, WALLET_READ_SERVER_INFO_V1,
    WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1, WALLET_READ_TRANSACTION_BY_ID_V1,
    WALLET_READ_TRANSPARENT_OUTPUTS_V1, WALLET_READ_TREE_STATE_CHECKPOINT_V1,
    WALLET_SNAPSHOT_MEMPOOL_V1,
};
pub use zinder_proto::v1::ops::ErrorReason;
pub use zinder_proto::v1::wallet::WalletServerInfo;
pub use zinder_store::ChainEventStreamFamily;
