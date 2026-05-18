//! Proto-enum mappings between Zinder native types and generated wire types.
//!
//! Every native-to-proto enum translation lives in this module. Each
//! submodule owns one concept (currently privacy-shape) and exposes
//! `encode_*` / `decode_*` functions named after the convention they
//! implement. Inline `match shape { PrivacyShape::TransparentOnly => ... }`
//! tables outside this module are a forbidden pattern; call the encoder.
//!
//! The pure-domain side (proto-agnostic key codecs, byte-order helpers)
//! lives in `crates/zinder-core/src/wire/`. This module is its proto-aware
//! twin: it owns the translations whose return type is one of the
//! generated `zinder_proto::v1::*` enums.

mod privacy_shape;

pub use privacy_shape::encode_privacy_shape;
