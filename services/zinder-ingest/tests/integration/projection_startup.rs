#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, path::Path, sync::Arc, time::Duration};

use eyre::{Result, eyre};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zinder_core::{
    BlockHeight, ChainEpoch, ChainEpochId, Network, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint, TransparentOutputArtifact, TransparentSpendFact, TransparentUnspentOutput,
    UnixTimestampMillis,
};
use zinder_derive::{
    ConsumerProjectionCoverage, ConsumerProjectionState, DeriveConsumerSchema, DeriveStore,
    DeriveStoreOptions, ProjectionPreset, TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA, TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILIES,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
};
use zinder_ingest::{
    CommitmentRootBackfillConfig, ConventionalFeeDistributionBackfillConfig, DeriveReplayPolicy,
    HistoricalWorkGate, IngestDeriveConfig, PaidFeeDistributionBackfillConfig,
    ProjectionStartupInputs, ProjectionStartupPlan, ProjectionStartupSettings,
    ProjectionStartupWork, TransactionComponentBackfillConfig, TransactionHistoryVerifierConfig,
    ValuePoolBalanceBackfillConfig, ValuePoolFlowBackfillConfig,
    open_primary_derive_store_for_canonical_with_projection_preset,
};
use zinder_runtime::{IngestPhase, Readiness};
use zinder_store::{
    ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange,
    RocksDbResourceBudget,
};
use zinder_testkit::{
    ChainFixture, MockNodeSource, encode_fixture_block_replay, sample_regtest_upgrade_activations,
};

const LEGACY_OUTPOINT_SPEND_SCHEMA: DeriveConsumerSchema = DeriveConsumerSchema::new(
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
    0,
    TRANSPARENT_OUTPOINT_SPEND_COLUMN_FAMILIES,
);
const LEGACY_WALLET_SCHEMAS: &[DeriveConsumerSchema] = &[
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
    LEGACY_OUTPOINT_SPEND_SCHEMA,
];

fn enabled_settings() -> ProjectionStartupSettings {
    ProjectionStartupSettings {
        derive: IngestDeriveConfig {
            replay_batch_blocks: NonZeroU32::MIN,
            replay_policy: DeriveReplayPolicy::Continuous,
            memory_budget_bytes: None,
            memory_degrade_ratio: 0.85,
            memory_pause_ratio: 0.95,
            memory_resume_ratio: 0.75,
            min_replay_batch_blocks: NonZeroU32::MIN,
            startup_handoff_lag_blocks: 1_000,
        },
        commitment_root_backfill: CommitmentRootBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
            fetch_concurrency: NonZeroU32::MIN,
        },
        conventional_fee_distribution_backfill: ConventionalFeeDistributionBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
        },
        paid_fee_distribution_backfill: PaidFeeDistributionBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
            fetch_concurrency: NonZeroU32::MIN,
            history_days: NonZeroU32::MIN,
            timestamp_safety_seconds: 0,
        },
        transaction_component_backfill: TransactionComponentBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
        },
        transaction_history_verifier: TransactionHistoryVerifierConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
        },
        value_pool_flow_backfill: ValuePoolFlowBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
            fetch_concurrency: NonZeroU32::MIN,
        },
        value_pool_balance_backfill: ValuePoolBalanceBackfillConfig {
            enabled: true,
            batch_blocks: NonZeroU32::MIN,
            fetch_concurrency: NonZeroU32::MIN,
        },
    }
}

