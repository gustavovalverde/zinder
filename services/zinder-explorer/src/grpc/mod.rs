//! gRPC adapters served by the explorer plane.

mod adapter;
mod block_activity;
mod block_view;
mod chain_reorg_history;
mod commitment_root_search;
mod conventional_fee_distribution;
mod displaced_block;
mod endpoint_admission;
mod endpoint_capabilities;
mod error;
mod fee_summary;
mod freshness;
mod intrinsic_value_balances;
mod mempool;
mod mempool_event_counts;
mod migration;
mod network_upgrade_status;
mod overview_snapshot;
mod paid_fee_distribution;
mod recent_transactions;
mod search;
mod transaction_component_summary;
mod transaction_detail;
mod transaction_history;
mod transparent_address_activity;
mod transparent_address_deltas;
mod transparent_address_ranking;
mod transparent_input;
mod utxo_set_summary;
mod value_pool_balance_history;
mod value_pool_flow;
mod value_pool_summary;

pub use adapter::{
    ExplorerEndpointMetadata, ExplorerQueryEndpointComposition, ExplorerQueryGrpcAdapter,
    ExplorerQueryGrpcAdapterBuilder, describe_request_metrics,
};
pub use endpoint_admission::{ExplorerEndpointAdmissionError, ExplorerWalletQueryHealthError};

fn require_matching_chain_epoch(
    expected: zinder_core::ChainEpoch,
    actual: zinder_core::ChainEpoch,
) -> Result<(), tonic::Status> {
    if actual != expected {
        return Err(error::ExplorerError::internal(format!(
            "WalletQuery chain epoch identity mismatch: expected {expected:?}, received {actual:?}",
        ))
        .into());
    }
    Ok(())
}

/// Clamps a caller-requested page size to a default and a hard cap.
///
/// `requested == 0` selects `default`; any other value is bounded by `cap`.
pub(crate) const fn clamp_max_entries(requested: u32, default: u32, cap: u32) -> u32 {
    let target = if requested == 0 { default } else { requested };
    if target > cap { cap } else { target }
}
