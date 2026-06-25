//! Zinder capability strings and the single declarative advertisement table.
//!
//! Every served RPC on `WalletQuery`, `ExplorerQuery`, and `IngestControl`
//! has one row in [`CAPABILITIES`]. Each row binds a capability string to its
//! surface, the fully qualified proto method it gates, and a declarative
//! [`AdvertisePolicy`]. The three `ServerInfo` builders fold over this table,
//! filtering by surface and evaluating the policy against their own context;
//! the `capability-table-vs-descriptor` CI guard cross-checks the table
//! against the compiled `FileDescriptorSet`. The full protocol contract is in
//! [Public interfaces §Capability
//! Discovery](../../docs/architecture/public-interfaces.md#capability-discovery).
//!
//! Capability naming follows `domain.subdomain.capability_name_v{N}`.
//! Versioned suffixes are part of the capability identity; a `_v2`
//! capability is a separate string from its `_v1` predecessor and may
//! coexist during a deprecation window.

use crate::v1::explorer::ExplorerServerInfo;
use crate::v1::ops::ServerInfo as OpsServerInfo;
use crate::v1::wallet::WalletServerInfo;

/// Capability advertised for `WalletQuery.LatestBlock`.
pub const WALLET_READ_LATEST_BLOCK_V1: &str = "wallet.read.latest_block_v1";
/// Capability advertised for `WalletQuery.BlockIdBySelector`.
pub const WALLET_READ_BLOCK_ID_BY_SELECTOR_V1: &str = "wallet.read.block_id_by_selector_v1";
/// Capability advertised for `WalletQuery.BlockHeaderBySelector`.
pub const WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1: &str = "wallet.read.block_header_by_selector_v1";
/// Capability advertised for `WalletQuery.CompactBlock`.
pub const WALLET_READ_COMPACT_BLOCK_AT_V1: &str = "wallet.read.compact_block_at_v1";
/// Capability advertised for `WalletQuery.CompactBlocksInRange`.
pub const WALLET_READ_COMPACT_BLOCK_RANGE_V1: &str = "wallet.read.compact_block_range_v1";
/// Capability advertised for `WalletQuery.FullBlock`.
///
/// The serialized block bytes are present only when the writer deployment
/// retains block blobs (ingest `raw_blob_policy` is `all`). Reads for
/// unretained heights return `ArtifactUnavailable` (gRPC `NOT_FOUND`). The
/// capability advertises the RPC surface unconditionally; bytes availability
/// is a deployment concern.
pub const WALLET_READ_FULL_BLOCK_AT_V1: &str = "wallet.read.full_block_at_v1";
/// Capability advertised for `WalletQuery.FullBlocksInRange`.
///
/// Same block-blob retention requirement as
/// [`WALLET_READ_FULL_BLOCK_AT_V1`]: the stream yields serialized blocks only
/// when the writer deployment sets `raw_blob_policy = "all"`.
pub const WALLET_READ_FULL_BLOCK_RANGE_V1: &str = "wallet.read.full_block_range_v1";
/// Capability advertised for `WalletQuery.TreeStateAtHeight`.
pub const WALLET_READ_TREE_STATE_AT_HEIGHT_V1: &str = "wallet.read.tree_state_at_height_v1";
/// Capability advertised for `WalletQuery.LatestTreeStateCheckpoint`.
pub const WALLET_READ_LATEST_TREE_STATE_CHECKPOINT_V1: &str =
    "wallet.read.latest_tree_state_checkpoint_v1";
/// Capability advertised for `WalletQuery.SubtreeRoots`.
pub const WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1: &str = "wallet.read.subtree_roots_in_range_v1";
/// Capability advertised for `WalletQuery.Transaction`.
///
/// Covers the typed transaction-status response, including the mined arm's
/// `raw_transaction_bytes`. The serialized bytes ride on this capability
/// rather than a separate one; they are empty when the deployment does not
/// retain transaction blobs (ingest `raw_blob_policy` is `none`).
pub const WALLET_READ_TRANSACTION_BY_ID_V1: &str = "wallet.read.transaction_by_id_v1";
/// Capability advertised for `WalletQuery.ServerInfo`.
pub const WALLET_READ_SERVER_INFO_V1: &str = "wallet.read.server_info_v1";
/// Capability advertised for `WalletQuery.TransparentOutputsByOutpoint`.
pub const WALLET_READ_TRANSPARENT_OUTPUTS_V1: &str =
    "wallet.read.transparent_outputs_by_outpoint_v1";
