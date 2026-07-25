#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::{NonZeroU32, NonZeroU64};

use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::{TempDir, tempdir};
use zinder_bench::{
    capture::measure_fixture_blocks,
    fixture::{
        ActivationRecord, FIXTURE_CONTRACT_IDENTITY, FIXTURE_FORMAT_VERSION, FixtureManifest,
        SubtreeRootSet, write_segment,
    },
    recorder::install_recorder,
    replay::{ReplayConfig, replay_fixture},
    report::{AcceptanceThresholds, StartingCanonicalStateKind},
};
use zinder_core::{BlockHeight, Network, wire::encode_zinder_native_chain_name};
use zinder_source::SourceBlock;
use zinder_testkit::sample_regtest_upgrade_activations;

const REGTEST_BLOCK_1: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-block-1.json");
const REGTEST_BLOCK_603: &str =
    include_str!("../../zinder-ingest/tests/fixtures/z3-regtest-ironwood-block-603.json");

fn regtest_activation_records() -> Vec<ActivationRecord> {
    sample_regtest_upgrade_activations()
        .activations()
        .iter()
        .map(|activation| ActivationRecord {
            branch_id: activation.branch_id.value(),
            activation_height: activation.activation_height.value(),
            name: activation.name.clone(),
        })
        .collect()
}

fn write_regtest_fixture() -> Result<TempDir> {
    write_regtest_fixture_from_json(REGTEST_BLOCK_1)
}

fn write_non_genesis_manifest_fixture() -> Result<TempDir> {
    write_regtest_fixture_from_json(REGTEST_BLOCK_603)
}

fn write_regtest_fixture_from_json(fixture_json: &str) -> Result<TempDir> {
    let fixture: Value = serde_json::from_str(fixture_json)?;
    let height = fixture
        .get("height")
        .and_then(Value::as_u64)
        .and_then(|height| u32::try_from(height).ok())
        .ok_or_else(|| eyre!("fixture height must fit in u32"))?;
    let raw_block_hex = fixture
        .get("raw_block_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture raw_block_hex must be a string"))?;
    let block = SourceBlock::from_raw_block_bytes(
        Network::ZcashRegtest,
        BlockHeight::new(height),
        hex::decode(raw_block_hex)?,
    )?;
    let fixture_directory = tempdir()?;
    let descriptor = write_segment(fixture_directory.path(), 0, std::slice::from_ref(&block))?;
    let measurements = measure_fixture_blocks(
        std::slice::from_ref(&block),
        &sample_regtest_upgrade_activations(),
    )?;
    FixtureManifest {
        contract_identity: FIXTURE_CONTRACT_IDENTITY.to_owned(),
        fixture_format_version: FIXTURE_FORMAT_VERSION,
        network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
        from_height: height,
        to_height: height,
        block_count: 1,
        workload_density: measurements.workload_density,
        canonical_artifact_schema_version: zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
        canonical_block_facts_digest_evidence: measurements
            .canonical_block_facts_digest_evidence()?,
        tip_hash_hex: hex::encode(block.hash.as_bytes()),
        network_upgrade_activations: regtest_activation_records(),
        segments: vec![descriptor],
        subtree_roots: SubtreeRootSet::default(),
    }
    .write(fixture_directory.path())?;
    Ok(fixture_directory)
}

