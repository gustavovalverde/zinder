//! Outer live acceptance contract for the unreleased `PostgreSQL` tracer.

#![allow(
    missing_docs,
    reason = "Live test names and assertions describe the external contract under test."
)]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use eyre::{Result, WrapErr, eyre};
use serde_json::Value;
use tempfile::tempdir;
use zinder_core::Network;
use zinder_testkit::live::{init, require_live_for};

use crate::common::{
    basic_auth_credentials, fetch_live_tip_height, rpc_block_hash_at_height, zinder_ingest_command,
};

const MIGRATION_DATABASE_URL_ENV: &str = "ZINDER_TEST_POSTGRES_DATABASE_URL";
const WRITER_DATABASE_URL_ENV: &str = "ZINDER_TEST_POSTGRES_WRITER_DATABASE_URL";
const TLS_ROOT_CERTIFICATE_PATH_ENV: &str = "ZINDER_TEST_POSTGRES_TLS_ROOT_CERTIFICATE_PATH";
const POSTGRES_TOPOLOGY: &str = "postgres-horizontal";
const CANONICAL_SCHEMA: &str = "canonical";

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn writer_exit_preserves_one_postgres_transition_for_fresh_probe() -> Result<()> {
    let _guard = init();
    let Some(live_env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };
    let migration_database_url = env::var(MIGRATION_DATABASE_URL_ENV)
        .map_err(|_| eyre!("set {MIGRATION_DATABASE_URL_ENV} to a fresh disposable database"))?;
    let writer_database_url = env::var(WRITER_DATABASE_URL_ENV)
        .map_err(|_| eyre!("set {WRITER_DATABASE_URL_ENV} to the writer-role database URI"))?;
    let tls_root_certificate_path = env::var_os(TLS_ROOT_CERTIFICATE_PATH_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| {
            eyre!(
                "set {TLS_ROOT_CERTIFICATE_PATH_ENV} to the PostgreSQL test server's PEM trust root"
            )
        })?;
    let migration_database_password =
        password_from_database_url(&migration_database_url, MIGRATION_DATABASE_URL_ENV)?;
    let writer_database_password =
        password_from_database_url(&writer_database_url, WRITER_DATABASE_URL_ENV)?;
    let target_height = fetch_live_tip_height(&live_env).await?;
    let checkpoint_height = predecessor_height(target_height.value())?;
    let target_hash = rpc_block_hash_at_height(&live_env, target_height.value()).await?;
    let (node_username, node_password) = basic_auth_credentials(&live_env)?;

    let temporary = tempdir()?;
    let migration_database_url_path = temporary.path().join("migration-database-url");
    let writer_database_url_path = temporary.path().join("writer-database-url");
    let migration_config_path = temporary.path().join("zinder-migrate.toml");
    let runtime_config_path = temporary.path().join("zinder-ingest.toml");
    fs::write(&migration_database_url_path, &migration_database_url)?;
    fs::write(&writer_database_url_path, &writer_database_url)?;
    fs::write(
        &migration_config_path,
        postgres_migration_config(&migration_database_url_path, &tls_root_certificate_path)?,
    )?;
    fs::write(
        &runtime_config_path,
        postgres_ingest_config(&PostgresIngestConfig {
            live_env: &live_env,
            node_username,
            node_password,
            database_url_path: &writer_database_url_path,
            tls_root_certificate_path: &tls_root_certificate_path,
            checkpoint_height,
            target_height: target_height.value(),
        })?,
    )?;
    let secrets = [
        migration_database_url.as_str(),
        writer_database_url.as_str(),
        migration_database_password,
        writer_database_password,
        path_str(&migration_database_url_path)?,
        path_str(&writer_database_url_path)?,
        node_password,
    ];

    let migration = run_process(
        zinder_migrate_command()?,
        &migration_config_path,
        "zinder-migrate",
    )?;
    assert_success_and_redaction(&migration, "zinder-migrate", &secrets)?;

    let writer = run_process(
        zinder_ingest_command(),
        &runtime_config_path,
        "zinder-ingest writer",
    )?;
    assert_success_and_redaction(&writer, "zinder-ingest writer", &secrets)?;

    let mut probe_command = zinder_ingest_command();
    probe_command.arg("probe");
    let probe = run_process(probe_command, &runtime_config_path, "zinder-ingest probe")?;
    assert_success_and_redaction(&probe, "zinder-ingest probe", &secrets)?;
    assert_probe_identity(&probe.stdout, target_height.value(), &target_hash)
}

fn predecessor_height(target_height: u32) -> Result<u32> {
    target_height
        .checked_sub(1)
        .ok_or_else(|| eyre!("PostgreSQL tracer requires a non-genesis source tip"))
}

