//! Chain value-pool balances surfaced by the upstream node.
//!
//! Zebra reports chain-wide totals through
//! `getblockchaininfo.valuePools`. Zinder keeps that list-shaped contract
//! instead of normalizing into a fixed set of known pool names so future
//! pools can flow through the boundary without a wire-shape change.

use crate::{BlockId, ChainEpoch};

/// One cumulative value-pool balance reported for a historical block.
///
/// Pool identifiers are deliberately dynamic so a node can advertise future
/// consensus pools without a Zinder type or storage-schema change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValuePoolBalance {
    /// Stable upstream pool identifier.
    pub id: String,
    /// Whether the upstream node monitors this pool's value.
    pub monitored: bool,
    /// Cumulative pool balance after the block, in zatoshi.
    ///
    /// `None` preserves an advertised pool whose value is unavailable.
    pub value_zat: Option<u64>,
}

impl ValuePoolBalance {
    /// Builds one cumulative value-pool balance.
    #[must_use]
    pub fn new(id: impl Into<String>, monitored: bool, value_zat: Option<u64>) -> Self {
        Self {
            id: id.into(),
            monitored,
            value_zat,
        }
    }
}

/// Authoritative cumulative value-pool balances after one block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockValuePoolBalances {
    /// Exact block identity attached to the upstream observation.
    pub block_id: BlockId,
    /// Block timestamp returned by the same upstream observation.
    pub block_time_seconds: i64,
    /// Value-pool balances in upstream-advertised order.
    pub pools: Vec<ValuePoolBalance>,
}

impl BlockValuePoolBalances {
    /// Builds one block-bound cumulative value-pool balance artifact.
    #[must_use]
    pub fn new(block_id: BlockId, block_time_seconds: i64, pools: Vec<ValuePoolBalance>) -> Self {
        Self {
            block_id,
            block_time_seconds,
            pools,
        }
    }
}

/// One upstream value-pool entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainValuePool {
    /// Stable upstream pool identifier, such as `transparent`, `sapling`,
    /// `orchard`, or a future consensus pool id.
    pub id: String,
    /// Whether the upstream node monitors this pool's value.
    pub monitored: bool,
    /// Chain-wide pool total at the observed tip, in zatoshi.
    ///
    /// Zebra uses signed integers for `chainValueZat`. Healthy chains should
    /// report non-negative totals, but negative values are preserved as a
    /// data-integrity signal rather than wrapped. `None` means the upstream
    /// node reported the pool id but did not expose its value.
    pub chain_value_zat: Option<i64>,
}

impl ChainValuePool {
    /// Builds one value-pool entry.
    #[must_use]
    pub fn new(id: impl Into<String>, monitored: bool, chain_value_zat: Option<i64>) -> Self {
        Self {
            id: id.into(),
            monitored,
            chain_value_zat,
        }
    }
}

/// Chain-wide value pool totals at a particular source tip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainValuePools {
    /// Source tip the upstream node reported when computing the pools.
    pub source_tip: BlockId,
    /// Upstream value-pool entries, preserved in upstream order.
    pub pools: Vec<ChainValuePool>,
}

impl ChainValuePools {
    /// Builds a tip-bound value-pool snapshot.
    #[must_use]
    pub fn new(source_tip: BlockId, pools: Vec<ChainValuePool>) -> Self {
        Self { source_tip, pools }
    }
}

/// Wallet-plane chain value-pool response bound to a visible chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainValuePoolsAtTip {
    /// Chain epoch visible to Zinder when the upstream value-pool read was
    /// answered.
    pub chain_epoch: ChainEpoch,
    /// Source tip the upstream node reported when computing the pools.
    pub source_tip: BlockId,
    /// Upstream value-pool entries, preserved in upstream order.
    pub pools: Vec<ChainValuePool>,
}

impl ChainValuePoolsAtTip {
    /// Binds a source value-pool snapshot to a wallet-plane chain epoch.
    #[must_use]
    pub fn from_source(chain_epoch: ChainEpoch, source_value_pools: ChainValuePools) -> Self {
        Self {
            chain_epoch,
            source_tip: source_value_pools.source_tip,
            pools: source_value_pools.pools,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ArtifactSchemaVersion, BlockHash, BlockHeight, BlockId, ChainEpoch, ChainEpochId,
        ChainTipMetadata, Network, UnixTimestampMillis,
    };

    use super::{ChainValuePool, ChainValuePools, ChainValuePoolsAtTip};

    #[test]
    fn wallet_value_pools_preserve_source_tip_identity() {
        let source_tip = BlockId::new(BlockHeight::new(42), BlockHash::from_bytes([0x42; 32]));
        let source_value_pools = ChainValuePools::new(
            source_tip,
            vec![ChainValuePool::new("transparent", true, Some(1))],
        );

        let chain_epoch = ChainEpoch {
            id: ChainEpochId::new(7),
            network: Network::ZcashRegtest,
            visible_tip_height: source_tip.height,
            visible_tip_hash: source_tip.hash,
            settled_tip_height: source_tip.height,
            settled_tip_hash: source_tip.hash,
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1),
        };
        let value_pools = ChainValuePoolsAtTip::from_source(chain_epoch, source_value_pools);

        assert_eq!(value_pools.source_tip, source_tip);
    }
}