/// Capability advertised for `WalletQuery.TransparentSpendsByOutpoint`.
///
/// The canonical (confirmed) reverse-spend resolver; reads the always-present
/// canonical spend-fact index, so it is advertised by every wallet-plane
/// deployment. The unmined half is
/// [`WALLET_MEMPOOL_TRANSPARENT_SPENDS_BY_OUTPOINT_V1`].
pub const WALLET_READ_TRANSPARENT_SPENDS_V1: &str = "wallet.read.transparent_spends_by_outpoint_v1";
/// Capability advertised for `WalletQuery.TransparentUnspentOutputsByOutpoint`.
///
/// The canonical single-outpoint unspent probe (gettxout-equivalent). It
/// composes the always-present canonical output and spend-fact indexes, so it
/// is advertised by every wallet-plane deployment. Mempool-aware unspent-ness
/// composes with [`WALLET_MEMPOOL_TRANSPARENT_SPENDS_BY_OUTPOINT_V1`].
pub const WALLET_READ_TRANSPARENT_UNSPENT_OUTPUTS_V1: &str =
    "wallet.read.transparent_unspent_outputs_by_outpoint_v1";
/// Capability advertised for `WalletQuery.ChainValuePoolsAtTip`.
pub const WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1: &str = "wallet.read.chain_value_pools_at_tip_v1";
/// Capability advertised for `WalletQuery.BroadcastTransaction`.
pub const WALLET_BROADCAST_TRANSACTION_V1: &str = "wallet.broadcast.transaction_v1";
/// Capability advertised for `WalletQuery.ChainEvents`.
pub const WALLET_EVENTS_CHAIN_V1: &str = "wallet.events.chain_v1";
/// Capability advertised for `WalletQuery.MempoolSnapshot`.
pub const WALLET_SNAPSHOT_MEMPOOL_V1: &str = "wallet.snapshot.mempool_v1";
/// Capability advertised for `WalletQuery.MempoolEvents`.
pub const WALLET_EVENTS_MEMPOOL_V1: &str = "wallet.events.mempool_v1";
/// Capability advertised for `WalletQuery.TransparentMempoolOutputsByAddress`.
pub const WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1: &str =
    "wallet.mempool.transparent_outputs_by_address_v1";
/// Capability advertised for `WalletQuery.TransparentMempoolSpendsByOutpoint`.
pub const WALLET_MEMPOOL_TRANSPARENT_SPENDS_BY_OUTPOINT_V1: &str =
    "wallet.mempool.transparent_spends_by_outpoint_v1";
/// Capability advertised for `WalletQuery.TransparentMempoolOutputsByOutpoint`.
pub const WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_V1: &str =
    "wallet.mempool.transparent_outputs_by_outpoint_v1";
/// Capability advertised for `WalletQuery.TransparentAddressUnspentOutputs`.
pub const WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1: &str =
    "wallet.address.transparent_unspent_outputs_v1";
