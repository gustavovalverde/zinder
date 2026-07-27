use std::{env, fs, path::PathBuf};

use eyre::{Result, eyre};
use tempfile::{TempDir, tempdir};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls};
use zinder_core::{BlockHeight, BlockId, Network, decode_canonical_block_replay};
use zinder_postgres::{
    CanonicalAppend, CanonicalAppendOutcome, CanonicalPersistenceError, CanonicalStore,
    DatabaseConfig, DatabaseError, DatabaseState, MigrationError, MigrationOutcome,
    migrate_database,
};
use zinder_runtime::PostgresTlsPolicy;
use zinder_testkit::ChainFixture;

#[tokio::test]
#[ignore = "PostgreSQL integration test; set the migration and writer database URL variables"]
async fn migration_is_idempotent_and_binds_database_identity() -> Result<()> {
    let database = TestDatabase::from_environment()?;
    assert_migration_and_identity(&database).await?;

    let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let (mut store, expected_tip) =
        assert_atomic_rollback_and_first_commit(&database, &fixture).await?;
    assert_stale_predecessor(&mut store, &fixture).await?;
    assert_competing_commit(&database.writer_config, &mut store, &fixture, expected_tip).await?;
    store.close().await?;
    Ok(())
}

struct TestDatabase {
    _secret_directory: TempDir,
    migration_database_url: String,
    migration_config: DatabaseConfig,
    writer_config: DatabaseConfig,
    writer_uri_path: PathBuf,
    tls_policy: PostgresTlsPolicy,
}

impl TestDatabase {
    fn from_environment() -> Result<Self> {
        let migration_database_url = env::var("ZINDER_TEST_POSTGRES_DATABASE_URL")
            .map_err(|_| eyre!("ZINDER_TEST_POSTGRES_DATABASE_URL is required"))?;
        let writer_database_url = env::var("ZINDER_TEST_POSTGRES_WRITER_DATABASE_URL")
            .map_err(|_| eyre!("ZINDER_TEST_POSTGRES_WRITER_DATABASE_URL is required"))?;
        let secret_directory = tempdir()?;
        let migration_uri_path = secret_directory.path().join("migration-database-uri");
        let writer_uri_path = secret_directory.path().join("writer-database-uri");
        fs::write(&migration_uri_path, &migration_database_url)?;
        fs::write(&writer_uri_path, writer_database_url)?;
        let tls_policy = env::var_os("ZINDER_TEST_POSTGRES_TLS_ROOT_CERTIFICATE_PATH").map_or(
            PostgresTlsPolicy::LoopbackPlaintext,
            |root_certificate_path| PostgresTlsPolicy::VerifyFull {
                root_certificate_path: PathBuf::from(root_certificate_path),
            },
        );
        Ok(Self {
            _secret_directory: secret_directory,
            migration_database_url,
            migration_config: DatabaseConfig::new(
                migration_uri_path,
                tls_policy.clone(),
                Network::ZcashRegtest,
                "zinder-postgres-test-migrate",
            ),
            writer_config: DatabaseConfig::new(
                writer_uri_path.clone(),
                tls_policy.clone(),
                Network::ZcashRegtest,
                "zinder-postgres-test-writer",
            ),
            writer_uri_path,
            tls_policy,
        })
    }
}

async fn assert_migration_and_identity(database: &TestDatabase) -> Result<()> {
    let first = migrate_database(&database.migration_config).await?;
    let second = migrate_database(&database.migration_config).await?;

    assert_eq!(first, MigrationOutcome::Applied);
    assert_eq!(second, MigrationOutcome::AlreadyCurrent);
    assert_eq!(
        DatabaseState::read(&database.writer_config).await?,
        DatabaseState::current(Network::ZcashRegtest)
    );

    let wrong_network = DatabaseConfig::new(
        database.writer_uri_path.clone(),
        database.tls_policy.clone(),
        Network::ZcashTestnet,
        "zinder-postgres-test-wrong-network",
    );
    assert!(matches!(
        DatabaseState::read(&wrong_network).await,
        Err(MigrationError::NetworkMismatch { .. })
    ));
    assert!(matches!(
        CanonicalStore::open(&database.migration_config).await,
        Err(CanonicalPersistenceError::WriterRoleRejected)
    ));
    Ok(())
}

