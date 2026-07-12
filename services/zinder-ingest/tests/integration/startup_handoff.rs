#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::{Result, eyre};
use prost::Message as _;
use zinder_core::{BlockHeight, BlockHeightRange, ChainEpochId, Network};
use zinder_derive::{BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore};
use zinder_ingest::{
    DeriveReplayPolicy, IngestDeriveConfig, TransparentAddressRankingBootstrapOutcome,
    bootstrap_transparent_address_ranking, catch_up_derive_store_to_canonical,
    catch_up_derive_store_to_canonical_until_handoff, open_primary_derive_store_for_canonical,
};
use zinder_proto::v1::wallet::{DeriveHealth, DeriveStatus};
use zinder_store::{ChainEpochArtifacts, ReorgWindowChange, RocksDbResourceBudget};
use zinder_testkit::{ChainFixture, StoreFixture};

const CANONICAL_HEIGHT: u32 = 24;
const FIRST_EPOCH_TIP: u32 = 12;

fn derive_config(startup_handoff_lag_blocks: u64) -> Result<IngestDeriveConfig> {
    Ok(IngestDeriveConfig {
        replay_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid replay batch"))?,
        replay_policy: DeriveReplayPolicy::DEFAULT,
        memory_budget_bytes: None,
        memory_degrade_ratio: 0.85,
        memory_pause_ratio: 0.95,
        memory_resume_ratio: 0.75,
        min_replay_batch_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid minimum replay batch"))?,
        startup_handoff_lag_blocks,
    })
}

fn committed_canonical_store() -> Result<StoreFixture> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(CANONICAL_HEIGHT);
    StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))
        .map_err(|error| eyre!("could not commit canonical chain fixture: {error}"))
}

/// Commits the canonical chain as two chain events so a bounded catch-up can
/// finish the first and leave the second unreplayed, reproducing the residual
/// lag the startup handoff hands to the tailer.
fn committed_two_epoch_store() -> Result<StoreFixture> {
    let first = ChainFixture::new(Network::ZcashRegtest).extend_blocks(FIRST_EPOCH_TIP);
    let store_fixture = StoreFixture::with_chain_committed(&first, ChainEpochId::new(1))
        .map_err(|error| eyre!("could not commit first chain epoch: {error}"))?;

    let full = ChainFixture::new(Network::ZcashRegtest).extend_blocks(CANONICAL_HEIGHT);
    let chain_epoch = full
        .chain_epoch(ChainEpochId::new(2))
        .ok_or_else(|| eyre!("second chain epoch fixture is empty"))?;
    let appended_from = usize::try_from(FIRST_EPOCH_TIP)?;
    let block_range = BlockHeightRange::inclusive(
        BlockHeight::new(FIRST_EPOCH_TIP + 1),
        BlockHeight::new(CANONICAL_HEIGHT),
    );
    store_fixture
        .chain_store()
        .commit_chain_epoch(
            ChainEpochArtifacts::new(
                chain_epoch,
                full.block_header_artifacts().split_off(appended_from),
                full.compact_block_artifacts().split_off(appended_from),
            )
            .with_block_blobs(full.block_blob_artifacts().split_off(appended_from))
            .with_reorg_window_change(ReorgWindowChange::Extend { block_range }),
        )
        .map_err(|error| eyre!("could not commit second chain epoch: {error}"))?;
    Ok(store_fixture)
}

fn block_summary_head(derive_store: &DeriveStore) -> Result<u32> {
    derive_store
        .last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)?
        .map(BlockHeight::value)
        .ok_or_else(|| eyre!("derive store materialized no block summary"))
}

fn derive_status(derive_store: &DeriveStore) -> Result<DeriveStatus> {
    let bytes = derive_store
        .get_derive_status()?
        .ok_or_else(|| eyre!("derive status record missing"))?;
    DeriveStatus::decode(bytes.as_slice()).map_err(|error| eyre!("decode derive status: {error}"))
}

