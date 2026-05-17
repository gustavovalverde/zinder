//! Zinder capability strings advertised through `WalletQuery.ServerInfo`.
//!
//! Capability strings are exact-match. New methods on `WalletQuery` add a
//! capability string here in the same change. The `capability-coverage` CI
//! job asserts that every RPC has a corresponding entry. The full protocol
//! contract is in [Public interfaces §Capability
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
/// Capability advertised for `WalletQuery.CompactBlockRange`.
pub const WALLET_READ_COMPACT_BLOCK_RANGE_V1: &str = "wallet.read.compact_block_range_v1";
/// Capability advertised for `WalletQuery.FullBlock`.
///
/// Returns the full canonical block bytes for consumers that need the
/// transaction list including transparent-only and coinbase transactions
/// the compact-block format omits.
pub const WALLET_READ_FULL_BLOCK_AT_V1: &str = "wallet.read.full_block_at_v1";
/// Capability advertised for `WalletQuery.TreeState`.
pub const WALLET_READ_TREE_STATE_AT_V1: &str = "wallet.read.tree_state_at_v1";
/// Capability advertised for `WalletQuery.LatestTreeState`.
pub const WALLET_READ_LATEST_TREE_STATE_V1: &str = "wallet.read.latest_tree_state_v1";
/// Capability advertised for `WalletQuery.SubtreeRoots`.
pub const WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1: &str = "wallet.read.subtree_roots_in_range_v1";
/// Capability advertised for `WalletQuery.Transaction`.
pub const WALLET_READ_TRANSACTION_BY_ID_V1: &str = "wallet.read.transaction_by_id_v1";
/// Capability advertised for `WalletQuery.ServerInfo`.
pub const WALLET_READ_SERVER_INFO_V1: &str = "wallet.read.server_info_v1";
/// Capability advertised for `WalletQuery.TransparentPrevouts`.
pub const WALLET_READ_TRANSPARENT_PREVOUTS_V1: &str = "wallet.read.transparent_prevouts_v1";
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
/// Capability advertised for `WalletQuery.TransparentMempoolSpendByOutpoint`.
pub const WALLET_MEMPOOL_TRANSPARENT_SPEND_BY_OUTPOINT_V1: &str =
    "wallet.mempool.transparent_spend_by_outpoint_v1";
/// Capability advertised for `WalletQuery.TransparentMempoolPrevouts`.
pub const WALLET_MEMPOOL_TRANSPARENT_PREVOUTS_V1: &str = "wallet.mempool.transparent_prevouts_v1";
/// Capability advertised for `WalletQuery.TransparentAddressUtxos[Stream]`.
pub const WALLET_ADDRESS_TRANSPARENT_UTXOS_V1: &str = "wallet.address.transparent_utxos_v1";
/// Capability advertised for `WalletQuery.TransparentAddressTxIdsInRange`.
pub const WALLET_ADDRESS_TRANSPARENT_HISTORY_V1: &str = "wallet.address.transparent_history_v1";
/// Always-on canonical-confirmed-balance path for `WalletQuery.TransparentAddressBalance`.
///
/// Advertised whenever the deployment exposes the RPC. Clients that need the
/// mempool overlay must additionally check for
/// [`EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1`]; the wallet capability alone
/// signals confirmed totals computed from canonical UTXOs.
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
/// fees: computing actual fees requires prevout resolution and is out
/// of scope for `v1`. Composed from `WalletQuery.FullBlock` per height;
/// no derive consumer required.
pub const EXPLORER_FEE_SUMMARY_V1: &str = "explorer.fee.summary_v1";
/// Capability advertised for `ExplorerQuery.ValuePoolSummary`.
///
/// Signals that the explorer plane can surface upstream
/// `getblockchaininfo.valuePools` through the wallet-plane
/// `ChainValuePoolsAtTip` primitive. The response preserves upstream pool ids
/// instead of projecting into a fixed list of known pools.
pub const EXPLORER_VALUE_POOL_SUMMARY_V1: &str = "explorer.value_pool.summary_v1";

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
/// Capability advertised for `IngestControl.TransparentMempoolSpendByOutpoint`.
pub const INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPEND_BY_OUTPOINT_V1: &str =
    "ingest.control.transparent_mempool_spend_by_outpoint_v1";
/// Capability advertised for `IngestControl.TransparentMempoolPrevouts`.
pub const INGEST_CONTROL_TRANSPARENT_MEMPOOL_PREVOUTS_V1: &str =
    "ingest.control.transparent_mempool_prevouts_v1";
/// Capability advertised for `IngestControl.ChainValuePoolsAtTip`.
pub const INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1: &str =
    "ingest.control.chain_value_pools_at_tip_v1";

/// Capability for `IngestControl.WriterStatus.phase`.
///
/// Advertises the classifier-driven `zinder.v1.ingest.WriterPhase`
/// vocabulary wired in
/// [ADR-0015](../../../docs/adrs/0015-unified-phase-driven-ingest.md).
pub const INGEST_WRITER_PHASE_V1: &str = "ingest.writer.phase_v1";