async fn assert_atomic_rollback_and_first_commit(
    database: &TestDatabase,
    fixture: &ChainFixture,
) -> Result<(CanonicalStore, BlockId)> {
    let first_block = fixture
        .blocks()
        .first()
        .ok_or_else(|| eyre!("fixture first block is absent"))?;
    let second_block = fixture
        .blocks()
        .get(1)
        .ok_or_else(|| eyre!("fixture second block is absent"))?;
    let expected_predecessor = BlockId::new(first_block.height, first_block.hash);
    let expected_tip = BlockId::new(second_block.height, second_block.hash);
    let (migration_client, migration_connection) =
        connect_migration_role(&database.migration_database_url).await?;
    migration_client
        .batch_execute("REVOKE UPDATE ON canonical.writer_fence FROM zinder_ingest")
        .await?;
    assert!(matches!(
        CanonicalStore::open(&database.writer_config).await,
        Err(CanonicalPersistenceError::WriterRoleRejected)
    ));
    migration_client
        .batch_execute("GRANT UPDATE ON canonical.writer_fence TO zinder_ingest")
        .await?;
    migration_client
        .batch_execute("REVOKE USAGE ON SCHEMA canonical FROM zinder_ingest")
        .await?;
    assert!(matches!(
        CanonicalStore::open(&database.writer_config).await,
        Err(CanonicalPersistenceError::WriterRoleRejected)
    ));
    migration_client
        .batch_execute("GRANT USAGE ON SCHEMA canonical TO zinder_ingest")
        .await?;
    migration_client
        .batch_execute("REVOKE USAGE ON SCHEMA zinder_metadata FROM zinder_ingest")
        .await?;
    assert!(matches!(
        CanonicalStore::open(&database.writer_config).await,
        Err(CanonicalPersistenceError::WriterRoleRejected)
    ));
    migration_client
        .batch_execute("GRANT USAGE ON SCHEMA zinder_metadata TO zinder_ingest")
        .await?;
    let mut store = CanonicalStore::open(&database.writer_config).await?;
    migration_client
        .batch_execute("REVOKE INSERT ON canonical.control FROM zinder_ingest")
        .await?;
    let denied_commit = store
        .commit_append(fixture_append(fixture, 1, expected_predecessor)?)
        .await;
    migration_client
        .batch_execute("GRANT INSERT ON canonical.control TO zinder_ingest")
        .await?;
    assert!(matches!(
        denied_commit,
        Err(CanonicalPersistenceError::Database(
            DatabaseError::Operation { ref sqlstate, .. }
        )) if sqlstate == "42501"
    ));
    assert_transaction_rollback(&migration_client).await?;

    let committed = store
        .commit_append(fixture_append(fixture, 1, expected_predecessor)?)
        .await?;
    assert!(matches!(
        committed,
        CanonicalAppendOutcome::Committed(state) if state.visible_tip() == expected_tip
    ));
    let replayed = store
        .commit_append(fixture_append(fixture, 1, expected_predecessor)?)
        .await?;
    assert!(matches!(
        replayed,
        CanonicalAppendOutcome::AlreadyCommitted(state) if state.visible_tip() == expected_tip
    ));
    drop(migration_client);
    migration_connection.await??;
    Ok((store, expected_tip))
}

async fn assert_stale_predecessor(
    store: &mut CanonicalStore,
    fixture: &ChainFixture,
) -> Result<()> {
    let stale_predecessor = BlockId::new(
        BlockHeight::new(0),
        fixture
            .blocks()
            .first()
            .ok_or_else(|| eyre!("fixture first block is absent"))?
            .parent_hash,
    );
    assert!(matches!(
        store
            .commit_append(fixture_append(fixture, 0, stale_predecessor)?)
            .await,
        Err(CanonicalPersistenceError::StalePredecessor)
    ));
    Ok(())
}

async fn assert_competing_commit(
    writer_config: &DatabaseConfig,
    store: &mut CanonicalStore,
    fixture: &ChainFixture,
    expected_predecessor: BlockId,
) -> Result<()> {
    let third_block = fixture
        .blocks()
        .get(2)
        .ok_or_else(|| eyre!("fixture third block is absent"))?;
    let third_tip = BlockId::new(third_block.height, third_block.hash);
    let mut competing_store = CanonicalStore::open(writer_config).await?;
    let (first_writer, second_writer) = tokio::join!(
        store.commit_append(fixture_append(fixture, 2, expected_predecessor)?),
        competing_store.commit_append(fixture_append(fixture, 2, expected_predecessor)?),
    );
    let outcomes = [first_writer?, second_writer?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CanonicalAppendOutcome::Committed(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CanonicalAppendOutcome::AlreadyCommitted(_)))
            .count(),
        1
    );
    assert!(outcomes.iter().all(|outcome| match outcome {
        CanonicalAppendOutcome::Committed(state)
        | CanonicalAppendOutcome::AlreadyCommitted(state) =>
            state.visible_tip() == third_tip
                && state.visible_epoch_id() == 2
                && state.event_sequence() == 2,
    }));
    competing_store.close().await?;
    Ok(())
}

async fn connect_migration_role(
    database_url: &str,
) -> Result<(Client, JoinHandle<Result<(), tokio_postgres::Error>>)> {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;
    Ok((client, tokio::spawn(connection)))
}

async fn assert_transaction_rollback(client: &Client) -> Result<()> {
    let row = client
        .query_one(
            r"
SELECT
    (SELECT count(*) FROM canonical.block_facts),
    (SELECT count(*) FROM canonical.chain_epochs),
    (SELECT count(*) FROM canonical.chain_events),
    (SELECT count(*) FROM canonical.control),
    (SELECT writer_term FROM canonical.writer_fence WHERE singleton = TRUE)
",
            &[],
        )
        .await?;
    for column in 0..5 {
        assert_eq!(row.try_get::<_, i64>(column)?, 0);
    }
    Ok(())
}

fn fixture_append(
    fixture: &ChainFixture,
    block_index: usize,
    expected_predecessor: BlockId,
) -> Result<CanonicalAppend> {
    let envelope = fixture
        .block_replay_envelopes()
        .into_iter()
        .nth(block_index)
        .ok_or_else(|| eyre!("fixture replay envelope {block_index} is absent"))?;
    let facts = decode_canonical_block_replay(envelope.as_bytes())?.into_facts();
    Ok(CanonicalAppend::new(expected_predecessor, facts, envelope))
}
