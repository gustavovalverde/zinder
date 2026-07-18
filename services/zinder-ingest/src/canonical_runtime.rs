//! Version-1 canonical construction, admission, and follower handoff.

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zinder_core::{BlockHeight, BlockId, NetworkUpgradeActivations, UnixTimestampMillis};
use zinder_runtime::{IngestPhase, Readiness, ReadinessState};
use zinder_source::{NodeSource, SourceError};
use zinder_store::{
    CanonicalBaselinePublication, CanonicalReorgPolicy, CanonicalStoreBuildPlan,
    CanonicalStoreBuildPlanError, CanonicalStoreError, CanonicalStoreWorkload,
    RocksDbCanonicalBuilder, RocksDbCanonicalStore, RocksDbResourceBudget,
};

use crate::{
    CanonicalConstructionConfig, CanonicalConstructionError, CanonicalControlCommand,
    CanonicalFollowConfig, CanonicalFollowError, CanonicalFollower, follow_canonical_tip,
    follow_canonical_tip_with_control, load_fresh_canonical,
};

/// Complete concrete configuration for the first `RocksDB` single-host runtime.
#[derive(Clone, Debug)]
pub struct CanonicalRuntimeConfig {
    /// Final version-1 canonical store path.
    pub storage_path: PathBuf,
    /// Bounded `RocksDB` resources for construction, following, and reopen.
    pub resource_budget: RocksDbResourceBudget,
    /// Bounded source and preparation settings for fresh construction.
    pub construction: CanonicalConstructionConfig,
    /// Optional authenticated predecessor for a checkpointed build.
    pub checkpoint_height: Option<BlockHeight>,
    /// Maximum depth used to select the baseline settled tip.
    pub reorg_window_blocks: u32,
    /// Continuous follower policy.
    pub follow: CanonicalFollowConfig,
}

/// Failure while opening or constructing the version-1 runtime.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalRuntimeError {
    /// The configured store path could not be inspected.
    #[error("canonical runtime path {path:?} is unavailable: {source}")]
    PathUnavailable {
        /// Path being inspected or changed.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The source could not provide an identity needed before construction.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// The fixed construction range was invalid.
    #[error(transparent)]
    BuildPlan(#[from] CanonicalStoreBuildPlanError),
    /// Fresh canonical loading or validation failed.
    #[error(transparent)]
    Construction(#[from] CanonicalConstructionError),
    /// Canonical store admission, publication, or readback failed.
    #[error(transparent)]
    Store(#[from] CanonicalStoreError),
    /// The checkpoint cannot yet have a retained successor at the observed source tip.
    #[error(
        "source tip {source_tip:?} has not advanced beyond canonical checkpoint {checkpoint_height:?}"
    )]
    CheckpointNotBehindSource {
        /// Configured authenticated predecessor height.
        checkpoint_height: BlockHeight,
        /// Latest atomic source observation.
        source_tip: BlockId,
    },
    /// Continuous following failed after READY admission.
    #[error(transparent)]
    Follow(#[from] CanonicalFollowError),
    /// A staged READY store changed identity while it was installed and cold-reopened.
    #[error("installed canonical READY fence differs from staged publication")]
    InstalledFenceMismatch,
    /// The private canonical-control server stopped while the writer was still running.
    #[error("canonical control server failed: {reason}")]
    ControlServer {
        /// Redacted tonic transport failure.
        reason: String,
    },
}

/// Opens or freshly constructs the version-1 store, then follows Zebra.
pub async fn run_canonical_runtime<Source>(
    source: &Source,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    config: CanonicalRuntimeConfig,
    readiness: &Readiness,
    cancel: &CancellationToken,
) -> Result<RocksDbCanonicalStore, CanonicalRuntimeError>
where
    Source: NodeSource + Clone,
{
    run_canonical_runtime_with_control(
        source,
        network_upgrade_activations,
        config,
        readiness,
        cancel,
        None,
    )
    .await
}

/// Opens or constructs the one canonical primary, then follows while serving
/// commands from the private canonical-control channel.
#[expect(
    clippy::too_many_arguments,
    reason = "the controlled variant preserves the established runtime dependency boundary while adding only the optional owner command receiver"
)]
pub async fn run_canonical_runtime_with_control<Source>(
    source: &Source,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    config: CanonicalRuntimeConfig,
    readiness: &Readiness,
    cancel: &CancellationToken,
    control_commands: Option<mpsc::Receiver<CanonicalControlCommand>>,
) -> Result<RocksDbCanonicalStore, CanonicalRuntimeError>
where
    Source: NodeSource + Clone,
{
    let store = open_or_construct_canonical_store(
        source,
        Arc::clone(&network_upgrade_activations),
        &config,
        readiness,
    )
    .await?;
    let follower = CanonicalFollower::new(
        source,
        network_upgrade_activations,
        config.follow,
        readiness,
        cancel,
    );
    let store = match control_commands {
        Some(control_commands) => {
            follow_canonical_tip_with_control(store, follower, control_commands).await?
        }
        None => follow_canonical_tip(store, follower).await?,
    };
    Ok(store)
}

