//! gRPC adapters served by the derive plane.

mod adapter;

pub use adapter::{
    DERIVE_EXPLORER_READY_CAPABILITY, ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings,
};
