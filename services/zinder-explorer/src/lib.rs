//! Explorer-plane runtime serving block-explorer, dashboard, and analytics reads.
//!
//! `zinder-explorer` is the fourth Zinder deployable. It runs in a separate
//! process from `zinder-ingest` and `zinder-query`, reads the ingest-owned
//! derive store in secondary mode, and serves the `ExplorerQuery` gRPC surface.
//!
//! See `docs/architecture/explorer-plane.md` for the product surface and
//! `docs/architecture/derive-plane.md` for the derive store this crate reads.

mod grpc;

pub use grpc::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings, describe_request_metrics};
pub use zinder_derive::{
    BLOCK_SUMMARY_CAPABILITIES, BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME,
    BlockCommitContext, BlockCommitContextError, BlockKeyedConsumer, BlockSummaryConsumer,
    BlockSummaryConsumerError, ChainCommittedEvent, ChainReorgedEvent, CommittedRange,
    ConsumerEntry, DERIVE_SCHEMA_VERSION, DeriveConsumer, DeriveConsumerCtx, DeriveConsumerError,
    DeriveConsumerName, DeriveCursorEntry, DeriveError, DeriveMempoolConsumer, DeriveStore,
    DeriveStoreColumnFamily, DeriveStoreError, DeriveStoreOptions, DeriveStoreTable,
    MEMPOOL_EVENT_COUNTS_COLUMN_FAMILY, MEMPOOL_EVENT_COUNTS_CONSUMER_NAME,
    MEMPOOL_EVENT_COUNTS_RETENTION_SECONDS, MempoolConsumerEvent, MempoolConsumerEventVariant,
    MempoolEventCountsConsumer, RECENT_TRANSACTIONS_COLUMN_FAMILY,
    RECENT_TRANSACTIONS_CONSUMER_NAME, RecentTransactionsConsumer, RecentTransactionsConsumerError,
    RevertedRange, TRANSACTION_FEES_COLUMN_FAMILIES, TRANSACTION_FEES_COLUMN_FAMILY,
    TRANSACTION_FEES_CONSUMER_NAME, TRANSACTION_FEES_INDEX_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILIES, TRANSPARENT_ADDRESS_ACTIVITY_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME, TRANSPARENT_ADDRESS_ACTIVITY_INDEX_COLUMN_FAMILY,
    TRANSPARENT_ADDRESS_ACTIVITY_KEY_LEN, TransactionFeesConsumer, TransactionFeesConsumerError,
    TransparentAddressActivityConsumer, TransparentAddressActivityConsumerError,
    TransparentSpendFacts, decode_stored_record,
};
