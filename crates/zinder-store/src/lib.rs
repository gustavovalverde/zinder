//! Canonical chain storage contracts for Zinder.
//!
//! `zinder-store` exposes domain-shaped commit and read APIs while keeping
//! `RocksDB` handles, column families, and write batches private to the adapter.

mod address_output_index;
mod artifact_visibility;
mod block_artifact;
mod block_hash_index;
mod chain_epoch;
mod chain_epoch_reader;
mod chain_event;
mod chain_store;
mod event_stream;
mod format;
mod grpc_status;
mod kv;
mod mempool_event;
mod mempool_event_store;
mod proto_codec;
mod rocksdb_resource_budget;
mod store_error;
mod subtree_root;
mod transaction_artifact;
mod transparent_output;
mod transparent_spend_fact;
mod tree_state;

pub use address_output_index::AddressOutputIndexStore;
pub use block_artifact::{
    BlockBlobStore, BlockHeaderStore, BlockTransactionIndexStore, CompactBlockStore,
};
pub use block_hash_index::BlockHashLookup;
pub use chain_epoch::{ChainEpochArtifacts, ReorgWindowChange};
pub use chain_epoch_reader::ChainEpochReader;
pub use chain_event::{
    ChainEpochCommitOutcome, ChainEpochCommitted, ChainEvent, ChainEventEnvelope,
    ChainRangeReverted,
};
pub use chain_store::{
    AddressOutputIndexPage, AddressOutputIndexPageRequest, CURRENT_ARTIFACT_SCHEMA_VERSION,
    ChainEpochReadApi, ChainEventHistoryRequest, ChainEventRetentionReport, ChainStoreOptions,
    DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS, MAX_SUPPORTED_ARTIFACT_SCHEMA_VERSION,
    PrimaryChainStore, RawBlobRetention, SecondaryCatchupOutcome, SecondaryChainStore,
};
pub use event_stream::{
    ChainEventStreamResume, EventEnvelope, EventStreamStartPosition, run_event_stream,
};
pub use format::{
    AddressOutputCursorPayload, AddressOutputStreamFamily, ChainEventStreamFamily,
    MempoolEventCursorPayload, MempoolEventStreamFamily, STREAM_CURSOR_TOKEN_V1_LEN,
    SnapshotPageCursorAnchor, SnapshotPageCursorPayload, SnapshotPageStreamFamily,
    StreamCursorError, StreamCursorTokenV1, TransparentHistoryCursorAnchor,
    TransparentHistoryCursorPayload, TransparentHistoryStreamFamily,
};
pub use grpc_status::{
    chain_event_stream_family_from_request, event_stream_start_from_request,
    mempool_event_stream_family_from_request, status_from_store_error,
};
pub use kv::{
    BoundedRocksDbOpen, ResourceGaugeThrottle, RocksDbIoMode, RocksDbOpenRole,
    RocksDbResourceGaugeInputs, StoreReadCaller, StoreRole, build_block_based_table_factory,
    open_bounded_rocksdb, record_rocksdb_resource_gauges,
};
pub use mempool_event::{MempoolEvent, MempoolEventEnvelope, MempoolEventPosition};
pub use mempool_event_store::{
    DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS, MempoolEventHistoryRequest,
    MempoolEventRetentionConfig, MempoolEventRetentionReport,
};
pub use proto_codec::{
    ChainEventEncodeError, MempoolDecodeError, block_tip_message, chain_epoch_from_message,
    chain_epoch_message, chain_event_envelope_message, chain_event_stream_family_from_message,
    chain_view_message, event_stream_start_from_message, event_stream_start_message,
    mempool_entry_from_message, mempool_entry_message, mempool_event_envelope_from_message,
    mempool_event_envelope_message, mempool_event_stream_family_from_message,
    outpoint_from_message, outpoint_message, stream_cursor_from_message_bytes,
    transparent_mempool_output_from_message, transparent_mempool_output_message,
    transparent_mempool_spend_from_message, transparent_mempool_spend_message,
    transparent_output_entry_message, transparent_output_message, transparent_spend_message,
};
pub use rocksdb_resource_budget::RocksDbResourceBudget;
pub use store_error::{ArtifactFamily, StorageErrorKind, StorageKey, StoreError};
pub use subtree_root::SubtreeRootStore;
pub use transaction_artifact::{
    TransactionBlobStore, TransactionFactsStore, TransactionLocationStore,
};
pub use tree_state::TreeStateStore;