/// Capability advertised for `WalletQuery.TransparentAddressTxIdsInRange`.
pub const WALLET_ADDRESS_TRANSPARENT_HISTORY_V1: &str = "wallet.address.transparent_history_v1";
/// Capability advertised for `WalletQuery.TransparentAddressBalance`.
///
/// The confirmed total is summed in-process from the canonical
/// unspent-output index; the signed `unconfirmed_delta_zat` overlays the live
/// mempool when an ingest-control endpoint is wired and is zero otherwise.
/// Always advertised: the canonical unspent index the confirmed sum reads is
/// present on every wallet-plane deployment.
pub const WALLET_ADDRESS_TRANSPARENT_BALANCE_V1: &str = "wallet.address.transparent_balance_v1";
/// Capability advertised for `ExplorerQuery.ServerInfo`.
pub const EXPLORER_SERVER_INFO_V1: &str = "explorer.server_info_v1";
/// Capability advertised for `ExplorerQuery.TransactionDetail`.
///
/// Signals that the response carries the full `TransactionPublicFacts` shape
/// per [ADR-0010](../../../docs/adrs/0010-transaction-public-facts.md). The
/// always-on wallet capability for raw transaction lookup remains
/// [`WALLET_READ_TRANSACTION_BY_ID_V1`].
pub const EXPLORER_TRANSACTION_DETAIL_V1: &str = "explorer.transaction.detail_v1";
/// Capability advertised for `ExplorerQuery.BlockSummariesInRange`.
///
/// Signals that the explorer plane is materializing the `BlockSummary`
/// derive view and the consumer has caught up far enough to serve the
/// summary shape (`block_height`, `block_hash`, `block_time_unix_seconds`,
/// `transaction_count`, `previous_block_hash`). The companion
/// [`EXPLORER_BLOCK_DETAIL_V1`] covers the per-block transaction-id list.
pub const EXPLORER_BLOCK_SUMMARY_V1: &str = "explorer.block.summary_v1";
/// Capability advertised for `ExplorerQuery.BlockDetail`.
///
/// Signals that the explorer plane materialized the per-block transaction
/// id list alongside the summary fields. Coexists with
/// [`EXPLORER_BLOCK_SUMMARY_V1`]; both are advertised together by the same
/// `BlockSummaryConsumer` derive view.
pub const EXPLORER_BLOCK_DETAIL_V1: &str = "explorer.block.detail_v1";
/// Capability advertised for `ExplorerQuery.Search`.
///
/// Signals that the explorer plane classifies a raw user input string
/// into typed search candidates per
/// [ADR-0012](../../../docs/adrs/0012-typed-explorer-search-and-privacy-refusal.md).
/// The classifier short-circuits shielded receivers and viewing keys
/// into the typed `NotPubliclyIndexable` refusal arm before any storage
/// read; gated on `wallet_query_endpoint.is_some()` because hash
/// disambiguation routes through `WalletQuery`.
pub const EXPLORER_SEARCH_V1: &str = "explorer.search_v1";
/// Capability advertised for `ExplorerQuery.MempoolSummary`.
///
/// Signals that the explorer plane aggregates the live mempool snapshot
/// into the explorer-shaped page (total counts, privacy-shape and
/// version distributions, freshness extremes) at request time. Composed
/// from `WalletQuery.MempoolSnapshot`; no derive consumer required.
pub const EXPLORER_MEMPOOL_SUMMARY_V1: &str = "explorer.mempool.summary_v1";
/// Capability advertised for `ExplorerQuery.MempoolActivity`.
///
/// Signals that the explorer plane projects the live mempool entries
/// into the typed `MempoolActivityEntry` rows ordered by newest-first
/// observation time. Composed from `WalletQuery.MempoolSnapshot`.
pub const EXPLORER_MEMPOOL_ACTIVITY_V1: &str = "explorer.mempool.activity_v1";
/// Capability advertised for `ExplorerQuery.TransparentAddressActivity`.
///
/// Signals that the explorer plane composes
/// `WalletQuery.TransparentAddressTxIdsInRange` and
/// `WalletQuery.TransparentMempoolOutputsByAddress` into a unified
/// confirmed-plus-pending activity feed. Pageable; the mempool overlay
/// is emitted only on the first page so subsequent pages stay
/// deterministic.
pub const EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1: &str =
    "explorer.transparent_address.activity_v1";