fn transparent_spend_fixture(block_count: u32) -> Result<ChainFixture> {
    let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(block_count);
    let received_block = fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture receive block missing"))?;
    let spending_block = fixture
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("fixture spend block missing"))?;
    let output = TransparentOutputArtifact::new(
        TransparentOutPoint::new(TransactionId::from_bytes([0x41; 32]), 0),
        50_000,
        b"projection-startup-script".to_vec(),
        TransparentAddressScriptHash::from_bytes([0x42; 32]),
        received_block.height,
        received_block.hash,
    );
    let spend = TransparentSpendFact::from_input_and_output(
        output.outpoint,
        0,
        TransactionId::from_bytes([0x43; 32]),
        0,
        spending_block.height,
        spending_block.hash,
        &output,
    );
    Ok(fixture
        .with_address_output_index(TransparentUnspentOutput::new(
            output.address_script_hash,
            output.script_pub_key.clone(),
            output.outpoint,
            output.value_zat,
            output.block_height,
            output.block_hash,
        ))
        .with_transparent_spend_fact(spend))
}

#[tokio::test]
async fn wallet_startup_runs_only_wallet_projection_work() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
        &canonical_path,
        RocksDbResourceBudget::for_local_tests(),
        ProjectionPreset::Wallet,
    )?;
    let source = MockNodeSource::from_chain(ChainFixture::new(Network::ZcashRegtest));
    let plan = ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet);
    assert!(plan.includes(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME));
    assert!(plan.includes(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME));
    assert_eq!(
        plan.selected_work().collect::<Vec<_>>(),
        vec![
            ProjectionStartupWork::Replay,
            ProjectionStartupWork::DeriveTailer,
        ]
    );

    let cancel = CancellationToken::new();
    let readiness = Readiness::default();
    readiness.set_phase(IngestPhase::BulkCatchup);
    let historical_work_gate = HistoricalWorkGate::new(readiness);
    let runtime = plan
        .start(ProjectionStartupInputs {
            settings: enabled_settings(),
            request_timeout: Duration::from_secs(1),
            activations: Arc::new(sample_regtest_upgrade_activations()),
            source: Arc::new(source.clone()),
            chain_store: &chain_store,
            derive_store: &derive_store,
            historical_work_gate: &historical_work_gate,
            cancel: &cancel,
        })
        .await?;
    assert!(runtime.optional_task_names().is_empty());
    assert_eq!(source.fetch_attempts(), 0);
    cancel.cancel();
    runtime.join().await;
    Ok(())
}

#[tokio::test]
async fn startup_defers_existing_derive_replay_to_the_tailer() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let artifacts = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture unexpectedly empty"))?;
    chain_store.commit_chain_epoch(artifacts)?;
    let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
        &canonical_path,
        RocksDbResourceBudget::for_local_tests(),
        ProjectionPreset::Wallet,
    )?;
    let source = MockNodeSource::from_chain(ChainFixture::new(Network::ZcashRegtest));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let readiness = Readiness::default();
    readiness.set_phase(IngestPhase::FollowingTip);
    let historical_work_gate = HistoricalWorkGate::new(readiness);

    let runtime = ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .start(ProjectionStartupInputs {
            settings: enabled_settings(),
            request_timeout: Duration::from_secs(1),
            activations: Arc::new(sample_regtest_upgrade_activations()),
            source: Arc::new(source),
            chain_store: &chain_store,
            derive_store: &derive_store,
            historical_work_gate: &historical_work_gate,
            cancel: &cancel,
        })
        .await?;

    assert_eq!(
        derive_store.get_chain_event_cursor(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?,
        None,
        "startup must not synchronously replay persisted derive debt"
    );
    runtime.join().await;
    Ok(())
}

#[test]
fn complete_startup_plan_owns_every_optional_projection_job() {
    let plan = ProjectionStartupPlan::for_preset(ProjectionPreset::Explorer);
    assert_eq!(
        plan.selected_work().collect::<Vec<_>>(),
        ProjectionStartupWork::all().to_vec()
    );
}

