//! Canonical chain storage contracts for Zinder.
//!
//! `zinder-store` exposes domain-shaped commit and read APIs while keeping
//! `RocksDB` handles, column families, and write batches private to the adapter.

mod artifact_visibility;
mod block_artifact;
mod chain_epoch;
mod chain_epoch_reader;
mod chain_event;
mod chain_event_stream;
mod chain_store;
mod format;
mod grpc_status;
mod kv;
mod proto_codec;
mod store_error;
mod subtree_root;
mod transaction_artifact;
mod transparent_utxo;
mod tree_state;

pub use block_artifact::{CompactBlockStore, FinalizedBlockStore};
pub use chain_epoch::{ChainEpochArtifacts, ReorgWindowChange};
pub use chain_epoch_reader::ChainEpochReader;
pub use chain_event::{
    ChainEpochCommitOutcome, ChainEpochCommitted, ChainEvent, ChainEventEnvelope,
    ChainRangeReverted,
};
pub use chain_event_stream::run_chain_event_stream;
pub use chain_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochReadApi, ChainEventHistoryRequest,
    ChainEventRetentionReport, ChainStoreOptions, DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS,
    MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION, PrimaryChainStore, SecondaryCatchupOutcome,
    SecondaryChainStore,
};
pub use format::{
    ChainEventStreamFamily, STREAM_CURSOR_TOKEN_V1_LEN, StreamCursorError, StreamCursorTokenV1,
};
pub use grpc_status::status_from_store_error;
pub use proto_codec::{ChainEventEncodeError, chain_epoch_message, chain_event_envelope_message};
pub use store_error::{ArtifactFamily, StorageErrorKind, StorageKey, StoreError};
pub use subtree_root::SubtreeRootStore;
pub use transaction_artifact::TransactionArtifactStore;
pub use transparent_utxo::TransparentUtxoStore;
pub use tree_state::TreeStateStore;