/// Capability advertised for `ExplorerQuery.FeeSummary`.
///
/// Signals that the explorer plane aggregates per-transaction
/// ZIP-317 conventional fee floors over a block range at request time.
/// The fee fields are ZIP-317 conventional fees, not miner-collected
/// fees: computing actual fees requires transparent-output resolution and is
/// materialized by the derive plane.
pub const EXPLORER_FEE_SUMMARY_V1: &str = "explorer.fee.summary_v1";
/// Capability advertised for `ExplorerQuery.ValuePoolSummary`.
///
/// Signals that the explorer plane can surface upstream
/// `getblockchaininfo.valuePools` through the wallet-plane
/// `ChainValuePoolsAtTip` primitive. The response preserves upstream pool ids
/// instead of projecting into a fixed list of known pools.
pub const EXPLORER_VALUE_POOL_SUMMARY_V1: &str = "explorer.value_pool.summary_v1";
/// Capability advertised for the per-transaction paid-fee surface.
///
/// Signals that the explorer plane has transparent-output facts online and
/// the `TransactionFeesConsumer` has materialized per-transaction fee rows.
/// When advertised, `TransactionDetail` responses populate `paid_fee_zat` and
/// `transparent_inputs[].value_zat`; absent, those fields stay default and
/// consumers fall back to `zip317_conventional_fee_zat` with a
/// `prevout_resolution_status` chip. Gates fields on `TransactionDetail`
/// rather than a dedicated RPC.
pub const EXPLORER_TRANSACTION_FEES_V1: &str = "explorer.transaction.fees_v1";
/// Capability advertised for `ExplorerQuery.RecentTransactions`.
///
/// Signals that the explorer plane materializes a time-descending
/// projection of the most recent transactions into the
/// `recent_transactions` derive column family, served as a single
/// streaming RPC. Eliminates the consumer-side N+1 round-trip tree of
/// `BlockSummariesInRange` + per-block `BlockDetail` + per-tx
/// `TransactionDetail` calls a "recent transactions" panel would
/// otherwise issue.
pub const EXPLORER_TRANSACTION_RECENT_V1: &str = "explorer.transaction.recent_v1";
/// Capability advertised for `ExplorerQuery.MempoolEventCounts`.
///
/// Signals that the explorer plane materializes per-second counters of
/// `Added`, `Mined`, `Invalidated`, and `Suppressed` mempool events into
/// a derive column family. Replaces the in-memory ring buffer the BFF
/// used to keep alongside its own `WalletQuery.MempoolEvents`
/// subscription; the count surface survives consumer restarts and works
/// across horizontally scaled consumer replicas.
pub const EXPLORER_MEMPOOL_EVENT_COUNTS_V1: &str = "explorer.mempool.event_counts_v1";
/// Capability advertised for `ExplorerQuery.VerifyPaymentDisclosure`.
///
/// Signals that the explorer plane runs the
/// [ZIP-311](https://zips.z.cash/zip-0311) payment-disclosure verifier
/// in-process with strict request-bytes redaction. Operator opt-in:
/// disabled by default. When the capability is absent, the consumer
/// falls back to its own local verifier; presence is the consumer's
/// signal to route to the hosted path.
pub const EXPLORER_PAYMENT_DISCLOSURE_VERIFY_V1: &str = "explorer.payment_disclosure.verify_v1";
/// Capability advertised for `ExplorerQuery.OverviewSnapshot`.
///
/// Signals that the explorer plane composes a single coherent overview
/// bundle — tip identity, mempool counts, fee summary, value pools,
/// recent blocks, recent transactions, mempool event counts — in one
/// read pass over the derive store, sharing one `ExplorerFreshness`
/// across every sub-field. Consumers that render a dashboard avoid the
/// per-card fan-out (six independent RPCs whose freshness can diverge)
/// in favor of this single RPC. Gated on `derive_store` and
/// `wallet_query_endpoint` both being online (same precondition as the
/// derive-backed cards the bundle composes).
pub const EXPLORER_OVERVIEW_SNAPSHOT_V1: &str = "explorer.overview.snapshot_v1";

/// Capability advertised for `IngestControl.ServerInfo`.
pub const INGEST_CONTROL_SERVER_INFO_V1: &str = "ingest.control.server_info_v1";
/// Capability advertised for `IngestControl.WriterStatus`.
pub const INGEST_CONTROL_WRITER_STATUS_V1: &str = "ingest.control.writer_status_v1";
/// Capability advertised for `IngestControl.ChainEvents`.
pub const INGEST_CONTROL_CHAIN_EVENTS_V1: &str = "ingest.control.chain_events_v1";
/// Capability advertised for `IngestControl.MempoolSnapshot`.
pub const INGEST_CONTROL_MEMPOOL_SNAPSHOT_V1: &str = "ingest.control.mempool_snapshot_v1";
/// Capability advertised for `IngestControl.MempoolEvents`.
pub const INGEST_CONTROL_MEMPOOL_EVENTS_V1: &str = "ingest.control.mempool_events_v1";
/// Capability advertised for `IngestControl.TransparentMempoolOutputsByAddress`.
pub const INGEST_CONTROL_TRANSPARENT_MEMPOOL_OUTPUTS_BY_ADDRESS_V1: &str =
    "ingest.control.transparent_mempool_outputs_by_address_v1";
/// Capability advertised for `IngestControl.TransparentMempoolSpendsByOutpoint`.
pub const INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPENDS_BY_OUTPOINT_V1: &str =
    "ingest.control.transparent_mempool_spends_by_outpoint_v1";
