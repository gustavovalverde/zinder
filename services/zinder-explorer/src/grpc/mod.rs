//! gRPC adapters served by the explorer plane.

mod adapter;
mod block_view;
mod search;
mod transaction_detail;

pub use adapter::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
