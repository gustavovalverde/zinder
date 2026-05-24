//! Derive-plane SDK.
//!
//! Reusable abstractions for building consumers that receive canonical
//! chain or mempool events from `zinder-ingest`, derive index state from
//! each event, and persist the result through [`DeriveStore`] atomically
//! with their cursor.
//!
//! - [`DeriveConsumer`] is the seam between consumer-agnostic infrastructure
//!   (the writer scheduler, the store, and parsed block contexts) and the
//!   consumer-specific aggregation logic each impl owns.
//! - [`BlockKeyedConsumer`] is the per-block convention every production
//!   chain-events consumer follows; the in-process dispatch helpers walk the
//!   height range from each event and dispatch per-height work.
//! - [`DeriveStore`] is the `RocksDB` wrapper consumers write through. It
//!   shares its option layering with `zinder_store`'s canonical store so the
//!   bulk-catchup-OOM bound from [ADR-0020] applies to both planes.
//!
//! See `docs/architecture/derive-plane.md` for the SDK pattern and
//! `docs/adrs/0017-derive-consumer-template-and-key-codec-convention.md` for
//! the trait+codec convention.

pub mod consumer;
pub mod error;
pub mod store;

pub use consumer::block_summary::{
    BLOCK_SUMMARY_CAPABILITIES, BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME,
    BlockSummaryConsumer, BlockSummaryConsumerError, decode_stored_record,
};
pub use consumer::mempool_event_counts::{
    MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY, MEMPOOL_EVENT_COUNTS_CONSUMER_NAME,
    MEMPOOL_EVENT_COUNTS_RETENTION_SECONDS, MempoolEventCountsConsumer,
};
pub use consumer::recent_transactions::{
    RECENT_TRANSACTIONS_COLUMN_FAMILY, RECENT_TRANSACTIONS_CONSUMER_NAME,
    RecentTransactionsConsumer, RecentTransactionsConsumerError,
};
pub use consumer::transaction_fees::{
    TRANSACTION_FEES_COLUMN_FAMILIES, TRANSACTION_FEES_COLUMN_FAMILY,
    TRANSACTION_FEES_CONSUMER_NAME, TRANSACTION_FEES_INDEX_COLUMN_FAMILY, TransactionFeesConsumer,
    TransactionFeesConsumerError,
};
pub use consumer::transparent_address_activity::{
    TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILIES, TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME, TRANSPARENT_ADDRESS_ACTIVITY_INDEX_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN, TransparentAddressActivityConsumer,
    TransparentAddressActivityConsumerError,
};
pub use consumer::transparent_address_transaction_history::{
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILIES,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_DESCENDING_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
    TransparentAddressTransactionHistoryConsumer,
    TransparentAddressTransactionHistoryConsumerError, TransparentAddressTransactionHistoryPage,
    TransparentAddressTransactionHistoryPageRequest,
};
pub use consumer::{
    BlockCommitContext, BlockCommitContextError, BlockCommitPayload, BlockKeyedConsumer,
    ChainCommittedEvent, ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx,
    DeriveConsumerError, DeriveConsumerName, DeriveMempoolConsumer, MempoolConsumerEvent,
    MempoolConsumerEventVariant, RevertedRange, TransparentSpendFacts,
    apply_chain_committed_in_memory, apply_chain_reorged_in_memory,
};
pub use error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError};
pub use store::{
    ChainEventDispatchInputs, ConsumerEntry, DERIVE_SCHEMA_VERSION, DERIVE_STORE_SUBDIR,
    DeriveCursorEntry, DeriveStore, DeriveStoreOptions, DeriveStoreTable,
};