/// Capability advertised for `IngestControl.TransparentMempoolOutputsByOutpoint`.
pub const INGEST_CONTROL_TRANSPARENT_MEMPOOL_PREVOUTS_V1: &str =
    "ingest.control.transparent_mempool_outputs_by_outpoint_v1";
/// Capability advertised for `IngestControl.ChainValuePoolsAtTip`.
pub const INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1: &str =
    "ingest.control.chain_value_pools_at_tip_v1";

/// Capability for `IngestControl.WriterStatus.phase`.
///
/// Advertises the classifier-driven `zinder.v1.ingest.WriterPhase`
/// vocabulary wired in
/// [ADR-0015](../../../docs/adrs/0015-unified-phase-driven-ingest.md). Gates a
/// field on `WriterStatus` rather than a dedicated RPC.
pub const INGEST_WRITER_PHASE_V1: &str = "ingest.writer.phase_v1";

/// The data plane a capability belongs to.
///
/// Selects which `ServerInfo` builder owns a [`CapabilitySpec`] and which
/// proto service the capability-table-vs-descriptor guard maps it against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CapabilitySurface {
    /// Served by `zinder-query` on `WalletQuery`.
    Wallet,
    /// Served by `zinder-explorer` on `ExplorerQuery`.
    Explorer,
    /// Served by `zinder-ingest` on `IngestControl`.
    Ingest,
}

/// Declarative gate a `ServerInfo` builder evaluates before advertising a
/// capability.
///
/// Each variant names a precondition that a single surface's builder resolves
/// against its own runtime context. A variant never appears on a
/// [`CapabilitySpec`] whose surface cannot evaluate it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AdvertisePolicy {
    /// Advertised by every deployment of the owning surface.
    AlwaysOn,
    /// Wallet: advertised when a transaction broadcaster is configured.
    RequiresBroadcaster,
    /// Wallet: advertised when the chain-event stream is served.
    RequiresChainEvents,
    /// Advertised when the upstream node reports chain value-pool totals.
    ///
    /// Wallet resolves this against the configured proxy flag; Ingest
    /// resolves it against the source handle's
    /// `NodeCapability::ChainValuePools`.
    RequiresChainValuePools,
    /// Explorer: advertised when a `WalletQuery` endpoint is wired.
    RequiresWalletQuery,
    /// Explorer: advertised when both a `WalletQuery` endpoint and the
    /// canonical store are online.
    RequiresWalletQueryAndCanonicalStore,
    /// Explorer: advertised when both the derive store and a `WalletQuery`
    /// endpoint are online.
    RequiresDeriveStoreAndWalletQuery,
    /// Explorer: advertised when the transparent-output prevout resolution
    /// path is online (which itself requires a `WalletQuery` endpoint).
    RequiresPrevoutResolution,
    /// Explorer: advertised when the in-process payment-disclosure verifier
    /// is enabled.
    RequiresPaymentDisclosureVerifier,
}

impl AdvertisePolicy {
    /// Resolves a `WalletQuery` capability against `zinder-query` settings.
    ///
    /// Wallet capabilities use only the always-on, broadcaster, chain-event,
    /// and chain-value-pool gates; the explorer-only variants never appear on
    /// a [`CapabilitySurface::Wallet`] spec and resolve to `false`.
    #[must_use]
    pub fn wallet_satisfied(
        self,
        broadcaster_enabled: bool,
        chain_events_enabled: bool,
        chain_value_pools_enabled: bool,
    ) -> bool {
        match self {
            Self::AlwaysOn => true,
            Self::RequiresBroadcaster => broadcaster_enabled,
            Self::RequiresChainEvents => chain_events_enabled,
            Self::RequiresChainValuePools => chain_value_pools_enabled,
            Self::RequiresWalletQuery
            | Self::RequiresWalletQueryAndCanonicalStore
            | Self::RequiresDeriveStoreAndWalletQuery
            | Self::RequiresPrevoutResolution
            | Self::RequiresPaymentDisclosureVerifier => false,
        }
    }

