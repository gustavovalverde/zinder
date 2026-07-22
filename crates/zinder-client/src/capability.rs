//! Client-owned wallet capability vocabulary.

use zinder_proto::capabilities::{
    WALLET_ADDRESS_TRANSPARENT_BALANCE_V1, WALLET_BROADCAST_TRANSACTION_V1, WALLET_EVENTS_CHAIN_V1,
    WALLET_EVENTS_MEMPOOL_V2, WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1,
    WALLET_READ_FULL_BLOCK_AT_V1, WALLET_READ_FULL_BLOCK_RANGE_V1,
    WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1, WALLET_SNAPSHOT_MEMPOOL_V3,
};

/// A wallet-plane capability advertised by a Zinder endpoint.
///
/// Known variants cover the optional operations consumers probe today.
/// [`Self::Unknown`] preserves additive capability strings emitted by a newer
/// server so callers can still perform exact-match discovery.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Capability {
    /// Raw transaction broadcast.
    Broadcast,
    /// Cursor-resumable chain-event stream.
    ChainEvents,
    /// Tip-certified bounded mempool snapshot.
    MempoolSnapshot,
    /// Replayable mempool-event stream.
    MempoolEvents,
    /// Chain value-pool totals at the upstream tip.
    ChainValuePools,
    /// Transparent-address balance.
    TransparentAddressBalance,
    /// Serialized full block at one height.
    FullBlock,
    /// Stream of serialized full blocks over a bounded range.
    FullBlockRange,
    /// Named network-upgrade activation heights and branch identifiers.
    NetworkUpgradeActivations,
    /// Additive capability emitted by a newer server.
    Unknown(String),
}

impl Capability {
    /// Returns the exact wire string advertised by the server.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Broadcast => WALLET_BROADCAST_TRANSACTION_V1,
            Self::ChainEvents => WALLET_EVENTS_CHAIN_V1,
            Self::MempoolSnapshot => WALLET_SNAPSHOT_MEMPOOL_V3,
            Self::MempoolEvents => WALLET_EVENTS_MEMPOOL_V2,
            Self::ChainValuePools => WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1,
            Self::TransparentAddressBalance => WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
            Self::FullBlock => WALLET_READ_FULL_BLOCK_AT_V1,
            Self::FullBlockRange => WALLET_READ_FULL_BLOCK_RANGE_V1,
            Self::NetworkUpgradeActivations => WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1,
            Self::Unknown(capability) => capability,
        }
    }

    pub(crate) fn from_wire_name(capability: String) -> Self {
        match capability.as_str() {
            WALLET_BROADCAST_TRANSACTION_V1 => Self::Broadcast,
            WALLET_EVENTS_CHAIN_V1 => Self::ChainEvents,
            WALLET_SNAPSHOT_MEMPOOL_V3 => Self::MempoolSnapshot,
            WALLET_EVENTS_MEMPOOL_V2 => Self::MempoolEvents,
            WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1 => Self::ChainValuePools,
            WALLET_ADDRESS_TRANSPARENT_BALANCE_V1 => Self::TransparentAddressBalance,
            WALLET_READ_FULL_BLOCK_AT_V1 => Self::FullBlock,
            WALLET_READ_FULL_BLOCK_RANGE_V1 => Self::FullBlockRange,
            WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1 => Self::NetworkUpgradeActivations,
            _ => Self::Unknown(capability),
        }
    }
}

/// Exact-match capability discovery for a server descriptor.
pub trait CapabilityDescriptor {
    /// Returns true when the descriptor advertises `capability` exactly.
    fn has(&self, capability: &str) -> bool;

    /// Returns true when the descriptor advertises the typed capability.
    fn supports(&self, capability: Capability) -> bool {
        self.has(capability.as_str())
    }
}
