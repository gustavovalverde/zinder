//! Canonical chain storage contracts for Zinder.
//!
//! `zinder-store` exposes domain-shaped commit and read APIs while keeping
//! `RocksDB` handles, column families, and write batches private to the adapter.

mod artifact_visibility;
mod block_artifact;
mod block_hash_index;
mod chain_epoch;
mod chain_epoch_reader;
mod chain_event;
mod chain_event_stream;
mod chain_store;
mod format;
mod grpc_status;
mod kv;
mod mempool_event;
mod mempool_event_store;
mod proto_codec;
mod store_error;
mod subtree_root;
mod transaction_artifact;
mod transparent_address_tx_index;
mod transparent_utxo;
mod tree_state;

pub use block_artifact::{CompactBlockStore, FinalizedBlockStore};
pub use block_hash_index::BlockHashLookup;
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
    SecondaryChainStore, TransparentAddressTxIndexPage, TransparentAddressTxIndexPageRequest,
    TransparentAddressUtxosPage, TransparentAddressUtxosPageRequest,
};
pub use format::{
    ChainEventStreamFamily, MempoolEventCursorPayload, MempoolEventStreamFamily,
    STREAM_CURSOR_TOKEN_V1_LEN, StreamCursorError, StreamCursorTokenV1,
    TransparentHistoryCursorAnchor, TransparentHistoryCursorPayload,
    TransparentHistoryStreamFamily, TransparentUtxoCursorPayload, TransparentUtxoStreamFamily,
};
pub use grpc_status::status_from_store_error;
pub use mempool_event::{MempoolEvent, MempoolEventEnvelope};
pub use mempool_event_store::{
    DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS, MempoolEventHistoryRequest,
    MempoolEventRetentionConfig, MempoolEventRetentionReport,
};
pub use proto_codec::{
    ChainEventEncodeError, MempoolDecodeError, chain_epoch_from_message, chain_epoch_message,
    chain_event_envelope_message, mempool_entry_from_message, mempool_entry_message,
    mempool_event_envelope_from_message, mempool_event_envelope_message, outpoint_from_message,
    outpoint_message, transparent_mempool_output_from_message, transparent_mempool_output_message,
    transparent_mempool_spend_from_message, transparent_mempool_spend_message,
};
pub use store_error::{ArtifactFamily, StorageErrorKind, StorageKey, StoreError};
pub use subtree_root::SubtreeRootStore;
pub use transaction_artifact::TransactionArtifactStore;
pub use transparent_address_tx_index::TransparentAddressTxIndexStore;
pub use transparent_utxo::TransparentUtxoStore;
pub use tree_state::TreeStateStore;
