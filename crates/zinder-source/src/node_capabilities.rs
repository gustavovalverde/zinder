//! Node capability vocabulary.

use std::fmt;

use thiserror::Error;

/// Capability exposed by an upstream node source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NodeCapability {
    /// Source can fetch blocks from the current best chain.
    BestChainBlocks,
    /// Source can report the current tip identity (height and hash).
    TipId,
    /// Source can provide tree-state data for fetched blocks.
    TreeState,
    /// Source can provide shielded subtree roots.
    SubtreeRoots,
    /// Source can report or infer finalized height.
    FinalizedHeight,
    /// Source has an explicit readiness probe.
    ReadinessProbe,
    /// Source can broadcast transactions.
    TransactionBroadcast,
    /// Source speaks JSON-RPC and accepted at least one request.
    JsonRpc,
    /// Source returned a structured `rpc.discover` (`OpenRPC`) response.
    OpenRpcDiscovery,
}

impl fmt::Display for NodeCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl NodeCapability {
    /// Stable configuration and diagnostic name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BestChainBlocks => "best_chain_blocks",
            Self::TipId => "tip_id",
            Self::TreeState => "tree_state",
            Self::SubtreeRoots => "subtree_roots",
            Self::FinalizedHeight => "finalized_height",
            Self::ReadinessProbe => "readiness_probe",
            Self::TransactionBroadcast => "transaction_broadcast",
            Self::JsonRpc => "json_rpc",
            Self::OpenRpcDiscovery => "openrpc_discovery",
        }
    }
}

const ORDERED_CAPABILITIES: &[NodeCapability] = &[
    NodeCapability::BestChainBlocks,
    NodeCapability::TipId,
    NodeCapability::TreeState,
    NodeCapability::SubtreeRoots,
    NodeCapability::FinalizedHeight,
    NodeCapability::ReadinessProbe,
    NodeCapability::TransactionBroadcast,
    NodeCapability::JsonRpc,
    NodeCapability::OpenRpcDiscovery,
];

const fn capability_bit(capability: NodeCapability) -> u16 {
    match capability {
        NodeCapability::BestChainBlocks => 1 << 0,
        NodeCapability::TipId => 1 << 1,
        NodeCapability::TreeState => 1 << 2,
        NodeCapability::SubtreeRoots => 1 << 3,
        NodeCapability::FinalizedHeight => 1 << 4,
        NodeCapability::ReadinessProbe => 1 << 5,
        NodeCapability::TransactionBroadcast => 1 << 6,
        NodeCapability::JsonRpc => 1 << 7,
        NodeCapability::OpenRpcDiscovery => 1 << 8,
    }
}

/// Capabilities exposed by a configured upstream node source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NodeCapabilities {
    capability_bits: u16,
}

/// Error returned while constructing a node capability set.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum NodeCapabilitiesError {
    /// The same capability was supplied more than once.
    #[error("node capability {capability} was supplied more than once")]
    Duplicate {
        /// Capability that appeared more than once.
        capability: NodeCapability,
    },
}

impl NodeCapabilities {
    /// Creates a capability set.
    pub fn new(
        capabilities: impl IntoIterator<Item = NodeCapability>,
    ) -> Result<Self, NodeCapabilitiesError> {
        let mut capability_bits = 0;
        for capability in capabilities {
            let capability_bit = capability_bit(capability);
            if capability_bits & capability_bit != 0 {
                return Err(NodeCapabilitiesError::Duplicate { capability });
            }

            capability_bits |= capability_bit;
        }

        Ok(Self { capability_bits })
    }

    pub(crate) fn from_trusted(capabilities: impl IntoIterator<Item = NodeCapability>) -> Self {
        let mut capability_bits = 0;
        for capability in capabilities {
            capability_bits |= capability_bit(capability);
        }

        Self { capability_bits }
    }

    /// Returns true when the source supports `capability`.
    #[must_use]
    pub fn supports(&self, capability: NodeCapability) -> bool {
        self.capability_bits & capability_bit(capability) != 0
    }

    /// Iterates capabilities in stable diagnostic order.
    pub fn iter(&self) -> impl Iterator<Item = NodeCapability> + '_ {
        ORDERED_CAPABILITIES
            .iter()
            .copied()
            .filter(|capability| self.supports(*capability))
    }

    /// Returns the capabilities in stable diagnostic order.
    #[must_use]
    pub fn to_vec(self) -> Vec<NodeCapability> {
        self.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeCapabilities, NodeCapabilitiesError, NodeCapability};

    #[test]
    fn new_rejects_duplicate_capabilities() {
        assert!(matches!(
            NodeCapabilities::new([NodeCapability::TipId, NodeCapability::TipId,]),
            Err(NodeCapabilitiesError::Duplicate {
                capability: NodeCapability::TipId,
            })
        ));
    }

    #[test]
    fn iter_returns_stable_deduplicated_order() -> Result<(), NodeCapabilitiesError> {
        let capabilities = NodeCapabilities::new([
            NodeCapability::TransactionBroadcast,
            NodeCapability::BestChainBlocks,
        ])?;

        assert_eq!(
            capabilities.to_vec(),
            vec![
                NodeCapability::BestChainBlocks,
                NodeCapability::TransactionBroadcast,
            ]
        );
        assert!(capabilities.supports(NodeCapability::BestChainBlocks));
        assert!(!capabilities.supports(NodeCapability::SubtreeRoots));

        Ok(())
    }
}
