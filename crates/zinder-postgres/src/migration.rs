use thiserror::Error;
use zinder_core::{Network, wire::decode_zinder_native_chain_name};
use zinder_runtime::DeploymentTopology;

use crate::database::{DatabaseConfig, DatabaseConnection, DatabaseError};

const DATABASE_IDENTITY: &str = "zinder";
const DATABASE_SCHEMA_VERSION: i16 = 1;
const CANONICAL_WRITER_ROLE: &str = "zinder_ingest";

const MIGRATION_SQL: &str = r"
CREATE SCHEMA IF NOT EXISTS zinder_metadata;
CREATE SCHEMA IF NOT EXISTS canonical;

CREATE TABLE IF NOT EXISTS zinder_metadata.database_identity (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    product_identity TEXT NOT NULL,
    deployment_topology TEXT NOT NULL,
    network TEXT NOT NULL,
    schema_version SMALLINT NOT NULL CHECK (schema_version > 0),
    initialized_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS canonical.writer_fence (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    writer_term BIGINT NOT NULL CHECK (writer_term >= 0)
);

INSERT INTO canonical.writer_fence (
    singleton,
    writer_term
)
VALUES (TRUE, 0)
ON CONFLICT (singleton) DO NOTHING;

CREATE TABLE IF NOT EXISTS canonical.block_facts (
    height BIGINT PRIMARY KEY CHECK (height >= 0 AND height <= 4294967295),
    block_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(block_hash) = 32),
    parent_hash BYTEA NOT NULL CHECK (octet_length(parent_hash) = 32),
    facts_digest_version SMALLINT NOT NULL CHECK (facts_digest_version > 0),
    facts_digest BYTEA NOT NULL CHECK (octet_length(facts_digest) = 32),
    facts_reference_encoding BYTEA NOT NULL,
    replay_format_version INTEGER NOT NULL CHECK (replay_format_version > 0),
    replay_envelope BYTEA NOT NULL
);