fn zinder_migrate_command() -> Result<Command> {
    let ingest_binary = Path::new(env!("CARGO_BIN_EXE_zinder-ingest"));
    let binary_directory = ingest_binary
        .parent()
        .ok_or_else(|| eyre!("zinder-ingest binary has no parent directory"))?;
    let migrate_binary =
        binary_directory.join(format!("zinder-migrate{}", std::env::consts::EXE_SUFFIX));
    let mut command = Command::new(&migrate_binary);
    command.env_clear();
    Ok(command)
}

fn run_process(mut command: Command, config_path: &Path, process_name: &str) -> Result<Output> {
    command.args(["--config", path_str(config_path)?]);
    command
        .output()
        .wrap_err_with(|| format!("failed to start separate {process_name} process"))
}

fn assert_success_and_redaction(
    process_output: &Output,
    process_name: &str,
    secrets: &[&str],
) -> Result<()> {
    let stdout = String::from_utf8(process_output.stdout.clone())?;
    let stderr = String::from_utf8(process_output.stderr.clone())?;
    for secret in secrets {
        assert!(
            !stdout.contains(secret),
            "{process_name} stdout leaked a secret"
        );
        assert!(
            !stderr.contains(secret),
            "{process_name} stderr leaked a secret"
        );
    }
    if !process_output.status.success() {
        return Err(eyre!(
            "{process_name} failed with status {:?}; process output withheld after redaction checks",
            process_output.status.code()
        ));
    }
    Ok(())
}

fn assert_probe_identity(stdout: &[u8], target_height: u32, target_hash: &str) -> Result<()> {
    let probe: Value = serde_json::from_slice(stdout)?;
    assert_eq!(probe["deployment_topology"], POSTGRES_TOPOLOGY);
    assert_eq!(probe["network"], "zcash-regtest");
    assert_eq!(probe["canonical"]["schema"], CANONICAL_SCHEMA);
    assert!(
        probe["canonical"]["schema_version"]
            .as_u64()
            .is_some_and(|schema_version| schema_version > 0)
    );
    assert_eq!(probe["canonical"]["visible_tip"]["height"], target_height);
    assert_eq!(probe["canonical"]["visible_tip"]["block_hash"], target_hash);
    Ok(())
}

struct PostgresIngestConfig<'fields> {
    live_env: &'fields zinder_testkit::live::LiveTestEnv,
    node_username: &'fields str,
    node_password: &'fields str,
    database_url_path: &'fields Path,
    tls_root_certificate_path: &'fields Path,
    checkpoint_height: u32,
    target_height: u32,
}

fn postgres_ingest_config(config: &PostgresIngestConfig<'_>) -> Result<String> {
    Ok(format!(
        r#"[deployment]
topology = "{POSTGRES_TOPOLOGY}"

[network]
name = "zcash-regtest"

[ops]
listen_addr = "127.0.0.1:0"

[node]
json_rpc_addr = "{}"
request_timeout_secs = {}

[node.auth]
method = "basic"
username = "{}"
password = "{}"

[storage.postgres]
database_url_path = "{}"
tls = "verify-full"
tls_root_certificate_path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest.run_overrides]
checkpoint_height = {}
target_height = {}
allow_reorg_window_settlement = true

[ingest_control]
listen_addr = "127.0.0.1:0"
"#,
        config.live_env.target.json_rpc_addr,
        config.live_env.target.request_timeout.as_secs(),
        config.node_username,
        config.node_password,
        path_str(config.database_url_path)?,
        path_str(config.tls_root_certificate_path)?,
        config.checkpoint_height,
        config.target_height,
    ))
}

fn postgres_migration_config(
    database_url_path: &Path,
    tls_root_certificate_path: &Path,
) -> Result<String> {
    Ok(format!(
        r#"[deployment]
topology = "{POSTGRES_TOPOLOGY}"

[network]
name = "zcash-regtest"

[storage.postgres]
database_url_path = "{}"
tls = "verify-full"
tls_root_certificate_path = "{}"
"#,
        path_str(database_url_path)?,
        path_str(tls_root_certificate_path)?,
    ))
}

fn password_from_database_url<'url>(
    database_url: &'url str,
    environment_variable: &str,
) -> Result<&'url str> {
    let authority = database_url
        .split_once("://")
        .map_or(database_url, |(_, remainder)| remainder)
        .split_once('@')
        .map(|(userinfo, _)| userinfo)
        .ok_or_else(|| eyre!("{environment_variable} must contain userinfo"))?;
    authority
        .rsplit_once(':')
        .map(|(_, password)| password)
        .filter(|password| !password.is_empty())
        .ok_or_else(|| eyre!("{environment_variable} must contain a password"))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre!("path is not valid UTF-8: {}", path.display()))
}