    /// Resolves an `ExplorerQuery` capability against the adapter's readiness.
    ///
    /// `prevout_resolution_online` is expected to already fold in the
    /// `WalletQuery`-endpoint precondition.
    #[must_use]
    pub fn explorer_satisfied(self, readiness: ExplorerReadiness) -> bool {
        match self {
            Self::AlwaysOn => true,
            Self::RequiresWalletQuery => readiness.wallet_query_online,
            Self::RequiresWalletQueryAndCanonicalStore => {
                readiness.wallet_query_online && readiness.canonical_store_online
            }
            Self::RequiresDeriveStoreAndWalletQuery => {
                readiness.derive_store_online && readiness.wallet_query_online
            }
            Self::RequiresPrevoutResolution => readiness.prevout_resolution_online,
            Self::RequiresPaymentDisclosureVerifier => readiness.payment_disclosure_verifier_online,
            Self::RequiresBroadcaster
            | Self::RequiresChainEvents
            | Self::RequiresChainValuePools => false,
        }
    }

    /// Resolves an `IngestControl` capability against the source handle.
    ///
    /// Ingest capabilities are always on except chain-value-pools, which is
    /// gated on the upstream node reporting the matching node capability.
    #[must_use]
    pub fn ingest_satisfied(self, chain_value_pools_supported: bool) -> bool {
        match self {
            Self::AlwaysOn => true,
            Self::RequiresChainValuePools => chain_value_pools_supported,
            Self::RequiresBroadcaster
            | Self::RequiresChainEvents
            | Self::RequiresWalletQuery
            | Self::RequiresWalletQueryAndCanonicalStore
            | Self::RequiresDeriveStoreAndWalletQuery
            | Self::RequiresPrevoutResolution
            | Self::RequiresPaymentDisclosureVerifier => false,
        }
    }
}

/// Readiness inputs the explorer plane resolves an [`AdvertisePolicy`] against.
///
/// Each field mirrors an online/offline gate the `ExplorerQuery` adapter
/// already tracks. `prevout_resolution_online` folds in the
/// `WalletQuery`-endpoint precondition at the call site. The explorer
/// constructs this directly per request, so it is not `#[non_exhaustive]`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Each bool is an independent explorer readiness gate, not a state machine."
)]
pub struct ExplorerReadiness {
    /// A `WalletQuery` endpoint is wired.
    pub wallet_query_online: bool,
    /// The canonical secondary store is open.
    pub canonical_store_online: bool,
    /// The derive store is open.
    pub derive_store_online: bool,
    /// Transparent-output prevout resolution is online (implies
    /// `wallet_query_online`).
    pub prevout_resolution_online: bool,
    /// The in-process payment-disclosure verifier is enabled.
    pub payment_disclosure_verifier_online: bool,
}

/// One row binding a capability string to its surface, proto method, and
/// advertise policy.
///
/// `method` is the fully qualified proto method name as it appears in the
/// compiled `FileDescriptorSet`
/// (`zinder.v1.<package>.<Service>.<Method>`), or `None` when the capability
/// gates a field on another RPC rather than a method of its own.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct CapabilitySpec {
    /// The exact-match capability string advertised on the wire.
    pub string: &'static str,
    /// The data plane that owns this capability.
    pub surface: CapabilitySurface,
    /// The fully qualified proto method the capability gates, or `None` for a
    /// field-level capability that rides on another RPC.
    pub method: Option<&'static str>,
    /// The precondition the owning surface evaluates before advertising.
    pub policy: AdvertisePolicy,
}

impl CapabilitySpec {
    const fn new(
        string: &'static str,
        surface: CapabilitySurface,
        method: Option<&'static str>,
        policy: AdvertisePolicy,
    ) -> Self {
        Self {
            string,
            surface,
            method,
            policy,
        }
    }
}