CREATE TABLE IF NOT EXISTS canonical.chain_epochs (
    epoch_id BIGINT PRIMARY KEY CHECK (epoch_id > 0),
    previous_epoch_id BIGINT REFERENCES canonical.chain_epochs(epoch_id),
    visible_tip_height BIGINT NOT NULL CHECK (
        visible_tip_height >= 0 AND visible_tip_height <= 4294967295
    ),
    visible_tip_hash BYTEA NOT NULL CHECK (octet_length(visible_tip_hash) = 32),
    committed_from_height BIGINT NOT NULL CHECK (
        committed_from_height >= 0 AND committed_from_height <= 4294967295
    ),
    committed_through_height BIGINT NOT NULL CHECK (
        committed_through_height >= committed_from_height
        AND committed_through_height <= 4294967295
    ),
    writer_term BIGINT NOT NULL CHECK (writer_term > 0),
    sequence_digest_version SMALLINT NOT NULL CHECK (sequence_digest_version > 0),
    sequence_block_count BIGINT NOT NULL CHECK (sequence_block_count > 0),
    sequence_digest BYTEA NOT NULL CHECK (octet_length(sequence_digest) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS canonical.chain_events (
    event_sequence BIGINT PRIMARY KEY CHECK (event_sequence > 0),
    resulting_epoch_id BIGINT NOT NULL REFERENCES canonical.chain_epochs(epoch_id),
    previous_epoch_id BIGINT REFERENCES canonical.chain_epochs(epoch_id),
    event_kind TEXT NOT NULL CHECK (event_kind = 'append'),
    committed_height BIGINT NOT NULL CHECK (
        committed_height >= 0 AND committed_height <= 4294967295
    ),
    committed_hash BYTEA NOT NULL CHECK (octet_length(committed_hash) = 32),
    writer_term BIGINT NOT NULL CHECK (writer_term > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS canonical.control (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    visible_epoch_id BIGINT NOT NULL REFERENCES canonical.chain_epochs(epoch_id),
    event_sequence BIGINT NOT NULL REFERENCES canonical.chain_events(event_sequence),
    visible_tip_height BIGINT NOT NULL CHECK (
        visible_tip_height >= 0 AND visible_tip_height <= 4294967295
    ),
    visible_tip_hash BYTEA NOT NULL CHECK (octet_length(visible_tip_hash) = 32),
    history_predecessor_height BIGINT NOT NULL CHECK (
        history_predecessor_height >= 0 AND history_predecessor_height <= 4294967295
    ),
    history_predecessor_hash BYTEA NOT NULL CHECK (
        octet_length(history_predecessor_hash) = 32
    ),
    writer_term BIGINT NOT NULL CHECK (writer_term > 0),
    sequence_digest_version SMALLINT NOT NULL CHECK (sequence_digest_version > 0),
    sequence_block_count BIGINT NOT NULL CHECK (sequence_block_count > 0),
    sequence_digest BYTEA NOT NULL CHECK (octet_length(sequence_digest) = 32)
);

REVOKE ALL ON SCHEMA zinder_metadata FROM PUBLIC;
REVOKE ALL ON SCHEMA canonical FROM PUBLIC;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;

GRANT USAGE ON SCHEMA zinder_metadata TO zinder_ingest;
GRANT SELECT ON zinder_metadata.database_identity TO zinder_ingest;

GRANT USAGE ON SCHEMA canonical TO zinder_ingest;
GRANT SELECT, UPDATE ON canonical.writer_fence TO zinder_ingest;
GRANT INSERT ON canonical.block_facts TO zinder_ingest;
GRANT INSERT ON canonical.chain_epochs TO zinder_ingest;
GRANT INSERT ON canonical.chain_events TO zinder_ingest;
GRANT SELECT, INSERT, UPDATE ON canonical.control TO zinder_ingest;
";

const READ_DATABASE_STATE_SQL: &str = r"
SELECT product_identity, deployment_topology, network, schema_version
FROM zinder_metadata.database_identity
WHERE singleton = TRUE
";

const INSERT_DATABASE_STATE_SQL: &str = r"
INSERT INTO zinder_metadata.database_identity (
    singleton,
    product_identity,
    deployment_topology,
    network,
    schema_version
)
VALUES (TRUE, $1, $2, $3, $4)
ON CONFLICT (singleton) DO NOTHING
";

/// Result of applying the exact schema version understood by this binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationOutcome {
    /// The schema and database identity were created by this invocation.
    Applied,
    /// The database already contained the exact current schema and identity.
    AlreadyCurrent,
}

/// Persisted admission identity of one `PostgreSQL` Zinder database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseState {
    product_identity: String,
    deployment_topology: DeploymentTopology,
    network: Network,
    schema_version: i16,
}

impl DatabaseState {
    /// Returns the exact state emitted by the current migration version.
    #[must_use]
    pub fn current(network: Network) -> Self {
        Self {
            product_identity: DATABASE_IDENTITY.to_owned(),
            deployment_topology: DeploymentTopology::PostgresHorizontal,
            network,
            schema_version: DATABASE_SCHEMA_VERSION,
        }
    }

    /// Returns the admitted deployment topology.
    #[must_use]
    pub const fn deployment_topology(&self) -> DeploymentTopology {
        self.deployment_topology
    }

    /// Returns the one Zcash network represented by this database.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Returns the exact admitted schema version.
    #[must_use]
    pub const fn schema_version(&self) -> i16 {
        self.schema_version
    }

    /// Reads and validates database identity through a fresh connection.
    pub async fn read(config: &DatabaseConfig) -> Result<Self, MigrationError> {
        let connection = DatabaseConnection::connect(config).await?;
        let state = Self::read_from_connection(&connection, config.network).await?;
        connection.close().await?;
        Ok(state)
    }

    pub(crate) async fn read_from_connection(
        connection: &DatabaseConnection,
        expected_network: Network,
    ) -> Result<Self, MigrationError> {
        let state = read_database_state(&connection.client).await?;
        validate_database_state(&state, expected_network)?;
        Ok(state)
    }
}

/// Failure while applying or admitting the `PostgreSQL` schema.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MigrationError {
    /// Database connection or operation failed.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// The connected role cannot own schema migrations.
    #[error("connected database role lacks migration-only schema authority")]
    MigrationRoleRejected,
    /// A runtime role that migrations grant privileges to has not been provisioned.
    #[error("required PostgreSQL role {role} is absent; provision it before migration")]
    RequiredRoleAbsent {
        /// Stable role name required by this schema version.
        role: &'static str,
    },
    /// The database has no singleton identity row.
    #[error("PostgreSQL database identity is absent")]
    DatabaseIdentityAbsent,
    /// The database belongs to another product or deployment topology.
    #[error(
        "PostgreSQL database identity mismatch: expected zinder/postgres-horizontal, found {product_identity}/{deployment_topology}"
    )]
    DatabaseIdentityMismatch {
        /// Persisted product identity.
        product_identity: String,
        /// Persisted deployment topology.
        deployment_topology: String,
    },
    /// The database belongs to another Zcash network.
    #[error("PostgreSQL database network mismatch: expected {expected}, found {actual}")]
    NetworkMismatch {
        /// Network requested by the process.
        expected: String,
        /// Network persisted by migration.
        actual: String,
    },
    /// The exact schema version differs from the binary contract.
    #[error("PostgreSQL schema version mismatch: expected {expected}, found {actual}")]
    SchemaVersionMismatch {
        /// Version understood by this binary.
        expected: i16,
        /// Version persisted in the database.
        actual: i16,
    },
    /// Persisted topology or network vocabulary is unknown to this binary.
    #[error("PostgreSQL database identity contains unsupported {field} value {persisted_value}")]
    UnsupportedIdentityValue {
        /// Identity field being decoded.
        field: &'static str,
        /// Persisted value.
        persisted_value: String,
    },
}

