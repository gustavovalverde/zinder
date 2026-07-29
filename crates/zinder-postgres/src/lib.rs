//! `PostgreSQL` data plane for [`zinder_runtime::DeploymentTopology::PostgresHorizontal`].
//!
//! This package currently owns PostgreSQL-specific connection, migration, and
//! canonical tracer mechanics. It exposes domain-shaped operations rather than
//! a generic SQL or storage adapter.

mod canonical;
mod database;
mod migration;

pub use canonical::{
    CanonicalAppend, CanonicalAppendOutcome, CanonicalPersistenceError, CanonicalState,
    CanonicalStore,
};
pub use database::{DatabaseConfig, DatabaseError};
pub use migration::{DatabaseState, MigrationError, MigrationOutcome, migrate_database};