/// The single declarative capability contract.
///
/// Every served RPC on the three planes has a row here, plus the field-level
/// capabilities (`explorer.transaction.fees_v1`, `ingest.writer.phase_v1`)
/// that ride on an existing RPC. The `ServerInfo` builders fold over this
/// table filtered by surface; the CI drift guard cross-checks it against the
/// compiled `FileDescriptorSet`.
pub const CAPABILITIES: &[CapabilitySpec] = &[
    CapabilitySpec::new(
        WALLET_READ_LATEST_BLOCK_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.LatestBlock"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.BlockIdBySelector"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.BlockHeaderBySelector"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_COMPACT_BLOCK_AT_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.CompactBlock"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_COMPACT_BLOCK_RANGE_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.CompactBlocksInRange"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_FULL_BLOCK_AT_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.FullBlock"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_FULL_BLOCK_RANGE_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.FullBlocksInRange"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_TREE_STATE_AT_HEIGHT_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TreeStateAtHeight"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_LATEST_TREE_STATE_CHECKPOINT_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.LatestTreeStateCheckpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.SubtreeRoots"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_TRANSACTION_BY_ID_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.Transaction"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_SERVER_INFO_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.ServerInfo"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_TRANSPARENT_OUTPUTS_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentOutputsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_TRANSPARENT_SPENDS_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentSpendsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_TRANSPARENT_UNSPENT_OUTPUTS_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentUnspentOutputsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.ChainValuePoolsAtTip"),
        AdvertisePolicy::RequiresChainValuePools,
    ),
    CapabilitySpec::new(
        WALLET_BROADCAST_TRANSACTION_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.BroadcastTransaction"),
        AdvertisePolicy::RequiresBroadcaster,
    ),
    CapabilitySpec::new(
        WALLET_EVENTS_CHAIN_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.ChainEvents"),
        AdvertisePolicy::RequiresChainEvents,
    ),
    CapabilitySpec::new(
        WALLET_SNAPSHOT_MEMPOOL_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.MempoolSnapshot"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_EVENTS_MEMPOOL_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.MempoolEvents"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentMempoolOutputsByAddress"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_MEMPOOL_TRANSPARENT_SPENDS_BY_OUTPOINT_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentMempoolSpendsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentMempoolOutputsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentAddressUnspentOutputs"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentAddressTxIdsInRange"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
        CapabilitySurface::Wallet,
        Some("zinder.v1.wallet.WalletQuery.TransparentAddressBalance"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        EXPLORER_SERVER_INFO_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.ServerInfo"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        EXPLORER_TRANSACTION_DETAIL_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.TransactionDetail"),
        AdvertisePolicy::RequiresWalletQueryAndCanonicalStore,
    ),
    CapabilitySpec::new(
        EXPLORER_BLOCK_SUMMARY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.BlockSummariesInRange"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_BLOCK_DETAIL_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.BlockDetail"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_SEARCH_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.Search"),
        AdvertisePolicy::RequiresWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_MEMPOOL_SUMMARY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.MempoolSummary"),
        AdvertisePolicy::RequiresWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_MEMPOOL_ACTIVITY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.MempoolActivity"),
        AdvertisePolicy::RequiresWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.TransparentAddressActivity"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_FEE_SUMMARY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.FeeSummary"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_VALUE_POOL_SUMMARY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.ValuePoolSummary"),
        AdvertisePolicy::RequiresWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.MempoolEventCounts"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_TRANSACTION_FEES_V1,
        CapabilitySurface::Explorer,
        None,
        AdvertisePolicy::RequiresPrevoutResolution,
    ),
    CapabilitySpec::new(
        EXPLORER_TRANSACTION_RECENT_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.RecentTransactions"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        EXPLORER_PAYMENT_DISCLOSURE_VERIFY_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.VerifyPaymentDisclosure"),
        AdvertisePolicy::RequiresPaymentDisclosureVerifier,
    ),
    CapabilitySpec::new(
        EXPLORER_OVERVIEW_SNAPSHOT_V1,
        CapabilitySurface::Explorer,
        Some("zinder.v1.explorer.ExplorerQuery.OverviewSnapshot"),
        AdvertisePolicy::RequiresDeriveStoreAndWalletQuery,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_SERVER_INFO_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.ServerInfo"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_WRITER_STATUS_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.WriterStatus"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_CHAIN_EVENTS_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.ChainEvents"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_MEMPOOL_SNAPSHOT_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.MempoolSnapshot"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_MEMPOOL_EVENTS_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.MempoolEvents"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_TRANSPARENT_MEMPOOL_OUTPUTS_BY_ADDRESS_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.TransparentMempoolOutputsByAddress"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPENDS_BY_OUTPOINT_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.TransparentMempoolSpendsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_TRANSPARENT_MEMPOOL_PREVOUTS_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.TransparentMempoolOutputsByOutpoint"),
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_WRITER_PHASE_V1,
        CapabilitySurface::Ingest,
        None,
        AdvertisePolicy::AlwaysOn,
    ),
    CapabilitySpec::new(
        INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1,
        CapabilitySurface::Ingest,
        Some("zinder.v1.ingest.IngestControl.ChainValuePoolsAtTip"),
        AdvertisePolicy::RequiresChainValuePools,
    ),
];

