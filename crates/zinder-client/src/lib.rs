//! Typed Rust client surface for Zinder chain-index consumers.
//!
//! `zinder-client` is the public Rust API that wallet daemons and application
//! code import. It keeps `RocksDB`, `tonic`, and generated protobuf types behind
//! typed domain methods so downstream consumers do not need to couple to
//! Zinder service internals.
//!
//! This crate is the external consumer SDK; Zinder's own services do not use
//! it for service-to-service calls, which go through each service's own
//! authenticated channel setup instead.

mod chain_index;
mod error;
#[cfg(feature = "local")]
mod local;
#[cfg(feature = "remote")]
mod remote;

#[cfg(feature = "remote")]
pub use chain_index::{
    ChainEpochCommitted, ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventStream,
    ChainEventStreamFamily, ChainRangeReverted, EndpointBackedIndex, EventStreamStart,
    MempoolEvent, MempoolEventCursor, MempoolEventEnvelope, MempoolEventStream,
    MempoolSnapshotCursor, MempoolSnapshotRequest, MempoolSnapshotView,
};
pub use chain_index::{
    ChainIndex, ChainSnapshot, IndexStream, OwnedChainSnapshot, TransparentAddressTransactionChunk,
    TransparentAddressTxIdsQuery, TransparentAddressTxIdsStream,
    TransparentAddressUnspentOutputsQuery, TransparentAddressUnspentOutputsStream,
    TransparentHistoryCursor, TransparentUnspentOutputChunk, TransparentUtxoSetSummaryView,
};
pub use error::{ChainEventCursorRecovery, IndexerError, RetryPolicy};
#[cfg(feature = "local")]
pub use local::{DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex, LocalOpenOptions};
#[cfg(feature = "remote")]
pub use remote::{MIN_SUPPORTED_CONTRACT_REVISION, RemoteChainIndex, RemoteOpenOptions};
pub use zinder_core::{
    BlockHash, BlockHeader, BlockHeight, BlockHeightRange, BlockId, BlockSelector,
    BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastQueued,
    BroadcastRejected, BroadcastRejectionReason, BroadcastUnknown, ChainEpoch, ChainEpochId,
    ChainValuePool, ChainValuePools, ChainValuePoolsAtTip, CompactBlockArtifact,
    MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, MempoolEntry, MempoolEvictionReason, MinedTransaction,
    MinedTransactionChainContext, Network, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootIndex, SubtreeRootRange, TransactionBroadcastOutcome,
    TransactionId, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentMempoolOutput, TransparentMempoolOutputsRequest, TransparentMempoolSpend,
    TransparentOutPoint, TransparentUnspentOutput, TransparentUtxoSetCommitment, TreeStateArtifact,
    TxStatus, UtxoSetCommitmentScheme,
};
#[cfg(feature = "remote")]
pub use zinder_proto::capabilities::{
    AdvertisePolicy, CAPABILITIES, Capability, CapabilityDescriptor, CapabilitySpec,
    CapabilitySurface, EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1,
    EXPLORER_FEE_SUMMARY_V1, EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
    EXPLORER_MEMPOOL_SUMMARY_V1, EXPLORER_OVERVIEW_SNAPSHOT_V1, EXPLORER_SEARCH_V1,
    EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V4, EXPLORER_TRANSACTION_FEES_V1,
    EXPLORER_TRANSACTION_HISTORY_V1, EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
    EXPLORER_TRANSACTION_RECENT_V1, EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    EXPLORER_VALUE_POOL_SUMMARY_V1, INGEST_WRITER_PHASE_V1, WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
    WALLET_ADDRESS_TRANSPARENT_HISTORY_V1, WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1,
    WALLET_BROADCAST_TRANSACTION_V1, WALLET_EVENTS_CHAIN_V1, WALLET_EVENTS_MEMPOOL_V2,
    WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1, WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_V1,
    WALLET_MEMPOOL_TRANSPARENT_SPENDS_BY_OUTPOINT_V1, WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1,
    WALLET_READ_BLOCK_ID_BY_SELECTOR_V1, WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1,
    WALLET_READ_COMPACT_BLOCK_AT_V2, WALLET_READ_COMPACT_BLOCK_IRONWOOD_V2,
    WALLET_READ_COMPACT_BLOCK_RANGE_V2, WALLET_READ_LATEST_TREE_STATE_CHECKPOINT_V2,
    WALLET_READ_SERVER_INFO_V2, WALLET_READ_SETTLED_TIP_BLOCK_V1,
    WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1, WALLET_READ_SUBTREE_ROOTS_IRONWOOD_V1,
    WALLET_READ_TRANSACTION_BY_ID_V2, WALLET_READ_TRANSPARENT_OUTPUTS_V1,
    WALLET_READ_TREE_STATE_AT_HEIGHT_V2, WALLET_READ_VISIBLE_TIP_BLOCK_V1,
    WALLET_SNAPSHOT_MEMPOOL_V3, always_on_capability_strings, capabilities_for_surface,
};
#[cfg(feature = "remote")]
pub use zinder_proto::v1::ops::ErrorReason;
#[cfg(feature = "remote")]
pub use zinder_proto::v1::wallet::WalletServerInfo;

/// The server-side wallet recipe, compiled as a doctest so its worked skeleton
/// cannot drift from the real `connect` and stream API.
#[cfg(feature = "remote")]
#[allow(
    clippy::doc_markdown,
    reason = "Reader-facing recipe prose names product and crate identifiers without backticks by design."
)]
#[doc = include_str!("../../../docs/reference/server-side-wallet-pattern.md")]
mod server_side_wallet_recipe {}
