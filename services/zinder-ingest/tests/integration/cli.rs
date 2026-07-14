#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{error::Error, fs, path::Path, process::Command};

use tempfile::tempdir;
use zinder_core::{ChainEpochId, Network};
use zinder_derive::{BLOCK_SUMMARY_CONSUMER_NAME, DeriveStore, DeriveStoreOptions};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::ChainFixture;

#[test]
fn print_config_validates_and_redacts_basic_auth() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("print-config-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--json-rpc-addr",
            "http://127.0.0.1:40000",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[node.auth]"));
    assert!(stdout.contains("method = \"basic\""));
    assert!(stdout.contains("password = \"[REDACTED]\""));
    assert!(stdout.contains("json_rpc_addr = \"http://127.0.0.1:40000\""));
    assert!(!stdout.contains("file-secret"));

    Ok(())
}

#[test]
fn print_config_accepts_zebra_cookie_auth() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("cookie-auth-store");
    let cookie_path = tempdir.path().join("zebra-cookie");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        cookie_ingest_config_toml(&storage_path, &cookie_path)?,
    )?;
    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[node.auth]"));
    assert!(stdout.contains("method = \"cookie\""));
    assert!(stdout.contains("path = \"[REDACTED]\""));
    assert!(!stdout.contains("zebra-cookie"));

    Ok(())
}

#[test]
fn print_config_renders_ingest_sub_sections() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("ingest-print-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[ingest]"));
    assert!(stdout.contains("reorg_window_blocks = 100"));
    assert!(stdout.contains("[ingest.phases]"));
    assert!(stdout.contains("catchup_threshold_blocks ="));
    assert!(stdout.contains("[ingest.derive]"));
    assert!(stdout.contains("replay_batch_blocks = 100"));
    assert!(stdout.contains("replay_policy = \"canonical-first\""));
    assert!(stdout.contains("memory_degrade_ratio = 0.9"));
    assert!(stdout.contains("memory_pause_ratio = 0.99"));
    assert!(stdout.contains("memory_resume_ratio = 0.8"));
    assert!(stdout.contains("min_replay_batch_blocks = 10"));
    assert!(stdout.contains("[ingest.commitment_root_backfill]"));
    assert!(stdout.contains("enabled = true"));
    assert!(stdout.contains("batch_blocks = 256"));
    assert!(stdout.contains("fetch_concurrency = 8"));
    assert!(stdout.contains("[ingest.conventional_fee_distribution_backfill]"));
    assert!(stdout.contains("[ingest.transaction_component_backfill]"));
    assert!(stdout.contains("[ingest.bulk_catchup]"));
    assert!(stdout.contains("canonical_batch_max_blocks = 1000"));
    assert!(stdout.contains("canonical_batch_max_artifact_bytes = 536870912"));
    assert!(stdout.contains("canonical_batch_max_estimated_write_bytes = 536870912"));
    assert!(stdout.contains("canonical_batch_min_blocks_before_estimated_write_close = 100"));
    assert!(stdout.contains("source_segment_max_blocks = 64"));
    assert!(stdout.contains("source_segment_target_response_bytes = 33554432"));
    assert!(stdout.contains("source_fetch_max_in_flight_requests = 12"));
    assert!(stdout.contains("source_fetch_max_in_flight_bytes = 402653184"));
    assert!(stdout.contains("block_prepare_concurrency ="));
    assert!(stdout.contains("block_prepare_max_in_flight_artifact_bytes = 536870912"));
    assert!(stdout.contains("commit_reassembly_max_queued_artifact_bytes = 536870912"));
    assert!(stdout.contains("[ingest.tip_follow]"));
    assert!(stdout.contains("poll_interval_ms = 1000"));
    assert!(stdout.contains("lag_threshold_blocks ="));
    assert!(stdout.contains("[ingest.modifiers]"));
    assert!(stdout.contains("allow_near_tip_finalize = false"));
    assert!(stdout.contains("coverage = \"explicit\""));

    Ok(())
}

#[test]
fn commitment_root_backfill_env_overrides_print_config() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("root-backfill-env-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_INGEST__COMMITMENT_ROOT_BACKFILL__ENABLED", "false")
        .env(
            "ZINDER_INGEST__COMMITMENT_ROOT_BACKFILL__BATCH_BLOCKS",
            "64",
        )
        .env(
            "ZINDER_INGEST__COMMITMENT_ROOT_BACKFILL__FETCH_CONCURRENCY",
            "2",
        )
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let section = stdout
        .split("[ingest.commitment_root_backfill]")
        .nth(1)
        .and_then(|tail| tail.split("\n[").next())
        .ok_or("commitment-root backfill section missing")?;
    assert!(section.contains("enabled = false"));
    assert!(section.contains("batch_blocks = 64"));
    assert!(section.contains("fetch_concurrency = 2"));
    Ok(())
}

