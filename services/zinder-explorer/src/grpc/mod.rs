//! gRPC adapters served by the explorer plane.

mod adapter;
mod block_view;
mod chain_reorg_history;
mod error;
mod fee_summary;
mod freshness;
mod mempool;
mod mempool_event_counts;
mod overview_snapshot;
mod recent_transactions;
mod search;
mod transaction_detail;
mod transparent_address_activity;
mod transparent_address_deltas;
mod utxo_set_summary;
mod value_pool_summary;

pub use adapter::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings, describe_request_metrics};

/// Clamps a caller-requested page size to a default and a hard cap.
///
/// `requested == 0` selects `default`; any other value is bounded by `cap`.
pub(crate) const fn clamp_max_entries(requested: u32, default: u32, cap: u32) -> u32 {
    let target = if requested == 0 { default } else { requested };
    if target > cap { cap } else { target }
}