/// Returns the capability rows owned by `surface`, in table order.
///
/// `ServerInfo` builders filter the single [`CAPABILITIES`] table by their own
/// surface, then evaluate each row's [`AdvertisePolicy`] against their runtime
/// context.
pub fn capabilities_for_surface(
    surface: CapabilitySurface,
) -> impl Iterator<Item = &'static CapabilitySpec> {
    CAPABILITIES
        .iter()
        .filter(move |spec| spec.surface == surface)
}

/// Returns the always-on capability strings for `surface`.
///
/// The readiness-gated ops endpoint advertises this subset: it spawns before
/// the source and store readiness gates resolve, so it can only honestly claim
/// the capabilities every deployment of the surface serves unconditionally.
#[must_use]
pub fn always_on_capability_strings(surface: CapabilitySurface) -> Vec<&'static str> {
    capabilities_for_surface(surface)
        .filter(|spec| matches!(spec.policy, AdvertisePolicy::AlwaysOn))
        .map(|spec| spec.string)
        .collect()
}

/// A typed wallet-plane capability a client probes before issuing a call.
///
/// Each variant names one advertised capability on the `WalletQuery` surface
/// whose presence a consumer checks to decide whether a feature is reachable.
/// [`Self::as_str`] returns the exact wire string, which is the same constant
/// the [`CAPABILITIES`] table advertises, so a typed probe and the wire stay
/// in lockstep. Prefer [`CapabilityDescriptor::supports`] over comparing raw
/// capability strings.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Capability {
    /// Raw transaction broadcast (`wallet.broadcast.transaction_v1`).
    Broadcast,
    /// Cursor-resumable chain-event stream (`wallet.events.chain_v1`).
    ChainEvents,
    /// Bounded mempool snapshot (`wallet.snapshot.mempool_v1`).
    MempoolSnapshot,
    /// Replayable mempool-event stream (`wallet.events.mempool_v1`).
    MempoolEvents,
    /// Chain value-pool totals at the upstream tip
    /// (`wallet.read.chain_value_pools_at_tip_v1`).
    ChainValuePools,
    /// Transparent-address balance (`wallet.address.transparent_balance_v1`).
    TransparentAddressBalance,
}

impl Capability {
    /// Returns the exact capability string this variant advertises.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Broadcast => WALLET_BROADCAST_TRANSACTION_V1,
            Self::ChainEvents => WALLET_EVENTS_CHAIN_V1,
            Self::MempoolSnapshot => WALLET_SNAPSHOT_MEMPOOL_V1,
            Self::MempoolEvents => WALLET_EVENTS_MEMPOOL_V1,
            Self::ChainValuePools => WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1,
            Self::TransparentAddressBalance => WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
        }
    }
}

/// Helpers for client-side capability discovery.
///
/// Implemented by every per-service descriptor (`WalletServerInfo`,
/// `ExplorerServerInfo`) plus the cross-service `ops::ServerInfo` they embed.
/// Capability discovery always reads from the embedded `ops::ServerInfo`;
/// per-service descriptors delegate.
pub trait CapabilityDescriptor {
    /// Returns true if the descriptor advertises `capability`.
    fn has(&self, capability: &str) -> bool;

    /// Returns true if the descriptor advertises the typed [`Capability`].
    ///
    /// Prefer this over [`Self::has`] with a raw string: the compiler checks
    /// the variant against the typed set, so a consumer cannot probe a
    /// capability string that no longer exists.
    fn supports(&self, capability: Capability) -> bool {
        self.has(capability.as_str())
    }
}

impl CapabilityDescriptor for OpsServerInfo {
    fn has(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|advertised| advertised == capability)
    }
}

impl CapabilityDescriptor for WalletServerInfo {
    fn has(&self, capability: &str) -> bool {
        self.common
            .as_ref()
            .is_some_and(|common| common.has(capability))
    }
}

impl CapabilityDescriptor for ExplorerServerInfo {
    fn has(&self, capability: &str) -> bool {
        self.common
            .as_ref()
            .is_some_and(|common| common.has(capability))
    }
}