#[test]
fn conventional_fee_distribution_backfill_env_overrides_print_config() -> Result<(), Box<dyn Error>>
{
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("fee-distribution-backfill-env-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env(
            "ZINDER_INGEST__CONVENTIONAL_FEE_DISTRIBUTION_BACKFILL__ENABLED",
            "false",
        )
        .env(
            "ZINDER_INGEST__CONVENTIONAL_FEE_DISTRIBUTION_BACKFILL__BATCH_BLOCKS",
            "64",
        )
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let section = stdout
        .split("[ingest.conventional_fee_distribution_backfill]")
        .nth(1)
        .and_then(|tail| tail.split("\n[").next())
        .ok_or("conventional-fee distribution backfill section missing")?;
    assert!(section.contains("enabled = false"));
    assert!(section.contains("batch_blocks = 64"));
    Ok(())
}

#[test]
fn print_config_allows_disabled_ingest_control_for_one_shot_runs() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("disabled-ingest-control-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let config = ingest_config_toml(&storage_path)?
        .replace("listen_addr = \"127.0.0.1:9100\"", "listen_addr = \"\"");
    fs::write(&config_path, config)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[ingest_control]"));
    assert!(stdout.contains("listen_addr = \"\""));

    Ok(())
}

#[test]
fn backup_print_config_loads_config_file() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("backup-print-config-store");
    let backup_path = tempdir.path().join("backup-print-config-checkpoint");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        backup_config_toml(&storage_path, &backup_path)?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "backup",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("[backup]"));
    assert!(stdout.contains(&format!("to_path = \"{}\"", path_str(&backup_path)?)));

    Ok(())
}

#[test]
fn backup_creates_checkpoint_from_primary_store() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("backup-source-store");
    let checkpoint_path = tempdir.path().join("backup-checkpoint");
    let derive_checkpoint_staging_path = checkpoint_path.with_extension("derive");
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let artifacts = chain_fixture
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or("chain fixture unexpectedly empty")?;
    let expected_chain_epoch = artifacts.chain_epoch;
    let expected_derive_cursor = b"derive-cursor";

    {
        let store = PrimaryChainStore::open(
            &storage_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        store.commit_chain_epoch(artifacts)?;
        let derive_store = DeriveStore::open(
            DeriveStore::path_for_canonical(&storage_path),
            DeriveStoreOptions {
                consumers: DeriveStore::bundled_consumers(),
                ..DeriveStoreOptions::default()
            },
        )?;
        derive_store.put_chain_event_cursor(BLOCK_SUMMARY_CONSUMER_NAME, expected_derive_cursor)?;
    }

    let output = zinder_ingest_command()
        .args([
            "backup",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
            "--to",
            path_str(&checkpoint_path)?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let checkpoint = PrimaryChainStore::open(
        &checkpoint_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    assert_eq!(
        checkpoint.current_chain_epoch()?,
        Some(expected_chain_epoch)
    );
    let derive_checkpoint = DeriveStore::open(
        DeriveStore::path_for_canonical(&checkpoint_path),
        DeriveStoreOptions {
            consumers: DeriveStore::bundled_consumers(),
            ..DeriveStoreOptions::default()
        },
    )?;
    assert_eq!(
        derive_checkpoint.get_chain_event_cursor(BLOCK_SUMMARY_CONSUMER_NAME)?,
        Some(expected_derive_cursor.to_vec())
    );
    assert!(!derive_checkpoint_staging_path.exists());

    Ok(())
}

#[test]
fn print_config_shows_explicit_near_tip_finalize_override() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("allow-near-tip-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--allow-near-tip-finalize",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("allow_near_tip_finalize = true"));

    Ok(())
}

#[test]
fn cli_overrides_environment_and_environment_overrides_config_file() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("precedence-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--target-height",
            "300",
        ])
        .env("ZINDER_INGEST__MODIFIERS__TARGET_HEIGHT", "200")
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    // CLI override (300) wins over env (200) which would win over file.
    assert!(stdout.contains("target_height = 300"), "{stdout}");

    Ok(())
}

