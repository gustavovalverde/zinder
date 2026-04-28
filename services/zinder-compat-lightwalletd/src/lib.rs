//! lightwalletd-compatible wallet sync adapter.
//!
//! This crate owns compatibility translation for the vendored lightwalletd
//! `CompactTxStreamer` protocol. It consumes [`zinder_query::WalletQueryApi`]
//! and does not open storage, call upstream nodes, or build artifacts.

mod grpc;

pub use grpc::{
    DEFAULT_MAX_LIGHTWALLETD_ADDRESS_UTXOS, DEFAULT_MAX_LIGHTWALLETD_SUBTREE_ROOTS,
    LightwalletdCompatibilityOptions, LightwalletdGrpcAdapter,
};
