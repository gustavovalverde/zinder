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
    TransparentHistoryCursor, TransparentUtxoCursor, TxStatus,
};
pub use error::IndexerError;
pub use local::{LocalChainIndex, LocalOpenOptions};
pub use remote::{RemoteChainIndex, RemoteOpenOptions};
pub use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, BlockId, BroadcastAccepted,
    BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastRejected, BroadcastUnknown, ChainEpoch,
    ChainEpochId, CompactBlockArtifact, MempoolEntry, MempoolEvictionReason, Network,
    RawTransactionBytes, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootIndex, SubtreeRootRange,
    TransactionArtifact, TransactionBroadcastResult, TransactionId, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentAddressUtxoArtifact, TransparentMempoolOutput,
    TransparentMempoolOutputsRequest, TransparentMempoolSpend, TransparentOutPoint,
    TreeStateArtifact,
};
pub use zinder_proto::ZINDER_CAPABILITIES;
pub use zinder_proto::v1::wallet::ServerCapabilities;
pub use zinder_store::ChainEventStreamFamily;
