//! gRPC adapters served by the explorer plane.

mod adapter;
mod block_view;
mod fee_summary;
mod mempool;
mod search;
mod transaction_detail;
mod transparent_address_activity;

pub use adapter::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
