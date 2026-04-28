//! Zinder capability strings advertised through `WalletQuery.ServerInfo`.
//!
//! Capability strings are exact-match. New methods on `WalletQuery` add a
//! capability string here in the same change. The `capability-coverage` CI job
//! asserts that every RPC has a corresponding entry. The full protocol
//! contract is in
//! [docs/specs/m2-push-primitive.md §D4](../../docs/specs/m2-push-primitive.md#d4-capability-descriptor-was-adr-0010).
//!
//! Capability naming follows `domain.subdomain.capability_name_v{N}`. Versioned
//! suffixes are part of the capability identity; a `_v2` capability is a
//! separate string from its `_v1` predecessor and may coexist during a
//! deprecation window.

use crate::v1::wallet::ServerCapabilities;

/// Active capability strings advertised by a Zinder deployment.
///
/// Adding a `WalletQuery` RPC requires extending this list. Removing a
/// capability is a deprecation step under the capability-descriptor contract
/// (see [Public interfaces §Capability Discovery](../../docs/architecture/public-interfaces.md#capability-discovery)).
pub const ZINDER_CAPABILITIES: &[&str] = &[
    "wallet.read.latest_block_v1",
    "wallet.read.compact_block_at_v1",
    "wallet.read.compact_block_range_v1",
    "wallet.read.tree_state_at_v1",
    "wallet.read.latest_tree_state_v1",
    "wallet.read.subtree_roots_in_range_v1",
    "wallet.read.transaction_by_id_v1",
    "wallet.read.server_info_v1",
    "wallet.broadcast.transaction_v1",
    "wallet.events.chain_v1",
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
