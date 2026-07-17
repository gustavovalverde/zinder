//! Version-1 wallet projection contracts independent of storage engines.
//!
//! This crate owns the exact durable keys and values needed by wallet-facing
//! reads. It does not depend on canonical-store readers or the legacy derive
//! plane.

#[cfg(not(any(target_pointer_width = "32", target_pointer_width = "64")))]
compile_error!("Zinder supports only 32-bit and 64-bit pointer-width targets.");

mod contract_error;
mod control;
mod digest;
mod rows;
mod serial_oracle;

pub use contract_error::WalletProjectionContractError;
pub use control::{
    WalletCanonicalSourceIdentity, WalletProjectionBuildPlan, WalletProjectionBuildState,
    WalletProjectionDigest, WalletProjectionFamilyRowCounts, WalletProjectionReadyEvidence,
    WalletProjectionSourcePosition, WalletStoreControl, WalletUtxoSetSummary,
};
pub use digest::{WalletProjectionDigestBuilder, WalletProjectionRowFamily};
pub use rows::{
    WalletAddressBalance, WalletAddressTransaction, WalletAddressTransactionKey,
    WalletAddressUnspentOutputKey, WalletOutpointKey, WalletReorgUndo, WalletSpentOutput,
    WalletTransactionPosition, WalletUnspentOutput,
};
pub use serial_oracle::WalletProjectionSerialOracle;

/// Initial and only wallet projection schema version.
pub const WALLET_PROJECTION_SCHEMA_VERSION: u16 = 1;

/// Initial and only wallet projection value-encoding version.
pub const WALLET_PROJECTION_VALUE_ENCODING_VERSION: u16 = 1;

/// Canonical store schema admitted by this projection schema.
pub const REQUIRED_CANONICAL_STORE_SCHEMA_VERSION: u16 = 1;

/// Canonical replay format admitted by this projection schema.
pub const REQUIRED_CANONICAL_REPLAY_FORMAT_VERSION: u32 = 1;

/// Canonical block-facts digest admitted by this projection schema.
pub const REQUIRED_CANONICAL_FACTS_DIGEST_VERSION: u16 = 1;

/// Canonical sequence digest admitted by this projection schema.
pub const REQUIRED_CANONICAL_SEQUENCE_DIGEST_VERSION: u16 = 1;

/// Fixed key of the singleton wallet control record.
pub const WALLET_STORE_CONTROL_KEY: &[u8] = b"store_control";

/// Persisted identity admitted as a wallet projection store.
pub const WALLET_PROJECTION_STORE_IDENTITY: &[u8] = b"wallet-projection";