/// Capabilities always advertised by `IngestControl`.
///
/// Source-backed capabilities are appended by the runtime only when their
/// backing source handle advertises the required node capability.
pub const INGEST_CONTROL_ALWAYS_ON_CAPABILITIES: &[&str] = &[
    INGEST_CONTROL_SERVER_INFO_V1,
    INGEST_CONTROL_WRITER_STATUS_V1,
    INGEST_CONTROL_CHAIN_EVENTS_V1,
    INGEST_CONTROL_MEMPOOL_SNAPSHOT_V1,
    INGEST_CONTROL_MEMPOOL_EVENTS_V1,
    INGEST_CONTROL_TRANSPARENT_MEMPOOL_OUTPUTS_BY_ADDRESS_V1,
    INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPEND_BY_OUTPOINT_V1,
    INGEST_CONTROL_TRANSPARENT_MEMPOOL_PREVOUTS_V1,
    INGEST_WRITER_PHASE_V1,
];

/// Known capability strings exposed by `IngestControl`.
///
/// Returned through the cross-service `ops.ServerInfo.capabilities` field on
/// the `IngestControl.ServerInfo` rpc so orchestration tooling can probe the
/// control-plane surface without an out-of-band schema lookup. Individual
/// server instances filter optional source-backed capabilities at runtime.
pub const INGEST_CONTROL_CAPABILITIES: &[&str] = &[
    INGEST_CONTROL_SERVER_INFO_V1,
    INGEST_CONTROL_WRITER_STATUS_V1,
    INGEST_CONTROL_CHAIN_EVENTS_V1,
    INGEST_CONTROL_MEMPOOL_SNAPSHOT_V1,
    INGEST_CONTROL_MEMPOOL_EVENTS_V1,
    INGEST_CONTROL_TRANSPARENT_MEMPOOL_OUTPUTS_BY_ADDRESS_V1,
    INGEST_CONTROL_TRANSPARENT_MEMPOOL_SPEND_BY_OUTPOINT_V1,
    INGEST_CONTROL_TRANSPARENT_MEMPOOL_PREVOUTS_V1,
    INGEST_CONTROL_CHAIN_VALUE_POOLS_AT_TIP_V1,
    INGEST_WRITER_PHASE_V1,
];
/// Mempool-overlay path for `WalletQuery.TransparentAddressBalance`.
///
/// Coexists with [`WALLET_ADDRESS_TRANSPARENT_BALANCE_V1`] when the explorer
/// plane is configured and ready. Signals that the same response carries the
/// live mempool overlay in `unconfirmed_delta_zat`. This is the federated form
/// advertised by `zinder-explorer` and proxied through `WalletQuery`; see
/// [ADR-0009](../../../docs/adrs/0009-explorer-plane-as-product-surface.md).
pub const EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1: &str = "explorer.transparent_address.balance_v1";

/// Active capability strings advertised by a Zinder deployment.
///
/// Adding a `WalletQuery` RPC requires extending this list. Removing a
/// capability is a deprecation step under the capability-descriptor contract
/// (see [Public interfaces §Capability Discovery](../../docs/architecture/public-interfaces.md#capability-discovery)).
pub const ZINDER_CAPABILITIES: &[&str] = &[
    WALLET_READ_LATEST_BLOCK_V1,
    WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
    WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1,
    WALLET_READ_COMPACT_BLOCK_AT_V1,
    WALLET_READ_COMPACT_BLOCK_RANGE_V1,
    WALLET_READ_FULL_BLOCK_AT_V1,
    WALLET_READ_TREE_STATE_AT_V1,
    WALLET_READ_LATEST_TREE_STATE_V1,
    WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1,
    WALLET_READ_TRANSACTION_BY_ID_V1,
    WALLET_READ_SERVER_INFO_V1,
    WALLET_BROADCAST_TRANSACTION_V1,
    WALLET_EVENTS_CHAIN_V1,
    WALLET_SNAPSHOT_MEMPOOL_V1,
    WALLET_EVENTS_MEMPOOL_V1,
    WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1,
    WALLET_MEMPOOL_TRANSPARENT_SPEND_BY_OUTPOINT_V1,
    WALLET_MEMPOOL_TRANSPARENT_PREVOUTS_V1,
    WALLET_READ_TRANSPARENT_PREVOUTS_V1,
    WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1,
    WALLET_ADDRESS_TRANSPARENT_UTXOS_V1,
    WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
    WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
    EXPLORER_SERVER_INFO_V1,
    EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
    EXPLORER_TRANSACTION_DETAIL_V1,
    EXPLORER_BLOCK_SUMMARY_V1,
    EXPLORER_BLOCK_DETAIL_V1,
    EXPLORER_SEARCH_V1,
    EXPLORER_MEMPOOL_SUMMARY_V1,
    EXPLORER_MEMPOOL_ACTIVITY_V1,
    EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1,
    EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_VALUE_POOL_SUMMARY_V1,
];

/// Helpers for client-side capability discovery.
///
/// Implemented by every per-service descriptor (`WalletServerInfo`,
/// `ExplorerServerInfo`) plus the cross-service `ops::ServerInfo` they embed.
/// Capability discovery always reads from the embedded `ops::ServerInfo`;
/// per-service descriptors delegate.
pub trait CapabilityDescriptor {
    /// Returns true if the descriptor advertises `capability` under
    /// [`ZINDER_CAPABILITIES`] semantics.
    fn has(&self, capability: &str) -> bool;
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