fn replay_config(fixture_directory: &TempDir, store_directory: &TempDir) -> Result<ReplayConfig> {
    Ok(ReplayConfig {
        fixture_directory: fixture_directory.path().to_path_buf(),
        store_path: store_directory.path().join("canonical"),
        block_prepare_concurrency: NonZeroU32::new(2).ok_or_else(|| eyre!("2 is non-zero"))?,
        max_response_bytes: None,
        source_segment_max_blocks: None,
        source_segment_target_response_bytes: None,
        source_fetch_max_in_flight_requests: None,
        source_fetch_max_in_flight_bytes: None,
        block_prepare_memory_watermark_bytes: None,
        source_segment_delay_millis: 0,
        canonical_block_cache_bytes: None,
        software_revision: Some("test-revision".to_owned()),
        trial_id: None,
        fixture_cache_policy: None,
        run_started_at_unix_millis: 1,
        runner_id: Some("test-runner".to_owned()),
        cpu_limit_cores: Some(2.0),
        memory_limit_bytes: Some(1024 * 1024 * 1024),
        storage_class: Some("test-tmpfs".to_owned()),
        image_reference: Some(format!("sha256:{}", "a".repeat(64))),
        canonical_fixture_replay_thresholds: None,
    })
}

#[tokio::test]
async fn replay_reports_source_admission_and_delay_settings() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory)?;
    config.max_response_bytes = NonZeroU64::new(67_108_864);
    config.source_segment_max_blocks = NonZeroU32::new(64);
    config.source_segment_target_response_bytes = NonZeroU64::new(33_554_432);
    config.source_fetch_max_in_flight_requests = NonZeroU32::new(12);
    config.source_fetch_max_in_flight_bytes = NonZeroU64::new(156_249_984);
    config.block_prepare_memory_watermark_bytes = NonZeroU64::new(156_249_984);
    config.source_segment_delay_millis = 37;

    let report = replay_fixture(config, None).await?;

    assert_eq!(report.replay.max_response_bytes, 67_108_864);
    assert_eq!(report.replay.source_segment_max_blocks, 64);
    assert_eq!(
        report.replay.source_segment_target_response_bytes,
        33_554_432
    );
    assert_eq!(report.replay.source_fetch_max_in_flight_requests, 12);
    assert_eq!(report.replay.source_fetch_max_in_flight_bytes, 156_249_984);
    assert_eq!(
        report.replay.block_prepare_memory_watermark_bytes,
        156_249_984
    );
    assert_eq!(report.replay.source_segment_delay_millis, 37);
    Ok(())
}

#[tokio::test]
async fn replay_resolves_pipeline_watermarks_from_the_resource_envelope() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory)?;
    config.cpu_limit_cores = Some(10.0);
    config.memory_limit_bytes = Some(10 * 1024 * 1024 * 1024);
    config.max_response_bytes = NonZeroU64::new(64 * 1024 * 1024);
    config.source_fetch_max_in_flight_bytes = None;
    config.block_prepare_memory_watermark_bytes = None;

    let report = replay_fixture(config, None).await?;

    assert_eq!(
        report.replay.source_fetch_max_in_flight_bytes,
        160 * 1024 * 1024
    );
    assert_eq!(
        report.replay.block_prepare_memory_watermark_bytes,
        160 * 1024 * 1024
    );
    assert_eq!(report.replay.block_prepare_concurrency, 2);
    Ok(())
}

#[tokio::test]
async fn replay_rejects_source_segment_target_above_response_limit() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory)?;
    config.max_response_bytes = NonZeroU64::new(64 * 1024 * 1024);
    config.source_segment_target_response_bytes = NonZeroU64::new(128 * 1024 * 1024);

    let Some(error) = replay_fixture(config, None).await.err() else {
        return Err(eyre!(
            "replay must reject a source segment target above its response limit"
        ));
    };

    assert!(
        error
            .to_string()
            .contains("source segment target 134217728 exceeds maximum response 67108864")
    );
    Ok(())
}

