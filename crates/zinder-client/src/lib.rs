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
    CHAIN_INDEX_CAPABILITIES, ChainEpochCommitted, ChainEvent, ChainEventCursor,
    ChainEventEnvelope, ChainEventStream, ChainIndex, ChainRangeReverted, IndexStream, TxStatus,
};
pub use error::IndexerError;
pub use local::{LocalChainIndex, LocalOpenOptions};
pub use remote::{RemoteChainIndex, RemoteOpenOptions};
pub use zinder_core::{
    BlockArtifact, BlockHash, BlockHeight, BlockHeightRange, BlockId, BroadcastAccepted,
    BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastRejected, BroadcastUnknown, ChainEpoch,
    ChainEpochId, CompactBlockArtifact, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootIndex, SubtreeRootRange, TransactionArtifact,
    TransactionBroadcastResult, TransactionId, TreeStateArtifact,
};
pub use zinder_proto::v1::wallet::ServerCapabilities;
pub use zinder_store::ChainEventStreamFamily;