#[tokio::test]
async fn startup_rejects_a_plan_store_mismatch() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
        &canonical_path,
        RocksDbResourceBudget::for_local_tests(),
        ProjectionPreset::Wallet,
    )?;
    let source = MockNodeSource::from_chain(ChainFixture::new(Network::ZcashRegtest));
    let cancel = CancellationToken::new();
    let readiness = Readiness::default();
    readiness.set_phase(IngestPhase::BulkCatchup);
    let historical_work_gate = HistoricalWorkGate::new(readiness);

    let outcome = ProjectionStartupPlan::for_preset(ProjectionPreset::Explorer)
        .start(ProjectionStartupInputs {
            settings: enabled_settings(),
            request_timeout: Duration::from_secs(1),
            activations: Arc::new(sample_regtest_upgrade_activations()),
            source: Arc::new(source.clone()),
            chain_store: &chain_store,
            derive_store: &derive_store,
            historical_work_gate: &historical_work_gate,
            cancel: &cancel,
        })
        .await;

    match outcome {
        Err(zinder_ingest::IngestError::DeriveDispatch(reason)) => {
            assert!(reason.contains("does not match the opened derive store"));
        }
        _ => return Err(eyre!("expected startup plan/store mismatch")),
    }
    assert_eq!(source.fetch_attempts(), 0);
    Ok(())
}

#[test]
fn fresh_storage_pair_preflight_does_not_create_the_derive_store() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let derive_path = zinder_derive::DeriveStore::path_for_canonical(&canonical_path);

    ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path)?;

    assert!(!derive_path.exists());
    Ok(())
}

#[test]
fn canonical_history_without_a_derive_store_is_rejected_before_creation() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let artifacts = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture unexpectedly empty"))?;
    chain_store.commit_chain_epoch(artifacts)?;
    let derive_path = zinder_derive::DeriveStore::path_for_canonical(&canonical_path);

    let outcome = ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path);

    assert!(matches!(
        outcome,
        Err(zinder_ingest::IngestError::ProjectionStoreMissingForCanonical { .. })
    ));
    assert!(!derive_path.exists());
    Ok(())
}

#[test]
fn matching_wallet_storage_pair_can_restart() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let artifacts = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture unexpectedly empty"))?;
    let commit = chain_store.commit_chain_epoch(artifacts)?;
    {
        let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
            &canonical_path,
            RocksDbResourceBudget::for_local_tests(),
            ProjectionPreset::Wallet,
        )?;
        for schema in ProjectionPreset::Wallet.consumer_schemas() {
            derive_store
                .put_chain_event_cursor(schema.name, commit.event_envelope.cursor.as_bytes())?;
        }
    }

    ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path)?;
    Ok(())
}

#[test]
fn preflight_rejects_a_tip_height_without_retention_coverage() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let artifacts = transparent_spend_fixture(5)?
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture unexpectedly empty"))?;
    let commit = chain_store.commit_chain_epoch(artifacts)?;
    chain_store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    chain_store.sweep_transparent_retention_once()?;
    assert_eq!(
        chain_store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(5))
    );

    {
        let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
            &canonical_path,
            RocksDbResourceBudget::for_local_tests(),
            ProjectionPreset::Wallet,
        )?;
        derive_store.put_consumer(
            zinder_derive::TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
            &zinder_core::wire::encode_height_key_ascending(BlockHeight::new(5)),
            &[],
        )?;
    }

    let outcome = ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path);

    assert!(matches!(
        outcome,
        Err(zinder_ingest::IngestError::ProjectionRetentionCoverageInsufficient { .. })
    ));

    {
        let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
            &canonical_path,
            RocksDbResourceBudget::for_local_tests(),
            ProjectionPreset::Wallet,
        )?;
        derive_store.put_consumer_projection_state(
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
            ConsumerProjectionState {
                projection_epoch_id: commit.chain_epoch.id,
                projection_tip_height: commit.chain_epoch.visible_tip_height,
                projection_tip_hash: commit.chain_epoch.visible_tip_hash,
                revision: 1,
                coverage: Some(ConsumerProjectionCoverage {
                    complete_from_height: BlockHeight::new(1),
                    complete_through_height: commit.chain_epoch.visible_tip_height,
                    complete_through_hash: commit.chain_epoch.visible_tip_hash,
                }),
            },
        )?;
    }
    ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path)?;
    Ok(())
}

