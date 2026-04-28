//! Zinder protocol ownership boundary.
//!
//! Service protocol schemas and generated modules live here so service crates
//! do not hand-write protocol-shaped payload structs.

pub mod capabilities;
pub use capabilities::{CapabilityDescriptor, ZINDER_CAPABILITIES};

/// Encoded descriptor set for native Zinder v1 protobuf services.
pub const ZINDER_V1_FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/zinder_v1_descriptor.bin"));

/// Native Zinder protocol modules.
pub mod v1 {
    /// Private ingest control-plane protocol messages.
    pub mod ingest {
        #![allow(
            clippy::allow_attributes_without_reason,
            clippy::default_trait_access,
            clippy::derive_partial_eq_without_eq,
            clippy::disallowed_names,
            clippy::doc_markdown,
            clippy::missing_fields_in_debug,
            clippy::must_use_candidate,
            clippy::too_many_lines,
            missing_docs,
            reason = "Generated protobuf code mirrors owned schemas."
        )]

        include!(concat!(env!("OUT_DIR"), "/zinder.v1.ingest.rs"));
    }

    /// Native wallet and wallet-like application protocol messages.
    pub mod wallet {
        #![allow(
            clippy::allow_attributes_without_reason,
            clippy::default_trait_access,
            clippy::derive_partial_eq_without_eq,
            clippy::disallowed_names,
            clippy::doc_markdown,
            clippy::missing_fields_in_debug,
            clippy::must_use_candidate,
            clippy::too_many_lines,
            missing_docs,
            reason = "Generated protobuf code mirrors owned schemas."
        )]

        include!(concat!(env!("OUT_DIR"), "/zinder.v1.wallet.rs"));
    }
}

/// Compatibility protocol modules.
pub mod compat {
    /// Vendored lightwalletd-compatible protocol messages.
    pub mod lightwalletd {
        #![allow(
            clippy::allow_attributes_without_reason,
            clippy::default_trait_access,
            clippy::derive_partial_eq_without_eq,
            clippy::doc_markdown,
            clippy::doc_overindented_list_items,
            clippy::disallowed_names,
            clippy::must_use_candidate,
            clippy::too_many_lines,
            clippy::too_long_first_doc_paragraph,
            missing_docs,
            reason = "Generated protobuf code mirrors vendored schemas."
        )]

        /// Upstream `lightwallet-protocol` commit served by this compatibility pin.
        pub const LIGHTWALLETD_PROTOCOL_COMMIT: &str = env!("LIGHTWALLETD_PROTOCOL_COMMIT");

        include!(concat!(env!("OUT_DIR"), "/cash.z.wallet.sdk.rpc.rs"));
    }
}