async fn open_or_construct_canonical_store<Source>(
    source: &Source,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    config: &CanonicalRuntimeConfig,
    readiness: &Readiness,
) -> Result<RocksDbCanonicalStore, CanonicalRuntimeError>
where
    Source: NodeSource + Clone,
{
    if let Some(store) = open_existing_store(network_upgrade_activations.as_ref(), config)? {
        return Ok(store);
    }
    let staging_path = construction_staging_path(&config.storage_path);
    if let Some(store) =
        recover_staged_store(&staging_path, network_upgrade_activations.as_ref(), config)?
    {
        return Ok(store);
    }
    construct_fresh_store(
        source,
        network_upgrade_activations.as_ref(),
        config,
        readiness,
        &staging_path,
    )
    .await
}

fn open_existing_store(
    network_upgrade_activations: &NetworkUpgradeActivations,
    config: &CanonicalRuntimeConfig,
) -> Result<Option<RocksDbCanonicalStore>, CanonicalRuntimeError> {
    if !path_exists(&config.storage_path)? {
        return Ok(None);
    }
    let store = RocksDbCanonicalStore::open_ready(
        &config.storage_path,
        network_upgrade_activations,
        CanonicalStoreWorkload::Wallet,
        CanonicalReorgPolicy::new(config.reorg_window_blocks)?,
        config.resource_budget,
    )?;
    tracing::info!(
        target: "zinder::ingest",
        event = "canonical_ready_store_reopened",
        storage_path = %config.storage_path.display(),
        visible_tip_height = store.event_fence().visible_tip().height.value(),
        chain_epoch = store.event_fence().chain_epoch_id().value(),
        chain_event_sequence = store.event_fence().chain_event_sequence(),
        "reopened the authenticated version-1 canonical fence"
    );
    Ok(Some(store))
}

fn recover_staged_store(
    staging_path: &std::path::Path,
    network_upgrade_activations: &NetworkUpgradeActivations,
    config: &CanonicalRuntimeConfig,
) -> Result<Option<RocksDbCanonicalStore>, CanonicalRuntimeError> {
    if !path_exists(staging_path)? {
        discard_unpublished_block_load_staging(staging_path)?;
        return Ok(None);
    }
    if remove_empty_construction_staging(staging_path)? {
        return Ok(None);
    }
    match RocksDbCanonicalStore::open_ready(
        staging_path,
        network_upgrade_activations,
        CanonicalStoreWorkload::Wallet,
        CanonicalReorgPolicy::new(config.reorg_window_blocks)?,
        config.resource_budget,
    ) {
        Ok(staged_ready) => {
            drop(staged_ready);
            install_staged_store(staging_path, &config.storage_path)?;
            RocksDbCanonicalStore::open_ready(
                &config.storage_path,
                network_upgrade_activations,
                CanonicalStoreWorkload::Wallet,
                CanonicalReorgPolicy::new(config.reorg_window_blocks)?,
                config.resource_budget,
            )
            .map(Some)
            .map_err(CanonicalRuntimeError::from)
        }
        Err(CanonicalStoreError::StoreNotReady { .. }) => {
            remove_unpublished_staging(staging_path)?;
            Ok(None)
        }
        Err(source) => Err(source.into()),
    }
}

