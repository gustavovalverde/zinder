//! Native to wire dialect conversions for Zinder domain types.
//!
//! Every native-to-wire translation lives in this module. Each submodule owns
//! one concept (`chain_name`, `transaction_id`, `block_hash`, `branch_id`)
//! across all dialects that use it. Function names disclose the convention:
//! `encode_internal_*` and `decode_internal_*` for proto `bytes` fields,
//! `encode_display_*_hex` and `decode_display_*_hex` for hex-string surfaces.
//!
//! [`encode_branch_id_hex`] drops the `_internal_`/`_display_` qualifier
//! because consensus branch ids have a single canonical wire form (the
//! `{:08x}` lowercase hex string used by lightwalletd and Zebra
//! `getblockchaininfo`); there is no byte-order companion to disambiguate
//! against. The single-form chain-name encoders ([`encode_bip70_chain_name`],
//! [`encode_zinder_native_chain_name`]) follow the same pattern.
//!
//! Adding a new wire field or a new ingress dialect MUST route through a
//! function here. Inline `transaction_id.as_bytes()`, inline
//! `format!("{:08x}", ...)`, and inline byte reversal at wire boundaries are
//! forbidden patterns; the structural test at
//! `crates/zinder-core/tests/wire_invariants.rs` enforces this.

mod address_script_hash;
mod block_hash;
mod branch_id;
mod chain_name;
mod height_key;
mod in_block_position;
mod transaction_id;
mod unix_seconds;

pub use address_script_hash::{
    ADDRESS_SCRIPT_HASH_LEN, decode_address_script_hash, encode_address_script_hash,
};
pub use block_hash::{
    decode_display_block_hash_hex, decode_internal_block_hash, encode_display_block_hash_hex,
    encode_internal_block_hash,
};
pub use branch_id::{decode_branch_id_hex, encode_branch_id_hex};
pub use chain_name::{
    decode_zinder_native_chain_name, encode_bip70_chain_name, encode_zinder_native_chain_name,
};
pub use height_key::{
    HEIGHT_KEY_LEN, decode_height_key_ascending, decode_height_key_descending,
    encode_height_key_ascending, encode_height_key_descending,
};
pub use in_block_position::{
    IN_BLOCK_POSITION_KEY_LEN, decode_in_block_position, encode_in_block_position,
};
pub use transaction_id::{
    decode_display_transaction_id_hex, decode_internal_transaction_id,
    encode_display_transaction_id_hex, encode_internal_transaction_id,
};
pub use unix_seconds::{UNIX_SECONDS_KEY_LEN, decode_unix_seconds, encode_unix_seconds};

/// Errors returned by `decode_*` functions in [`crate::wire`].
///
/// Encode operations are infallible by construction (the input is a typed
/// domain value with a fixed byte length). Decode operations may receive
/// arbitrary bytes or strings and must report the failure mode the caller can
/// recover from.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WireDecodeError {
    /// Input byte slice was not the expected length for the target type.
    #[error("wire decode expected {expected} bytes, received {actual}")]
    InvalidLength {
        /// Number of bytes the target type requires.
        expected: usize,
        /// Number of bytes the caller supplied.
        actual: usize,
    },

    /// Input hex string failed to decode.
    #[error("wire decode invalid hex: {reason}")]
    InvalidHex {
        /// Human-readable description of the hex decode failure.
        reason: String,
    },

    /// Input did not match a known enum discriminant on the named dialect.
    #[error("wire decode unrecognized {dialect} enum discriminant: {discriminant}")]
    UnrecognizedEnumDiscriminant {
        /// Wire dialect that produced the input (for example "lightwalletd", "zinder-native").
        dialect: &'static str,
        /// Discriminant value that did not match any known variant.
        discriminant: i32,
    },

    /// Input string did not match any known value for the named dialect.
    #[error("wire decode unrecognized {dialect} string: {input}")]
    UnrecognizedString {
        /// Wire dialect that produced the input.
        dialect: &'static str,
        /// String the caller supplied that did not match any known value.
        input: String,
    },
}
