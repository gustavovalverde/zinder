#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{error::Error, fs, path::Path};

use tempfile::tempdir;
use zinder_core::{
    CanonicalBlockFactsSequenceDigestBuilder, CanonicalBlockFactsSequenceDigestVersion,
    ChainEpochId, Network, decode_canonical_block_replay,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::ChainFixture;

use crate::common::zinder_ingest_command;

#[test]
fn version_reports_the_product_version() -> Result<(), Box<dyn Error>> {
    let output = zinder_ingest_command().arg("--version").output()?;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!("zinder-ingest {}\n", env!("CARGO_PKG_VERSION"))
    );

    Ok(())
}

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
fn print_config_applies_ops_listener_override() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("ops-listener-override-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--ops-listen-addr",
            "127.0.0.1:29105",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)?.contains("listen_addr = \"127.0.0.1:29105\""));

    Ok(())
}

#[test]
fn print_config_resolves_the_materialized_view_writer_budget() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("materialized-view-budget-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("[storage.materialized_views.rocksdb]"),
        "{stdout}"
    );
    assert!(stdout.contains("block_cache_bytes = 268435456"), "{stdout}");

    Ok(())
}

#[test]
fn print_config_applies_materialized_view_budget_overrides() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("materialized-view-override-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let config = format!(
        "{}\n[storage.materialized_views.rocksdb]\nmax_open_files = 96\n",
        ingest_config_toml(&storage_path)?
    );
    fs::write(&config_path, config)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("max_open_files = 96"), "{stdout}");

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
    assert!(stdout.contains("[ingest.phase_classification]"));
    assert!(stdout.contains("catchup_threshold_blocks ="));
    assert!(stdout.contains("[ingest.construction]"));
    assert!(stdout.contains("canonical_batch_max_blocks = 1000"));
    assert!(stdout.contains("canonical_batch_max_artifact_bytes = 536870912"));
    assert!(stdout.contains("canonical_batch_max_estimated_write_bytes = 536870912"));
    assert!(stdout.contains("canonical_batch_min_blocks_before_estimated_write_close = 100"));
    assert!(stdout.contains("source_segment_max_blocks = 64"));
    assert!(stdout.contains("source_segment_target_response_bytes = 33554432"));
    assert!(stdout.contains("source_fetch_max_in_flight_requests = 12"));
    assert!(stdout.contains("source_fetch_max_in_flight_bytes = 402653184"));
    assert!(stdout.contains("block_prepare_concurrency ="));
    assert!(stdout.contains("block_prepare_memory_watermark_bytes = 536870912"));
    assert!(stdout.contains("commit_reassembly_max_queued_artifact_bytes = 536870912"));
    assert!(stdout.contains("[ingest.mempool]"));
    assert!(stdout.contains("max_transaction_count = 8000"));
    assert!(stdout.contains("max_total_raw_transaction_bytes = 80000000"));
    assert!(stdout.contains("reconciliation_batch_target_raw_transaction_bytes = 16000000"));
    assert!(stdout.contains("[ingest.follow]"));
    assert!(stdout.contains("poll_interval_ms = 1000"));
    assert!(stdout.contains("lag_threshold_blocks ="));
    assert!(stdout.contains("[ingest.run_overrides]"));
    assert!(stdout.contains("allow_reorg_window_settlement = false"));
    assert!(stdout.contains("coverage = \"explicit\""));

    Ok(())
}

#[test]
fn mempool_limits_use_environment_overrides() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("mempool-limit-overrides-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .env("ZINDER_INGEST__MEMPOOL__MAX_TRANSACTION_COUNT", "7000")
        .env(
            "ZINDER_INGEST__MEMPOOL__MAX_TOTAL_RAW_TRANSACTION_BYTES",
            "70000000",
        )
        .env(
            "ZINDER_INGEST__MEMPOOL__RECONCILIATION_BATCH_TARGET_RAW_TRANSACTION_BYTES",
            "7000000",
        )
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("max_transaction_count = 7000"), "{stdout}");
    assert!(
        stdout.contains("max_total_raw_transaction_bytes = 70000000"),
        "{stdout}"
    );
    assert!(
        stdout.contains("reconciliation_batch_target_raw_transaction_bytes = 7000000"),
        "{stdout}"
    );

    Ok(())
}