#[test]
fn preflight_allows_destructive_rebuild_only_with_full_event_history() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let first_epoch = transparent_spend_fixture(5)?
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("chain fixture unexpectedly empty"))?;
    let first_commit = chain_store.commit_chain_epoch(first_epoch)?;
    chain_store.set_transparent_retention_release_height(BlockHeight::new(5))?;
    chain_store.sweep_transparent_retention_once()?;
    assert_eq!(
        chain_store.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(5))
    );
    let fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(6);
    let block = fixture
        .block_at(BlockHeight::new(6))
        .ok_or_else(|| eyre!("fixture block 6 missing"))?;
    let chain_epoch = fixture
        .chain_epoch(ChainEpochId::new(2))
        .ok_or_else(|| eyre!("fixture epoch missing"))?;
    let block_header = block.block_header_artifact();
    let replay_envelope = encode_fixture_block_replay(&block_header, &[]);
    let second_commit = chain_store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            chain_epoch,
            vec![block_header],
            vec![replay_envelope],
            vec![block.compact_block_artifact()],
        )
        .with_reorg_window_change(ReorgWindowChange::Extend {
            block_range: zinder_core::BlockHeightRange::inclusive(block.height, block.height),
        }),
    )?;

    seed_legacy_wallet_projection_for_rebuild(&canonical_path, second_commit.chain_epoch)?;

    ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path)?;
    chain_store.prune_chain_events_before(UnixTimestampMillis::new(u64::MAX))?;
    let outcome = ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path);

    assert!(matches!(
        outcome,
        Err(
            zinder_ingest::IngestError::ProjectionRetentionCoverageInsufficient {
                destructive_rebuild: true,
                ..
            }
        )
    ));
    assert_ne!(
        first_commit.event_envelope.cursor,
        second_commit.event_envelope.cursor
    );
    Ok(())
}

fn seed_legacy_wallet_projection_for_rebuild(
    canonical_path: &Path,
    chain_epoch: ChainEpoch,
) -> Result<()> {
    let derive_store = DeriveStore::open(
        DeriveStore::path_for_canonical(canonical_path),
        DeriveStoreOptions {
            consumers: LEGACY_WALLET_SCHEMAS,
            rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            sync_writes: false,
        },
    )?;
    derive_store.put_consumer(
        zinder_derive::TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
        &zinder_core::wire::encode_height_key_ascending(chain_epoch.visible_tip_height),
        &[],
    )?;
    derive_store.put_consumer_projection_state(
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
        ConsumerProjectionState {
            projection_epoch_id: chain_epoch.id,
            projection_tip_height: chain_epoch.visible_tip_height,
            projection_tip_hash: chain_epoch.visible_tip_hash,
            revision: 1,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: chain_epoch.visible_tip_height,
                complete_through_hash: chain_epoch.visible_tip_hash,
            }),
        },
    )?;
    Ok(())
}

#[test]
fn projection_data_without_canonical_history_is_rejected() -> Result<()> {
    let tempdir = tempdir()?;
    let canonical_path = tempdir.path().join("canonical");
    let chain_store = PrimaryChainStore::open(
        &canonical_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    {
        let derive_store = open_primary_derive_store_for_canonical_with_projection_preset(
            &canonical_path,
            RocksDbResourceBudget::for_local_tests(),
            ProjectionPreset::Wallet,
        )?;
        derive_store.put_chain_event_cursor(
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
            b"orphaned-projection-cursor",
        )?;
    }

    let outcome = ProjectionStartupPlan::for_preset(ProjectionPreset::Wallet)
        .preflight_storage_pair(&chain_store, &canonical_path);

    assert!(matches!(
        outcome,
        Err(zinder_ingest::IngestError::ProjectionStoreWithoutCanonicalHistory { .. })
    ));
    Ok(())
}
