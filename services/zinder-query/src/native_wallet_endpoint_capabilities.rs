//! Immutable native capability evidence owned by an admitted wallet query.

use std::sync::Arc;

use zinder_proto::capabilities::{self, CapabilitySurface, capabilities_for_surface};
use zinder_source::{NodeCapabilities, NodeCapability};
use zinder_store::RawBlobRetention;

/// Immutable native capability set for one admitted wallet endpoint.
///
/// Callers can inspect this value, but cannot construct it from arbitrary
/// strings or mutate it after query composition. The private constructors
/// below bind every claim to the concrete query implementation and its
/// admitted storage or upstream evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeWalletEndpointCapabilities {
    ordered: Arc<[&'static str]>,
}

impl NativeWalletEndpointCapabilities {
    /// Returns whether this endpoint structurally supports `capability`.
    #[must_use]
    pub fn contains(&self, capability: &str) -> bool {
        self.ordered.contains(&capability)
    }

    /// Iterates capability identifiers in stable registry order.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.ordered.iter().copied()
    }

    /// Shares the exact immutable identifier slice owned by this admitted
    /// query with an operational discovery surface.
    #[must_use]
    pub fn shared_identifiers(&self) -> Arc<[&'static str]> {
        Arc::clone(&self.ordered)
    }

    pub(crate) fn has_node_backed_capabilities(&self) -> bool {
        self.contains(capabilities::WALLET_READ_TREE_STATE_AT_HEIGHT_V2)
            || self.contains(capabilities::WALLET_BROADCAST_TRANSACTION_V1)
    }

    /// Derives the capabilities implemented by the exact serving-pair query.
    ///
    /// `node_capabilities` must be the admitted evidence from the same source
    /// handle installed as the query's tree-state provider and transaction
    /// broadcaster.
    pub(crate) fn for_wallet_serving_pair(
        raw_blob_retention: RawBlobRetention,
        node_capabilities: NodeCapabilities,
    ) -> Self {
        let openrpc_discovery_admitted =
            node_capabilities.supports(NodeCapability::OpenRpcDiscovery);
        Self::from_predicate(|capability| {
            let implemented_without_optional_evidence = matches!(
                capability,
                capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1
                    | capabilities::WALLET_READ_SETTLED_TIP_BLOCK_V1
                    | capabilities::WALLET_READ_COMPACT_BLOCK_AT_V2
                    | capabilities::WALLET_READ_COMPACT_BLOCK_RANGE_V2
                    | capabilities::WALLET_READ_COMPACT_BLOCK_IRONWOOD_V2
                    | capabilities::WALLET_READ_LATEST_TREE_STATE_CHECKPOINT_V2
                    | capabilities::WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1
                    | capabilities::WALLET_READ_SUBTREE_ROOTS_IRONWOOD_V1
                    | capabilities::WALLET_READ_SERVER_INFO_V2
                    | capabilities::WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1
                    | capabilities::WALLET_EVENTS_CHAIN_V1
                    | capabilities::WALLET_ADDRESS_TRANSPARENT_BALANCE_V1
            );
            implemented_without_optional_evidence
                || (matches!(
                    capability,
                    capabilities::WALLET_READ_FULL_BLOCK_AT_V1
                        | capabilities::WALLET_READ_FULL_BLOCK_RANGE_V1
                ) && raw_blob_retention.retains_block_blobs())
                || (capability == capabilities::WALLET_READ_TREE_STATE_AT_HEIGHT_V2
                    && openrpc_discovery_admitted
                    && node_capabilities.supports(NodeCapability::TreeState))
                || (capability == capabilities::WALLET_BROADCAST_TRANSACTION_V1
                    && openrpc_discovery_admitted
                    && node_capabilities.supports(NodeCapability::TransactionBroadcast))
        })
    }

    /// Conservative contract for the temporary generic primary-store query.
    ///
    /// This composition is removed after its remaining consumers move to the
    /// serving-pair query. Until then it advertises only operations guaranteed
    /// by `ChainEpochReadApi` itself, never optional stores or providers.
    pub(crate) fn for_chain_epoch_read_api() -> Self {
        Self::from_predicate(|capability| {
            matches!(
                capability,
                capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1
                    | capabilities::WALLET_READ_SETTLED_TIP_BLOCK_V1
                    | capabilities::WALLET_READ_BLOCK_ID_BY_SELECTOR_V1
                    | capabilities::WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1
                    | capabilities::WALLET_READ_COMPACT_BLOCK_AT_V2
                    | capabilities::WALLET_READ_COMPACT_BLOCK_RANGE_V2
                    | capabilities::WALLET_READ_COMPACT_BLOCK_IRONWOOD_V2
                    | capabilities::WALLET_READ_LATEST_TREE_STATE_CHECKPOINT_V2
                    | capabilities::WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1
                    | capabilities::WALLET_READ_SUBTREE_ROOTS_IRONWOOD_V1
                    | capabilities::WALLET_READ_TRANSACTION_BY_ID_V2
                    | capabilities::WALLET_READ_TRANSPARENT_OUTPUTS_V1
                    | capabilities::WALLET_READ_TRANSPARENT_SPENDS_V1
                    | capabilities::WALLET_READ_TRANSPARENT_UNSPENT_OUTPUTS_V1
                    | capabilities::WALLET_READ_TRANSPARENT_UTXO_SET_SUMMARY_V1
                    | capabilities::WALLET_READ_SERVER_INFO_V2
                    | capabilities::WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1
                    | capabilities::WALLET_EVENTS_CHAIN_V1
                    | capabilities::WALLET_ADDRESS_TRANSPARENT_UNSPENT_OUTPUTS_V1
                    | capabilities::WALLET_ADDRESS_TRANSPARENT_BALANCE_V1
            )
        })
    }

    fn from_predicate(mut is_supported: impl FnMut(&str) -> bool) -> Self {
        let ordered = capabilities_for_surface(CapabilitySurface::Wallet)
            .map(|spec| spec.string)
            .filter(|capability| is_supported(capability))
            .collect::<Vec<_>>()
            .into();
        Self { ordered }
    }
}

/// Diagnostic snapshot from the upstream node admission used by this query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpstreamNodeCapabilities {
    /// Node-reported semantic version when available.
    pub version: Option<String>,
    capabilities: Arc<[&'static str]>,
}

impl UpstreamNodeCapabilities {
    /// Captures stable diagnostic names from admitted source evidence.
    pub(crate) fn from_admitted(node_capabilities: NodeCapabilities) -> Self {
        Self {
            version: None,
            capabilities: node_capabilities
                .iter()
                .map(NodeCapability::name)
                .collect::<Vec<_>>()
                .into(),
        }
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.capabilities.iter().copied()
    }
}
