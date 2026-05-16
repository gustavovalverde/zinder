//! Chain value-pool balances surfaced by the upstream node.
//!
//! Zebra reports chain-wide totals through
//! `getblockchaininfo.valuePools`. Zinder keeps that list-shaped contract
//! instead of normalizing into a fixed set of known pool names so future
//! pools can flow through the boundary without a wire-shape change.

use crate::{BlockHeight, ChainEpoch};

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

/// Chain-wide value pool totals at a particular tip height.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainValuePools {
    /// Tip height the upstream node reported when computing the pools.
    pub tip_height: BlockHeight,
    /// Upstream value-pool entries, preserved in upstream order.
    pub pools: Vec<ChainValuePool>,
}

impl ChainValuePools {
    /// Builds a tip-bound value-pool snapshot.
    #[must_use]
    pub fn new(tip_height: BlockHeight, pools: Vec<ChainValuePool>) -> Self {
        Self { tip_height, pools }
    }
}

/// Wallet-plane chain value-pool response bound to a visible chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainValuePoolsAtTip {
    /// Chain epoch visible to Zinder when the upstream value-pool read was
    /// answered.
    pub chain_epoch: ChainEpoch,
    /// Tip height the upstream node reported when computing the pools.
    pub tip_height: BlockHeight,
    /// Upstream value-pool entries, preserved in upstream order.
    pub pools: Vec<ChainValuePool>,
}

impl ChainValuePoolsAtTip {
    /// Binds a source value-pool snapshot to a wallet-plane chain epoch.
    #[must_use]
    pub fn from_source(chain_epoch: ChainEpoch, source_value_pools: ChainValuePools) -> Self {
        Self {
            chain_epoch,
            tip_height: source_value_pools.tip_height,
            pools: source_value_pools.pools,
        }
    }
}
