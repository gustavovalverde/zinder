//! gRPC adapters served by the explorer plane.

mod adapter;
mod block_view;
mod fee_summary;
mod mempool;
mod search;
mod transaction_detail;
mod transparent_address_activity;
mod value_pool_summary;

pub use adapter::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