#[test]
fn target_height_uses_standard_config_precedence() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("target-height-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_INGEST__MODIFIERS__TARGET_HEIGHT", "777")
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("target_height = 777"), "{stdout}");

    Ok(())
}

#[test]
fn wallet_serving_print_config_marks_coverage() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        wallet_serving_ingest_config_toml(&storage_path)?,
    )?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("coverage = \"wallet-serving\""), "{stdout}");
    assert!(
        stdout.contains("raw_blob_policy = \"transactions\""),
        "{stdout}"
    );

    Ok(())
}

#[test]
fn wallet_serving_rejects_no_transaction_blob_retention() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let config = wallet_serving_ingest_config_toml(&storage_path)?.replacen(
        "[storage]\n",
        "[storage]\nraw_blob_policy = \"none\"\n",
        1,
    );
    fs::write(&config_path, config)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "ingest.modifiers.coverage = \"wallet-serving\" requires storage.raw_blob_policy = \"transactions\" or \"all\""
        ),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn wallet_serving_preserves_full_block_blob_retention() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let config = wallet_serving_ingest_config_toml(&storage_path)?.replacen(
        "[storage]\n",
        "[storage]\nraw_blob_policy = \"all\"\n",
        1,
    );
    fs::write(&config_path, config)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("raw_blob_policy = \"all\""), "{stdout}");

    Ok(())
}

#[test]
fn wallet_serving_rejects_explicit_checkpoint_height() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-checkpoint-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        wallet_serving_ingest_config_toml_with_checkpoint(&storage_path)?,
    )?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "ingest.modifiers.coverage = \"wallet-serving\" derives checkpoint_height from the node"
        ),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn wallet_serving_rejects_near_tip_finalize_override() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wallet-serving-near-tip-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        wallet_serving_ingest_config_toml(&storage_path)?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--allow-near-tip-finalize",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.modifiers.coverage = \"wallet-serving\" cannot be combined with"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn max_response_bytes_can_be_overridden_from_cli() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("max-response-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--max-response-bytes",
            "8388608",
            "--source-segment-target-response-bytes",
            "8388608",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("max_response_bytes = 8388608"), "{stdout}");
    assert!(
        stdout.contains("source_segment_target_response_bytes = 8388608"),
        "{stdout}"
    );

    Ok(())
}

#[test]
fn print_config_redacts_password_supplied_through_environment() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("env-password-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        ingest_config_toml_without_auth(&storage_path)?,
    )?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_NODE__AUTH__METHOD", "basic")
        .env("ZINDER_NODE__AUTH__USERNAME", "zebra")
        .env("ZINDER_NODE__AUTH__PASSWORD", "env-secret")
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("password = \"[REDACTED]\""));
    assert!(!stdout.contains("env-secret"));

    Ok(())
}

#[test]
fn print_config_redacts_inline_cookie_supplied_through_environment() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("inline-cookie-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        ingest_config_toml_without_auth(&storage_path)?,
    )?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_NODE__AUTH__METHOD", "cookie")
        .env("ZINDER_NODE__AUTH__COOKIE", "zebra:inline-cookie-secret")
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("method = \"cookie\""));
    assert!(!stdout.contains("inline-cookie-secret"));

    Ok(())
}

#[test]
fn zero_commit_batch_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-commit-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--canonical-batch-max-blocks",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.bulk_catchup.canonical_batch_max_blocks must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn zero_estimated_write_batch_budget_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-estimated-write-budget-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--canonical-batch-max-estimated-write-bytes",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "ingest.bulk_catchup.canonical_batch_max_estimated_write_bytes must be greater than zero"
        ),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn estimated_write_close_floor_above_block_cap_fails_before_storage_creation()
-> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir
        .path()
        .join("estimated-write-floor-above-block-cap-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--canonical-batch-max-blocks",
            "10",
            "--canonical-batch-min-blocks-before-estimated-write-close",
            "11",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "ingest.bulk_catchup.canonical_batch_min_blocks_before_estimated_write_close must be less than or equal to ingest.bulk_catchup.canonical_batch_max_blocks"
        ),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn zero_source_segment_max_blocks_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-source-segment-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env(
            "ZINDER_INGEST__BULK_CATCHUP__SOURCE_SEGMENT_MAX_BLOCKS",
            "0",
        )
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.bulk_catchup.source_segment_max_blocks must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn zero_block_prepare_concurrency_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-derive-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--block-prepare-concurrency",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.bulk_catchup.block_prepare_concurrency must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn zero_derive_replay_batch_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-derive-replay-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_INGEST__DERIVE__REPLAY_BATCH_BLOCKS", "0")
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.derive.replay_batch_blocks must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn invalid_derive_replay_policy_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("invalid-derive-replay-policy-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_INGEST__DERIVE__REPLAY_POLICY", "best-effort")
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.derive.replay_policy must be one of: canonical-first, continuous"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn zero_max_response_bytes_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-response-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--max-response-bytes",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("node.max_response_bytes must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn source_fetch_byte_budget_below_max_response_fails_before_storage_creation()
-> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("source-fetch-budget-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--max-response-bytes",
            "67108864",
            "--source-fetch-max-in-flight-bytes",
            "33554432",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(
            "ingest.bulk_catchup.source_fetch_max_in_flight_bytes must be greater than or equal to node.max_response_bytes"
        ),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn zero_poll_interval_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zero-poll-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--poll-interval-ms",
            "0",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("ingest.tip_follow.poll_interval_ms must be greater than zero"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn storage_path_default_resolves_to_canonical_zinder_layout() -> Result<(), Box<dyn Error>> {
    // The binary's default for `storage.path` matches the canonical Zinder
    // layout under `/var/lib/zinder/store`. Operators on non-PaaS hosts
    // override via `ZINDER_STORAGE__PATH` or the `--storage-path` flag.
    // This test guards the default's stability so the env-only deployment
    // shape (single-container Docker image) keeps working without a TOML
    // file or a `--config` argument.
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
        ])
        .output()?;

    assert!(
        output.status.success(),
        "print-config failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("path = \"/var/lib/zinder/store\"")
            || stdout.contains("path = '/var/lib/zinder/store'"),
        "stdout does not carry the canonical storage.path default:\n{stdout}"
    );

    Ok(())
}

#[test]
fn unknown_network_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("unknown-network-store");
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--network",
            "zcash-mars",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("network.name") || stderr.contains("zcash-mars"),
        "{stderr}"
    );

    Ok(())
}

#[test]
fn partial_basic_auth_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("partial-auth-store");
    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--network",
            "zcash-regtest",
            "--node-source",
            "zebra-json-rpc",
            "--json-rpc-addr",
            "http://127.0.0.1:18232",
            "--storage-path",
            path_str(&storage_path)?,
            "--node-auth-method",
            "basic",
            "--node-auth-username",
            "zebra",
        ])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("missing configuration field: node.auth.password"),
        "{stderr}"
    );

    Ok(())
}

fn zinder_ingest_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-ingest"));
    command.env_clear();
    command
}

fn ingest_config_toml(storage_path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "basic"
username = "zebra"
password = "file-secret"

[storage]
path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest_control]
listen_addr = "127.0.0.1:9100"
"#,
        path_str(storage_path)?,
    ))
}

fn ingest_config_toml_without_auth(storage_path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[storage]
path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest_control]
listen_addr = "127.0.0.1:9100"
"#,
        path_str(storage_path)?,
    ))
}

fn wallet_serving_ingest_config_toml(storage_path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "basic"
username = "zebra"
password = "file-secret"

[storage]
path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest.modifiers]
coverage = "wallet-serving"

[ingest_control]
listen_addr = "127.0.0.1:9100"
"#,
        path_str(storage_path)?,
    ))
}

fn wallet_serving_ingest_config_toml_with_checkpoint(
    storage_path: &Path,
) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "basic"
username = "zebra"
password = "file-secret"

[storage]
path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest.modifiers]
coverage = "wallet-serving"
checkpoint_height = 1

[ingest_control]
listen_addr = "127.0.0.1:9100"
"#,
        path_str(storage_path)?,
    ))
}

fn cookie_ingest_config_toml(
    storage_path: &Path,
    cookie_path: &Path,
) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
json_rpc_addr = "http://127.0.0.1:39232"
request_timeout_secs = 30

[node.auth]
method = "cookie"
path = "{}"

[storage]
path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest_control]
listen_addr = "127.0.0.1:9100"
"#,
        path_str(cookie_path)?,
        path_str(storage_path)?,
    ))
}

fn backup_config_toml(storage_path: &Path, backup_path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"

[backup]
to_path = "{}"
"#,
        path_str(storage_path)?,
        path_str(backup_path)?
    ))
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}