#[test]
fn zero_mempool_limits_fail_before_storage_creation() -> Result<(), Box<dyn Error>> {
    let cases = [
        (
            "ZINDER_INGEST__MEMPOOL__MAX_TRANSACTION_COUNT",
            "ingest.mempool.max_transaction_count must be greater than zero",
        ),
        (
            "ZINDER_INGEST__MEMPOOL__MAX_TOTAL_RAW_TRANSACTION_BYTES",
            "ingest.mempool.max_total_raw_transaction_bytes must be greater than zero",
        ),
        (
            "ZINDER_INGEST__MEMPOOL__RECONCILIATION_BATCH_TARGET_RAW_TRANSACTION_BYTES",
            "ingest.mempool.reconciliation_batch_target_raw_transaction_bytes must be greater than zero",
        ),
    ];

    for (environment_variable, expected_error) in cases {
        let tempdir = tempdir()?;
        let storage_path = tempdir.path().join("zero-mempool-limit-store");
        let config_path = tempdir.path().join("zinder-ingest.toml");
        fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

        let output = zinder_ingest_command()
            .args(["--print-config", "--config", path_str(&config_path)?])
            .env(environment_variable, "0")
            .output()?;

        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains(expected_error), "{stderr}");
        assert!(!storage_path.exists());
    }

    Ok(())
}

#[test]
fn print_config_output_round_trips() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("round-trip-store");
    let source_config_path = tempdir.path().join("source.toml");
    let rendered_config_path = tempdir.path().join("rendered.toml");
    fs::write(&source_config_path, ingest_config_toml(&storage_path)?)?;

    let first_output = zinder_ingest_command()
        .args(["--print-config", "--config", path_str(&source_config_path)?])
        .output()?;
    assert!(first_output.status.success(), "{first_output:?}");
    fs::write(&rendered_config_path, &first_output.stdout)?;

    let second_output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&rendered_config_path)?,
        ])
        .output()?;
    assert!(second_output.status.success(), "{second_output:?}");

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
fn canonical_replay_verification_print_config_loads_secondary_path() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("verification-print-config-store");
    let secondary_path = tempdir.path().join("verification-print-config-secondary");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        canonical_replay_verification_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "verify-canonical-replay",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(&format!("path = \"{}\"", path_str(&storage_path)?)));
    assert!(stdout.contains(&format!(
        "secondary_path = \"{}\"",
        path_str(&secondary_path)?
    )));
    assert!(!stdout.contains("raw_blob_policy"));

    Ok(())
}

#[test]
fn canonical_replay_verification_scans_multiple_bounded_batches() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("verification-source-store");
    let secondary_path = tempdir.path().join("verification-secondary");
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(257);
    let artifacts = chain_fixture
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or("chain fixture unexpectedly empty")?;
    let expected_chain_epoch = artifacts.chain_epoch;
    let mut expected_digest_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
        CanonicalBlockFactsSequenceDigestVersion::CURRENT,
    );
    for replay_envelope in &artifacts.block_replay_envelopes {
        expected_digest_builder.try_append(
            decode_canonical_block_replay(replay_envelope.as_bytes())?.reference_digest(),
        )?;
    }
    let expected_digest = expected_digest_builder.finish();
    let primary = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    primary.commit_chain_epoch(artifacts)?;

    let output = zinder_ingest_command()
        .args([
            "verify-canonical-replay",
            "--network",
            "zcash-regtest",
            "--storage-path",
            path_str(&storage_path)?,
            "--secondary-path",
            path_str(&secondary_path)?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        report["verification_scope"],
        "replay_envelope_and_canonical_header_parity"
    );
    assert_eq!(report["network"], "zcash-regtest");
    assert_eq!(report["chain_epoch_id"], expected_chain_epoch.id.value());
    assert_eq!(report["from_height"], 1);
    assert_eq!(report["to_height"], 257);
    assert_eq!(report["block_count"], 257);
    assert_eq!(report["sequence_digest_version"], 1);
    assert_eq!(
        report["sequence_digest_sha256"],
        hex::encode(expected_digest.as_bytes())
    );
    assert!(secondary_path.exists());

    Ok(())
}