fn write_starting_checkpoint_manifest(
    store_path: &std::path::Path,
    canonical_position: &serde_json::Value,
) -> Result<()> {
    std::fs::create_dir_all(store_path)?;
    let manifest = serde_json::json!({
        "format_version": 1,
        "network": "zcash-regtest",
        "canonical_position": canonical_position,
    });
    std::fs::write(
        store_path.join("zinder-benchmark-starting-store.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

#[test]
fn thresholded_config_requires_immutable_structured_provenance_and_telemetry() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory)?;
    config.canonical_fixture_replay_thresholds = Some(
        AcceptanceThresholds::try_from_seconds(10.0, 20.0)
            .map_err(|error| eyre!(error.to_string()))?,
    );

    assert!(config.validate(false).is_err());
    config.image_reference = Some("zinder-bench:mutable".to_owned());
    assert!(config.validate(true).is_err());
    config.image_reference = Some(format!("@sha256:{}", "a".repeat(64)));
    assert!(config.validate(true).is_err());
    config.image_reference = Some(format!("sha256:{}", "a".repeat(64)));
    config.cpu_limit_cores = None;
    assert!(config.validate(true).is_err());
    config.cpu_limit_cores = Some(2.0);
    config.validate(true)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thresholded_genesis_replay_accepts_a_proven_empty_start_without_manifest() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory)?;
    config.canonical_fixture_replay_thresholds = Some(
        AcceptanceThresholds::try_from_seconds(10.0, 20.0)
            .map_err(|error| eyre!(error.to_string()))?,
    );
    let report = replay_fixture(config, Some(install_recorder()?)).await?;
    assert_eq!(
        report.replay.starting_canonical_state.kind,
        StartingCanonicalStateKind::Empty
    );
    assert_eq!(
        report
            .replay
            .starting_canonical_state
            .checkpoint_manifest_sha256,
        None
    );
    assert_eq!(report.replay.starting_canonical_state.chain_epoch_id, None);
    assert_eq!(report.replay.starting_canonical_state.tip_height, None);
    assert_eq!(
        report.replay.starting_canonical_state.tip_hash_rpc_hex,
        None
    );
    let source_fetch = report
        .replay
        .source_fetch_attribution
        .as_ref()
        .ok_or_else(|| eyre!("replay report must include source-fetch attribution"))?;
    assert!(source_fetch.completed_segment_request_count >= 1);
    assert!(source_fetch.total_connected_blocks_returned >= 1);
    assert!(source_fetch.total_response_payload_bytes > 0);
    assert!(source_fetch.completed_segment_requests_per_second > 0.0);
    assert!(source_fetch.response_payload_bytes_per_second > 0.0);
    assert!(source_fetch.cumulative_fetch_chain_segment_task_seconds >= 0.0);
    report.validate_acceptance()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thresholded_non_genesis_replay_requires_a_checkpoint_manifest() -> Result<()> {
    let fixture_directory = write_non_genesis_manifest_fixture()?;
    let store_directory = tempdir()?;
    let mut config = replay_config(&fixture_directory, &store_directory)?;
    config.canonical_fixture_replay_thresholds = Some(
        AcceptanceThresholds::try_from_seconds(10.0, 20.0)
            .map_err(|error| eyre!(error.to_string()))?,
    );

    let Some(error) = replay_fixture(config, Some(install_recorder()?))
        .await
        .err()
    else {
        return Err(eyre!(
            "thresholded non-genesis replay must require checkpoint provenance"
        ));
    };
    assert!(error.to_string().contains("thresholded replay requires"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_rejects_checkpoint_manifest_position_that_disagrees_with_store() -> Result<()> {
    let fixture_directory = write_regtest_fixture()?;
    let store_directory = tempdir()?;
    let config = replay_config(&fixture_directory, &store_directory)?;
    write_starting_checkpoint_manifest(
        &config.store_path,
        &serde_json::json!({
            "chain_epoch_id": 7,
            "visible_tip_height": 6,
            "visible_tip_hash": "00".repeat(32),
            "artifact_schema_version": zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION.value(),
            "history_bounds": {"kind": "complete", "first_available_height": 1}
        }),
    )?;

    let Some(error) = replay_fixture(config, None).await.err() else {
        return Err(eyre!(
            "replay must reject checkpoint position that disagrees with the store"
        ));
    };
    assert!(error.to_string().contains("opened store is empty"));
    Ok(())
}
