//! Shared config sections used by every Zinder service binary.
//!
//! Each module owns one section of the canonical TOML layout
//! (`docs/architecture/public-interfaces.md`). Section structs are
//! consumed by the per-service `RawConfig` and the matching
//! [`crate::ConfigLoader`] helpers (`with_*_section`) wire the per-service
//! defaults so a new service cannot drift from the established schema.

pub mod defaults;
mod ingest_control;
mod ops;
mod retention;
mod service;
mod storage;

pub use ingest_control::{
    IngestControlReaderToml, IngestControlSection, IngestControlWriterToml,
    ResolvedIngestControlReader, ResolvedIngestControlWriter, resolve_ingest_control_reader,
    resolve_ingest_control_writer,
};
pub use ops::{OpsSection, OpsToml, resolve_ops_listen_addr};
pub use retention::{ResolvedRetention, RetentionSection, RetentionToml, resolve_retention};
pub use service::ServiceIdentifier;
pub use storage::{
    CanonicalSecondaryStorageSection, CanonicalSecondaryStorageToml, PrimaryStorageSection,
    PrimaryStorageToml, ResolvedCanonicalSecondaryStorage, ResolvedPrimaryStorage,
    ResolvedSecondaryStorage, RocksDbResourceBudgetSection, RocksDbResourceBudgetToml,
    SecondaryStorageSection, SecondaryStorageToml, StorageRoleSection, StorageRoleToml,
    resolve_canonical_rocksdb_budget, resolve_canonical_secondary_storage,
    resolve_derive_rocksdb_budget, resolve_primary_storage, resolve_secondary_storage,
};
