//! Explorer-plane runtime serving block-explorer, dashboard, and analytics reads.
//!
//! `zinder-explorer` is the fourth Zinder deployable. It runs in a separate
//! process from `zinder-ingest` and `zinder-query`, owns its own `RocksDB`
//! instance (per the `DeriveStore` pattern reused below), and reads the
//! canonical chain and mempool surfaces from `zinder-query` over gRPC. The
//! explorer plane is the home for materialized views, precomputed aggregates,
//! and explorer-shaped reads that should not live in the canonical wallet
//! data plane.
//!
//! See `docs/architecture/explorer-plane.md` for the product surface and
//! `docs/architecture/derive-plane.md` for the reusable SDK pattern this
//! crate exercises. The SDK abstractions ([`DeriveStore`], [`DeriveConsumer`],
//! [`BlockSource`], `run_chain_events_subscriber`, etc.) keep their pattern
//! names so a future second consumer can link the same crate without renames.

mod consumer;
mod error;
mod grpc;
mod store;

pub use consumer::block_summary::{
    BLOCK_SUMMARY_CAPABILITIES, BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME,
    BlockSummaryConsumer, BlockSummaryConsumerError, decode_stored_record,
};
pub use consumer::chain_events::{ChainEventsRunOutcome, run as run_chain_events_subscriber};
pub use consumer::mempool_event_counts::{
    MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY, MEMPOOL_EVENT_COUNTS_CONSUMER_NAME,
    MEMPOOL_EVENT_COUNTS_RETENTION_SECONDS, MempoolEventCountsConsumer,
};
pub use consumer::mempool_events::{MempoolEventsRunOutcome, run as run_mempool_events_subscriber};
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
pub use consumer::{
    BlockCommitContext, BlockCommitContextError, BlockKeyedConsumer, BlockSource,
    ChainCommittedEvent, ChainReorgedEvent, CommittedRange, DeriveConsumer, DeriveConsumerCtx,
    DeriveConsumerError, DeriveConsumerName, DeriveMempoolConsumer, MempoolConsumerEvent,
    MempoolConsumerEventVariant, PrevoutResolver, RevertedRange,
};
pub use error::{DeriveError, DeriveStoreColumnFamily, DeriveStoreError};
pub use grpc::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings, describe_request_metrics};
pub use store::{
    ConsumerEntry, DERIVE_SCHEMA_VERSION, DeriveCursorEntry, DeriveStore, DeriveStoreOptions,
    DeriveStoreTable,
};