fn remove_empty_construction_staging(
    staging_path: &std::path::Path,
) -> Result<bool, CanonicalRuntimeError> {
    let mut entries = std::fs::read_dir(staging_path).map_err(|source| {
        CanonicalRuntimeError::PathUnavailable {
            path: staging_path.to_path_buf(),
            source,
        }
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|source| CanonicalRuntimeError::PathUnavailable {
            path: staging_path.to_path_buf(),
            source,
        })?
        .is_some()
    {
        return Ok(false);
    }
    discard_unpublished_block_load_staging(staging_path)?;
    std::fs::remove_dir(staging_path).map_err(|source| CanonicalRuntimeError::PathUnavailable {
        path: staging_path.to_path_buf(),
        source,
    })?;
    tracing::warn!(
        target: "zinder::ingest",
        event = "canonical_empty_construction_staging_removed",
        staging_path = %staging_path.display(),
        "removed an empty runtime-owned construction staging directory"
    );
    Ok(true)
}

fn remove_unpublished_staging(staging_path: &std::path::Path) -> Result<(), CanonicalRuntimeError> {
    discard_unpublished_block_load_staging(staging_path)?;
    std::fs::remove_dir_all(staging_path).map_err(|source| {
        CanonicalRuntimeError::PathUnavailable {
            path: staging_path.to_path_buf(),
            source,
        }
    })?;
    tracing::warn!(
        target: "zinder::ingest",
        event = "canonical_unpublished_construction_restarted",
        staging_path = %staging_path.display(),
        "removed an unpublished runtime-owned construction staging directory"
    );
    Ok(())
}

fn discard_unpublished_block_load_staging(
    staging_path: &std::path::Path,
) -> Result<(), CanonicalRuntimeError> {
    if RocksDbCanonicalBuilder::discard_unpublished_block_load_staging(staging_path)? {
        tracing::warn!(
            target: "zinder::ingest",
            event = "canonical_unpublished_block_load_staging_discarded",
            canonical_build_path = %staging_path.display(),
            "discarded unpublished runtime-owned canonical block-load staging"
        );
    }
    Ok(())
}

async fn construct_fresh_store<Source>(
    source: &Source,
    network_upgrade_activations: &NetworkUpgradeActivations,
    config: &CanonicalRuntimeConfig,
    readiness: &Readiness,
    staging_path: &std::path::Path,
) -> Result<RocksDbCanonicalStore, CanonicalRuntimeError>
where
    Source: NodeSource + Clone,
{
    readiness.set(ReadinessState::syncing(None, None, None).with_phase(IngestPhase::BulkCatchup));
    let (build_plan, settled_tip, observed_tip) =
        resolve_construction_range(source, network_upgrade_activations, config).await?;
    readiness.set(
        ReadinessState::syncing(
            Some(u64::from(observed_tip.height.value().saturating_sub(
                build_plan.history_bounds().first_available_height().value(),
            ))),
            None,
            Some(observed_tip.height.value()),
        )
        .with_phase(IngestPhase::BulkCatchup),
    );
    let builder = RocksDbCanonicalBuilder::create_fresh(
        staging_path,
        CanonicalStoreWorkload::Wallet,
        build_plan,
        config.resource_budget,
    )?;
    tracing::info!(
        target: "zinder::ingest",
        event = "canonical_fresh_construction_started",
        staging_path = %staging_path.display(),
        fixed_tip_height = observed_tip.height.value(),
        "started version-1 canonical construction"
    );
    let loaded = load_fresh_canonical(builder, source, &config.construction).await?;
    let validated = loaded.builder.validate_for_publication()?;
    let publication = validated.prepare_baseline(CanonicalBaselinePublication::new(
        settled_tip,
        UnixTimestampMillis::now(),
    ))?;
    let staged_ready = validated.publish_baseline(publication)?;
    let published_fence = staged_ready.event_fence();
    drop(staged_ready);
    install_staged_store(staging_path, &config.storage_path)?;
    let store = RocksDbCanonicalStore::open_ready(
        &config.storage_path,
        network_upgrade_activations,
        CanonicalStoreWorkload::Wallet,
        CanonicalReorgPolicy::new(config.reorg_window_blocks)?,
        config.resource_budget,
    )?;
    if store.event_fence() != published_fence {
        return Err(CanonicalRuntimeError::InstalledFenceMismatch);
    }
    tracing::info!(
        target: "zinder::ingest",
        event = "canonical_fresh_construction_ready",
        storage_path = %config.storage_path.display(),
        visible_tip_height = published_fence.visible_tip().height.value(),
        chain_epoch = published_fence.chain_epoch_id().value(),
        chain_event_sequence = published_fence.chain_event_sequence(),
        "published and cold-reopened the version-1 canonical baseline"
    );
    Ok(store)
}

async fn resolve_construction_range<Source>(
    source: &Source,
    network_upgrade_activations: &NetworkUpgradeActivations,
    config: &CanonicalRuntimeConfig,
) -> Result<(CanonicalStoreBuildPlan, BlockId, BlockId), CanonicalRuntimeError>
where
    Source: NodeSource,
{
    let source_tip = source.tip_id().await?;
    let fixed_tip = match config.follow.target_height {
        Some(target_height) if target_height < source_tip.height => {
            let block = source.fetch_block_at(target_height).await?;
            BlockId::new(target_height, block.hash)
        }
        _ => source_tip,
    };
    let build_plan = if let Some(checkpoint_height) = config.checkpoint_height {
        if fixed_tip.height <= checkpoint_height {
            return Err(CanonicalRuntimeError::CheckpointNotBehindSource {
                checkpoint_height,
                source_tip: fixed_tip,
            });
        }
        let checkpoint = source
            .fetch_chain_checkpoint(checkpoint_height, network_upgrade_activations)
            .await?;
        CanonicalStoreBuildPlan::checkpointed(
            network_upgrade_activations,
            checkpoint,
            fixed_tip,
            CanonicalReorgPolicy::new(config.reorg_window_blocks)?,
        )?
    } else {
        let genesis = source.fetch_block_at(BlockHeight::new(0)).await?;
        CanonicalStoreBuildPlan::complete(
            network_upgrade_activations,
            genesis.block_time_seconds,
            fixed_tip,
            CanonicalReorgPolicy::new(config.reorg_window_blocks)?,
        )?
    };
    let first_retained_height = build_plan.history_bounds().first_available_height();
    let settled_height = BlockHeight::new(
        fixed_tip
            .height
            .value()
            .saturating_sub(config.reorg_window_blocks)
            .max(first_retained_height.value()),
    );
    let settled_tip = if settled_height == fixed_tip.height {
        fixed_tip
    } else {
        let block = source.fetch_block_at(settled_height).await?;
        BlockId::new(settled_height, block.hash)
    };
    Ok((build_plan, settled_tip, fixed_tip))
}

fn construction_staging_path(storage_path: &std::path::Path) -> PathBuf {
    let mut staging_path = OsString::from(storage_path.as_os_str());
    staging_path.push(".building");
    PathBuf::from(staging_path)
}

fn path_exists(path: &std::path::Path) -> Result<bool, CanonicalRuntimeError> {
    path.try_exists()
        .map_err(|source| CanonicalRuntimeError::PathUnavailable {
            path: path.to_path_buf(),
            source,
        })
}

fn install_staged_store(
    staging_path: &std::path::Path,
    storage_path: &std::path::Path,
) -> Result<(), CanonicalRuntimeError> {
    std::fs::rename(staging_path, storage_path).map_err(|source| {
        CanonicalRuntimeError::PathUnavailable {
            path: storage_path.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tempfile::tempdir;

    use super::{
        discard_unpublished_block_load_staging, remove_empty_construction_staging,
        remove_unpublished_staging,
    };

    #[test]
    fn empty_construction_staging_is_removed() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let staging_path = tempdir.path().join("store.building");
        std::fs::create_dir(&staging_path)?;

        assert!(remove_empty_construction_staging(&staging_path)?);
        assert!(!staging_path.exists());
        Ok(())
    }

    #[test]
    fn populated_construction_staging_is_preserved() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let staging_path = tempdir.path().join("store.building");
        std::fs::create_dir(&staging_path)?;
        std::fs::write(staging_path.join("CURRENT"), b"MANIFEST-000001\n")?;

        assert!(!remove_empty_construction_staging(&staging_path)?);
        assert!(staging_path.exists());
        Ok(())
    }

    #[test]
    fn unpublished_construction_removes_block_load_staging() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let staging_path = tempdir.path().join("store.building");
        let block_load_staging_path = tempdir.path().join("store.building.block-load-staging");
        std::fs::create_dir(&staging_path)?;
        std::fs::create_dir(&block_load_staging_path)?;
        std::fs::write(block_load_staging_path.join("partial.sst"), b"partial")?;

        remove_unpublished_staging(&staging_path)?;

        assert!(!staging_path.exists());
        assert!(!block_load_staging_path.exists());
        Ok(())
    }

    #[test]
    fn orphaned_block_load_staging_is_discarded() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let staging_path = tempdir.path().join("store.building");
        let block_load_staging_path = tempdir.path().join("store.building.block-load-staging");
        std::fs::create_dir(&block_load_staging_path)?;
        std::fs::write(block_load_staging_path.join("partial.sst"), b"partial")?;

        discard_unpublished_block_load_staging(&staging_path)?;

        assert!(!staging_path.exists());
        assert!(!block_load_staging_path.exists());
        Ok(())
    }

    #[test]
    fn missing_block_load_staging_is_accepted() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;

        discard_unpublished_block_load_staging(&tempdir.path().join("store.building"))?;

        Ok(())
    }
}
