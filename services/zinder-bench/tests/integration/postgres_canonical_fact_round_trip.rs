#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use zinder_bench::canonical_fact_round_trip::postgres::{
    PostgresCanonicalFactRoundTripConfig, run_postgres_canonical_fact_round_trip,
    validate_postgres_canonical_fact_round_trip_with_fresh_connection,
};

use crate::common::write_regtest_fixture;

const DATABASE_URL_ENV: &str = "ZINDER_TEST_POSTGRES_DATABASE_URL";

#[tokio::test]
#[ignore = "requires a fresh disposable PostgreSQL database"]
async fn completed_round_trip_supports_a_fresh_reader_and_rejects_schema_reuse() -> Result<()> {
    let database_url = std::env::var(DATABASE_URL_ENV)
        .map_err(|_| eyre!("set {DATABASE_URL_ENV} to a fresh disposable database"))?;
    let (fixture_directory, _) = write_regtest_fixture()?;
    let concurrency = NonZeroU32::new(2).ok_or_else(|| eyre!("2 must be non-zero"))?;
    let result = run_postgres_canonical_fact_round_trip(PostgresCanonicalFactRoundTripConfig::new(
        fixture_directory.path(),
        database_url.as_str(),
        concurrency,
    ))
    .await?;

    assert!(result.validation.position.block_count > 0);
    assert!(result.validation.position.logical_fact_bytes > 0);
    assert!(result.storage.fact_table_bytes > 0);
    assert!(result.storage.wal_bytes > 0);
    let fresh_reader = validate_postgres_canonical_fact_round_trip_with_fresh_connection(
        database_url.as_str(),
        fixture_directory.path(),
    )
    .await?;
    assert_eq!(fresh_reader, result.validation);

    let Some(reuse_error) =
        run_postgres_canonical_fact_round_trip(PostgresCanonicalFactRoundTripConfig::new(
            fixture_directory.path(),
            database_url.as_str(),
            concurrency,
        ))
        .await
        .err()
    else {
        return Err(eyre!("an existing candidate schema must reject rerun"));
    };
    assert!(reuse_error.to_string().contains("schema already exists"));
    assert!(!reuse_error.to_string().contains(database_url.as_str()));

    Ok(())
}
