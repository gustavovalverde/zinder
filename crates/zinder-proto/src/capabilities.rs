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

use crate::v1::wallet::ServerCapabilities;

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
/// [`DERIVE_EXPLORER_TRANSPARENT_BALANCE_V1`]; the wallet capability alone
/// signals confirmed totals computed from canonical UTXOs.
pub const WALLET_ADDRESS_TRANSPARENT_BALANCE_V1: &str = "wallet.address.transparent_balance_v1";
/// Capability advertised for `ExplorerQuery.ServerInfo`.
pub const DERIVE_EXPLORER_SERVER_INFO_V1: &str = "derive.explorer.server_info_v1";
/// Mempool-overlay path for `WalletQuery.TransparentAddressBalance`.
///
/// Coexists with [`WALLET_ADDRESS_TRANSPARENT_BALANCE_V1`] when the derive
/// plane is configured and ready. Signals that the same response carries the
/// live mempool overlay in `unconfirmed_delta_zat`.
pub const DERIVE_EXPLORER_TRANSPARENT_BALANCE_V1: &str = "derive.explorer.transparent_balance_v1";

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
    WALLET_ADDRESS_TRANSPARENT_UTXOS_V1,
    WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
    WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
    DERIVE_EXPLORER_SERVER_INFO_V1,
    DERIVE_EXPLORER_TRANSPARENT_BALANCE_V1,
];

/// Helpers for client-side capability discovery.
pub trait CapabilityDescriptor {
    /// Returns true if the descriptor advertises `capability` under
    /// [`ZINDER_CAPABILITIES`] semantics.
    fn has(&self, capability: &str) -> bool;

    /// Returns the deprecation entry for `capability` when one is present.
    ///
    /// A capability listed in `deprecated_capabilities` continues to function
    /// during the deprecation window; the entry communicates the replacement
    /// and the earliest removal version.
    fn deprecation(&self, capability: &str) -> Option<&crate::v1::wallet::DeprecatedCapability>;
}

impl CapabilityDescriptor for ServerCapabilities {
    fn has(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|advertised| advertised == capability)
    }

    fn deprecation(&self, capability: &str) -> Option<&crate::v1::wallet::DeprecatedCapability> {
        self.deprecated_capabilities
            .iter()
            .find(|entry| entry.capability == capability)
    }
}
