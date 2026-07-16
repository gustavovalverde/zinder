//! Version-1 wallet projection construction and storage for `RocksDB`.

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("Zinder supports only 32-bit and 64-bit pointer-width targets.");

mod build;
mod error;
mod projection_load;
mod store;

pub use build::{
    RocksDbWalletBuildOptions, RocksDbWalletBuildOutcome, RocksDbWalletBuildReport,
    WalletBuildPhaseDurations, build_wallet_from_canonical,
};
pub use error::RocksDbWalletError;
pub use store::{
    RocksDbWalletStore, WALLET_ROCKSDB_SCHEMA_VERSION, WalletAddressTransactionHistoryPage,
    WalletAddressUnspentOutputsPage,
};
