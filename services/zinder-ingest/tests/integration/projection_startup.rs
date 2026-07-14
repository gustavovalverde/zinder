#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use eyre::{Result, eyre};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use zinder_core::{ChainEpochId, Network};
use zinder_derive::{
    ProjectionPreset, TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
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
use zinder_store::{ChainStoreOptions, PrimaryChainStore, RocksDbResourceBudget};
use zinder_testkit::{ChainFixture, MockNodeSource, sample_regtest_upgrade_activations};

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
            startup_phase: IngestPhase::BulkCatchup,
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

#[test]
fn complete_startup_plan_owns_every_optional_projection_job() {
    let plan = ProjectionStartupPlan::for_preset(ProjectionPreset::Complete);
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

    let outcome = ProjectionStartupPlan::for_preset(ProjectionPreset::Complete)
        .start(ProjectionStartupInputs {
            settings: enabled_settings(),
            request_timeout: Duration::from_secs(1),
            activations: Arc::new(sample_regtest_upgrade_activations()),
            source: Arc::new(source.clone()),
            chain_store: &chain_store,
            derive_store: &derive_store,
            startup_phase: IngestPhase::BulkCatchup,
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