#[tokio::test]
async fn startup_handoff_returns_with_residual_lag_then_the_tailer_drains_the_rest() -> Result<()> {
    let handoff_lag_blocks: u64 = 6;
    let store_fixture = committed_canonical_store()?;
    let derive_store = open_primary_derive_store_for_canonical(
        store_fixture.tempdir_path(),
        RocksDbResourceBudget::for_local_tests(),
    )?;

    catch_up_derive_store_to_canonical_until_handoff(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(handoff_lag_blocks)?,
    )
    .await?;

    let residual_head = CANONICAL_HEIGHT - u32::try_from(handoff_lag_blocks)?;
    assert_eq!(
        block_summary_head(&derive_store)?,
        residual_head,
        "startup catch-up must hand off within the handoff lag, not drain the whole debt",
    );

    let status = derive_status(&derive_store)?;
    assert_eq!(
        status.health,
        DeriveHealth::CatchingUp as i32,
        "readiness must report the residual as catching-up, not dark",
    );
    assert_eq!(status.lag_blocks, handoff_lag_blocks);
    assert_eq!(status.indexed_height, residual_head);

    catch_up_derive_store_to_canonical(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(handoff_lag_blocks)?,
    )
    .await?;
    assert_eq!(
        block_summary_head(&derive_store)?,
        CANONICAL_HEIGHT,
        "the always-on tailer path must drain the residual to the canonical tip",
    );

    Ok(())
}

#[tokio::test]
async fn startup_handoff_returns_immediately_when_already_within_the_handoff_lag() -> Result<()> {
    let store_fixture = committed_canonical_store()?;
    let derive_store = open_primary_derive_store_for_canonical(
        store_fixture.tempdir_path(),
        RocksDbResourceBudget::for_local_tests(),
    )?;

    catch_up_derive_store_to_canonical_until_handoff(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(u64::from(CANONICAL_HEIGHT))?,
    )
    .await?;

    assert!(
        derive_store
            .last_materialized_height_ascending(BLOCK_SUMMARY_COLUMN_FAMILY)?
            .is_none(),
        "a debt already within the handoff lag must hand off without synchronous replay",
    );

    Ok(())
}

#[tokio::test]
async fn ranking_bootstrap_defers_when_replay_trails_the_event_tail() -> Result<()> {
    let handoff_lag_blocks = u64::from(CANONICAL_HEIGHT - FIRST_EPOCH_TIP);
    let store_fixture = committed_two_epoch_store()?;
    let derive_store = open_primary_derive_store_for_canonical(
        store_fixture.tempdir_path(),
        RocksDbResourceBudget::for_local_tests(),
    )?;

    catch_up_derive_store_to_canonical_until_handoff(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(handoff_lag_blocks)?,
    )
    .await?;
    assert_eq!(
        block_summary_head(&derive_store)?,
        FIRST_EPOCH_TIP,
        "the bounded catch-up must finish the first event and leave the second residual",
    );

    let outcome =
        bootstrap_transparent_address_ranking(store_fixture.chain_store(), &derive_store).await?;
    assert_eq!(
        outcome,
        TransparentAddressRankingBootstrapOutcome::ChainNotReady,
        "residual replay lag must defer the ranking, not fail the open-storage phase",
    );

    Ok(())
}

#[tokio::test]
async fn startup_handoff_resumes_cleanly_across_restarts() -> Result<()> {
    let store_fixture = committed_canonical_store()?;
    let derive_store = open_primary_derive_store_for_canonical(
        store_fixture.tempdir_path(),
        RocksDbResourceBudget::for_local_tests(),
    )?;

    catch_up_derive_store_to_canonical_until_handoff(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(12)?,
    )
    .await?;
    assert_eq!(block_summary_head(&derive_store)?, CANONICAL_HEIGHT - 12);

    catch_up_derive_store_to_canonical_until_handoff(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(4)?,
    )
    .await?;
    assert_eq!(
        block_summary_head(&derive_store)?,
        CANONICAL_HEIGHT - 4,
        "a later boot must resume from the persisted position without losing progress",
    );

    catch_up_derive_store_to_canonical(
        store_fixture.chain_store(),
        &derive_store,
        derive_config(4)?,
    )
    .await?;
    assert_eq!(block_summary_head(&derive_store)?, CANONICAL_HEIGHT);

    Ok(())
}