#[test]
fn print_config_shows_explicit_reorg_window_settlement_override() -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("allow-reorg-window-settlement-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, ingest_config_toml(&storage_path)?)?;

    let output = zinder_ingest_command()
        .args([
            "--print-config",
            "--config",
            path_str(&config_path)?,
            "--allow-reorg-window-settlement",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("allow_reorg_window_settlement = true"));

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
        .env("ZINDER_INGEST__RUN_OVERRIDES__TARGET_HEIGHT", "200")
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
        .env("ZINDER_INGEST__RUN_OVERRIDES__TARGET_HEIGHT", "777")
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
    assert_ingest_cli_rejects(
        |root| {
            Ok(
                wallet_serving_ingest_config_toml(&root.join("wallet-serving-store"))?.replacen(
                    "[storage]\n",
                    "[storage]\nraw_blob_policy = \"none\"\n",
                    1,
                ),
            )
        },
        &[],
        &[],
        "ingest.run_overrides.coverage = \"wallet-serving\" requires storage.raw_blob_policy = \"transactions\" or \"all\"",
    )
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
    assert_ingest_cli_rejects(
        |root| {
            wallet_serving_ingest_config_toml_with_checkpoint(
                &root.join("wallet-serving-checkpoint-store"),
            )
        },
        &[],
        &[],
        "ingest.run_overrides.coverage = \"wallet-serving\" requires complete transparent history and sets checkpoint_height to zero",
    )
}

#[test]
fn wallet_serving_rejects_reorg_window_settlement_override() -> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| {
            wallet_serving_ingest_config_toml(
                &root.join("wallet-serving-reorg-window-settlement-store"),
            )
        },
        &["--allow-reorg-window-settlement"],
        &[],
        "ingest.run_overrides.coverage = \"wallet-serving\" cannot be combined with",
    )
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
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("zero-commit-store")),
        &["--canonical-batch-max-blocks", "0"],
        &[],
        "ingest.construction.canonical_batch_max_blocks must be greater than zero",
    )
}

#[test]
fn zero_estimated_write_batch_budget_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("zero-estimated-write-budget-store")),
        &["--canonical-batch-max-estimated-write-bytes", "0"],
        &[],
        "ingest.construction.canonical_batch_max_estimated_write_bytes must be greater than zero",
    )
}

#[test]
fn estimated_write_close_floor_above_block_cap_fails_before_storage_creation()
-> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("estimated-write-floor-above-block-cap-store")),
        &[
            "--canonical-batch-max-blocks",
            "10",
            "--canonical-batch-min-blocks-before-estimated-write-close",
            "11",
        ],
        &[],
        "ingest.construction.canonical_batch_min_blocks_before_estimated_write_close must be less than or equal to ingest.construction.canonical_batch_max_blocks",
    )
}

#[test]
fn zero_source_segment_max_blocks_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("zero-source-segment-store")),
        &[],
        &[(
            "ZINDER_INGEST__CONSTRUCTION__SOURCE_SEGMENT_MAX_BLOCKS",
            "0",
        )],
        "ingest.construction.source_segment_max_blocks must be greater than zero",
    )
}

#[test]
fn zero_block_prepare_concurrency_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("zero-block-prepare")),
        &["--block-prepare-concurrency", "0"],
        &[],
        "ingest.construction.block_prepare_concurrency must be greater than zero",
    )
}

#[test]
fn zero_max_response_bytes_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("zero-response-store")),
        &["--max-response-bytes", "0"],
        &[],
        "node.max_response_bytes must be greater than zero",
    )
}

#[test]
fn source_fetch_byte_budget_below_max_response_fails_before_storage_creation()
-> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("source-fetch-budget-store")),
        &[
            "--max-response-bytes",
            "67108864",
            "--source-fetch-max-in-flight-bytes",
            "33554432",
        ],
        &[],
        "invalid ingest.construction pipeline limits: source fetch watermark 33554432 is below maximum response 67108864",
    )
}

#[test]
fn zero_poll_interval_fails_before_storage_creation() -> Result<(), Box<dyn Error>> {
    assert_ingest_cli_rejects(
        |root| ingest_config_toml(&root.join("zero-poll-store")),
        &["--poll-interval-ms", "0"],
        &[],
        "ingest.follow.poll_interval_ms must be greater than zero",
    )
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

fn assert_ingest_cli_rejects(
    build_config_toml: impl FnOnce(&Path) -> Result<String, Box<dyn Error>>,
    args: &[&str],
    environment: &[(&str, &str)],
    expected_error: &str,
) -> Result<(), Box<dyn Error>> {
    let tempdir = tempdir()?;
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(&config_path, build_config_toml(tempdir.path())?)?;

    let mut command = zinder_ingest_command();
    command.args(["--print-config", "--config", path_str(&config_path)?]);
    command.args(args);
    command.envs(environment.iter().copied());
    let output = command.output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains(expected_error), "{stderr}");

    Ok(())
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

[ingest.run_overrides]
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

[ingest.run_overrides]
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

fn canonical_replay_verification_config_toml(
    storage_path: &Path,
    secondary_path: &Path,
) -> Result<String, Box<dyn Error>> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?
    ))
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}