/// Applies the exact current `PostgreSQL` schema in one transaction.
pub async fn migrate_database(config: &DatabaseConfig) -> Result<MigrationOutcome, MigrationError> {
    let mut connection = DatabaseConnection::connect(config).await?;
    let migration_role_admitted = connection
        .client
        .query_one(
            "SELECT has_database_privilege(current_user, current_database(), 'CREATE')",
            &[],
        )
        .await
        .map_err(|source| DatabaseError::operation("migration role admission", source))?
        .try_get::<_, bool>(0)
        .map_err(|source| DatabaseError::operation("migration role admission decode", source))?;
    if !migration_role_admitted {
        return Err(MigrationError::MigrationRoleRejected);
    }
    let writer_role_exists = connection
        .client
        .query_one(
            "SELECT EXISTS (SELECT FROM pg_roles WHERE rolname = $1)",
            &[&CANONICAL_WRITER_ROLE],
        )
        .await
        .map_err(|source| DatabaseError::operation("runtime role admission", source))?
        .try_get::<_, bool>(0)
        .map_err(|source| DatabaseError::operation("runtime role admission decode", source))?;
    if !writer_role_exists {
        return Err(MigrationError::RequiredRoleAbsent {
            role: CANONICAL_WRITER_ROLE,
        });
    }
    let transaction = connection
        .client
        .transaction()
        .await
        .map_err(|source| DatabaseError::operation("migration transaction start", source))?;
    transaction
        .execute("SELECT pg_advisory_xact_lock($1)", &[&migration_lock_key()])
        .await
        .map_err(|source| DatabaseError::operation("migration lock", source))?;
    transaction
        .batch_execute(MIGRATION_SQL)
        .await
        .map_err(|source| DatabaseError::operation("schema migration", source))?;
    let inserted_identity = transaction
        .execute(
            INSERT_DATABASE_STATE_SQL,
            &[
                &DATABASE_IDENTITY,
                &DeploymentTopology::PostgresHorizontal.as_str(),
                &network_name(config.network),
                &DATABASE_SCHEMA_VERSION,
            ],
        )
        .await
        .map_err(|source| DatabaseError::operation("database identity initialization", source))?;
    let state = read_database_state(&transaction).await?;
    validate_database_state(&state, config.network)?;
    transaction
        .commit()
        .await
        .map_err(|source| DatabaseError::operation("migration transaction commit", source))?;
    connection.close().await?;
    Ok(if inserted_identity == 1 {
        MigrationOutcome::Applied
    } else {
        MigrationOutcome::AlreadyCurrent
    })
}

async fn read_database_state(
    client: &(impl tokio_postgres::GenericClient + Sync),
) -> Result<DatabaseState, MigrationError> {
    let row = client
        .query_opt(READ_DATABASE_STATE_SQL, &[])
        .await
        .map_err(|source| DatabaseError::operation("database identity read", source))?
        .ok_or(MigrationError::DatabaseIdentityAbsent)?;
    let product_identity = row
        .try_get::<_, String>(0)
        .map_err(|source| DatabaseError::operation("product identity decode", source))?;
    let deployment_topology = row
        .try_get::<_, String>(1)
        .map_err(|source| DatabaseError::operation("deployment topology decode", source))?;
    let network = row
        .try_get::<_, String>(2)
        .map_err(|source| DatabaseError::operation("network identity decode", source))?;
    let schema_version = row
        .try_get::<_, i16>(3)
        .map_err(|source| DatabaseError::operation("schema version decode", source))?;
    Ok(DatabaseState {
        product_identity,
        deployment_topology: parse_topology(&deployment_topology)?,
        network: decode_zinder_native_chain_name(&network).map_err(|_| {
            MigrationError::UnsupportedIdentityValue {
                field: "network",
                persisted_value: network,
            }
        })?,
        schema_version,
    })
}

fn validate_database_state(
    state: &DatabaseState,
    expected_network: Network,
) -> Result<(), MigrationError> {
    if state.product_identity != DATABASE_IDENTITY
        || state.deployment_topology != DeploymentTopology::PostgresHorizontal
    {
        return Err(MigrationError::DatabaseIdentityMismatch {
            product_identity: state.product_identity.clone(),
            deployment_topology: state.deployment_topology.as_str().to_owned(),
        });
    }
    if state.network != expected_network {
        return Err(MigrationError::NetworkMismatch {
            expected: network_name(expected_network),
            actual: network_name(state.network),
        });
    }
    if state.schema_version != DATABASE_SCHEMA_VERSION {
        return Err(MigrationError::SchemaVersionMismatch {
            expected: DATABASE_SCHEMA_VERSION,
            actual: state.schema_version,
        });
    }
    Ok(())
}

fn parse_topology(encoded_topology: &str) -> Result<DeploymentTopology, MigrationError> {
    DeploymentTopology::parse_config_name(encoded_topology).ok_or_else(|| {
        MigrationError::UnsupportedIdentityValue {
            field: "deployment topology",
            persisted_value: encoded_topology.to_owned(),
        }
    })
}

fn network_name(network: Network) -> String {
    zinder_core::wire::encode_zinder_native_chain_name(network).to_owned()
}

const fn migration_lock_key() -> i64 {
    i64::from_be_bytes(*b"ZINDMIGR")
}
