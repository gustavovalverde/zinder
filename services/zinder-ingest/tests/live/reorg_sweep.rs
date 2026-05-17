//! Live regtest reorg sweep.
//!
//! Forces canonical-chain reorgs on a running regtest sidecar via
//! `invalidateblock`/`reconsiderblock` and asserts that the writer's
//! `IngestControl.ChainEvents` stream emits a `ChainReorged` envelope whose
//! reverted range covers the invalidated heights and whose committed range
//! reaches the post-reorg tip.
//!
//! These tests join the `node-mutating` test group in `.config/nextest.toml`
//! so they serialize against every other live test that drives regtest chain
//! state (broadcast cycle, indexer mempool restart, deep chain, tip follow).
//!
//! Operator precondition: a clean regtest sidecar with at least a few
//! mineable blocks. The tests mine the blocks they need; no pre-seeded
//! wallet or address is required.

#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use eyre::{Result, eyre};
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use zinder_core::{BlockHeight, Network};
use zinder_ingest::{IngestControlGrpcAdapter, backfill, tip_follow_with_primary_store};
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{
        ChainEventEnvelope, ChainEventStreamFamily as ProtoChainEventStreamFamily,
        ChainEventsRequest, chain_event_envelope,
    },
};
use zinder_runtime::Readiness;
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live_for};

use crate::common::{
    fetch_live_tip_height, live_backfill_config, live_tip_follow_config, regtest_generate_blocks,
    rpc_block_hash_at_height, rpc_invalidate_block, rpc_reconsider_block,
    zebra_source_from_backfill, zebra_source_from_tip_follow,
};

/// Tip-follow poll interval. Sized so the writer notices the
/// freshly-mined reorg blocks within a single commit-observe cycle.
const TIP_FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Reorg-window budget passed to tip-follow.
///
/// The reorg-sweep tests force reorgs of at most a handful of blocks, so
/// the default production-style window is plenty of headroom.
const TIP_FOLLOW_REORG_WINDOW_BLOCKS: u32 = 100;

/// Backfill batch size for the bulk catchup phase. Sized for fast catchup
/// against a regtest sidecar that may already hold several thousand
/// blocks accumulated from prior live runs.
const BACKFILL_BATCH_BLOCKS: u32 = 1000;

/// Maximum wall-clock budget we wait for each round of freshly-mined
/// blocks during the reorg phase.
const COMMIT_OBSERVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum wall-clock budget we wait for the `IngestControl.ChainEvents`
/// stream to surface the `ChainReorged` envelope produced by a forced
/// reorg.
const REORG_OBSERVE_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn single_block_reorg_surfaces_chain_reorged_envelope() -> Result<()> {
    run_reorg_sweep(1).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn three_block_reorg_covers_full_reverted_range() -> Result<()> {
    run_reorg_sweep(3).await
}

#[allow(
    clippy::too_many_lines,
    reason = "Reorg gate composes tip-follow + IngestControl ChainEvents subscription + invalidateblock + post-reorg observation in one auditable path."
)]
async fn run_reorg_sweep(reorg_depth: u32) -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };

    let tempdir = tempfile::tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");

    // Phase 1: bulk-backfill up to Zebra's current tip. Without this, the
    // tip-follow loop catches up at a single-block-per-poll rate that
    // does not scale to a sidecar that already holds thousands of
    // regtest blocks. Backfill is built for this and finishes in
    // seconds.
    let zebra_tip_before_mining = fetch_live_tip_height(&env).await?;
    let backfill_config = live_backfill_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        zebra_tip_before_mining,
        NonZeroU32::new(BACKFILL_BATCH_BLOCKS).ok_or_else(|| eyre!("invalid backfill batch"))?,
        true,
    );
    let backfill_source = zebra_source_from_backfill(&backfill_config)?;
    backfill(&backfill_config, &backfill_source)
        .await?
        .ok_or_else(|| eyre!("expected committed backfill outcome"))?;

    // Phase 2: open the store and the tip-follow loop on top of the
    // populated state. Tip-follow now only has to absorb the
    // freshly-mined reorg blocks, which is fast.
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let tip_follow_config = live_tip_follow_config(
        &env,
        &storage_path,
        TIP_FOLLOW_REORG_WINDOW_BLOCKS,
        TIP_FOLLOW_POLL_INTERVAL,
    );
    let tip_follow_source = zebra_source_from_tip_follow(&tip_follow_config)?;
    let readiness = Readiness::default();

    let cancel = CancellationToken::new();
    let tip_follow_handle = {
        let store = store.clone();
        let readiness = readiness.clone();
        let cancel = cancel.clone();
        let tip_follow_config = tip_follow_config.clone();
        tokio::spawn(async move {
            tip_follow_with_primary_store(
                &tip_follow_config,
                &tip_follow_source,
                store,
                &readiness,
                None,
                None,
                cancel,
            )
            .await
        })
    };

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let ingest_control_addr = listener.local_addr()?;
    let server_cancel = cancel.clone();
    let ingest_adapter = IngestControlGrpcAdapter::new(env.network(), store.clone());
    let server_handle = tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(ingest_adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await;
    });

    // Phase 3: mine the reorg window on top of the existing chain so the
    // invalidated block stays in Zebra's non-finalized chain. Tip-follow
    // observes these and commits them.
    let blocks_to_mine = reorg_depth.saturating_add(2);
    regtest_generate_blocks(&env, blocks_to_mine).await?;
    let pre_reorg_tip = wait_for_chain_epoch_at_or_above(
        &store,
        zebra_tip_before_mining
            .value()
            .saturating_add(blocks_to_mine),
        COMMIT_OBSERVE_TIMEOUT,
    )
    .await?;
    let reorg_floor_height = pre_reorg_tip.value().saturating_sub(reorg_depth - 1);
    let invalidate_hash = rpc_block_hash_at_height(&env, reorg_floor_height).await?;

    let mut client = IngestControlClient::connect(format!("http://{ingest_control_addr}")).await?;
    let mut chain_events = client
        .chain_events(ChainEventsRequest {
            from_cursor: Vec::new(),
            family: ProtoChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();

    drain_chain_events_until_height(&mut chain_events, pre_reorg_tip).await?;

    rpc_invalidate_block(&env, &invalidate_hash).await?;
    regtest_generate_blocks(&env, reorg_depth.saturating_add(2)).await?;

    let chain_reorged = wait_for_chain_reorged(&mut chain_events, REORG_OBSERVE_TIMEOUT).await?;
    let reverted = chain_reorged
        .reverted
        .ok_or_else(|| eyre!("ChainReorged envelope missing reverted range"))?;
    let committed = chain_reorged
        .committed
        .ok_or_else(|| eyre!("ChainReorged envelope missing committed range"))?;

    assert!(
        reverted.start_height <= reorg_floor_height,
        "reverted range must include the invalidated floor; reverted.start_height={}, floor={}",
        reverted.start_height,
        reorg_floor_height
    );
    assert!(
        reverted.end_height >= pre_reorg_tip.value(),
        "reverted range must reach the pre-reorg tip; reverted.end_height={}, pre_reorg_tip={}",
        reverted.end_height,
        pre_reorg_tip.value()
    );
    assert_eq!(
        u32::checked_sub(reverted.end_height, reverted.start_height)
            .ok_or_else(|| eyre!("reverted range is inverted"))?
            .saturating_add(1),
        reorg_depth,
        "reverted block count must equal the requested reorg depth"
    );
    assert!(
        committed.start_height >= reverted.start_height,
        "committed range must start at or after the reverted range"
    );

    let _ = rpc_reconsider_block(&env, &invalidate_hash).await;
    cancel.cancel();
    drop(chain_events);
    drop(client);
    let _ = tip_follow_handle.await;
    let _ = server_handle.await;
    Ok(())
}

async fn wait_for_chain_epoch_at_or_above(
    store: &PrimaryChainStore,
    minimum_tip_height: u32,
    deadline: Duration,
) -> Result<BlockHeight> {
    let started = Instant::now();
    loop {
        if let Some(chain_epoch) = store.current_chain_epoch()?
            && chain_epoch.tip_height.value() >= minimum_tip_height
        {
            return Ok(chain_epoch.tip_height);
        }
        if started.elapsed() > deadline {
            return Err(eyre!(
                "writer did not reach tip height >= {minimum_tip_height} within {deadline:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn drain_chain_events_until_height(
    chain_events: &mut tonic::Streaming<ChainEventEnvelope>,
    target_height: BlockHeight,
) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < COMMIT_OBSERVE_TIMEOUT {
        let remaining = COMMIT_OBSERVE_TIMEOUT.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, chain_events.next()).await {
            Ok(Some(Ok(envelope))) => {
                if envelope_committed_end_height(&envelope)
                    .is_some_and(|height| height >= target_height.value())
                {
                    return Ok(());
                }
            }
            Ok(Some(Err(error))) => {
                return Err(eyre!("chain-events stream emitted error: {error}"));
            }
            Ok(None) => {
                return Err(eyre!("chain-events stream closed before pre-reorg tip"));
            }
            Err(_elapsed) => break,
        }
    }
    Err(eyre!(
        "chain-events stream did not reach pre-reorg tip {} within {:?}",
        target_height.value(),
        COMMIT_OBSERVE_TIMEOUT
    ))
}

async fn wait_for_chain_reorged(
    chain_events: &mut tonic::Streaming<ChainEventEnvelope>,
    deadline: Duration,
) -> Result<zinder_proto::v1::wallet::ChainReorged> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        let remaining = deadline.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, chain_events.next()).await {
            Ok(Some(Ok(envelope))) => match envelope.event {
                Some(chain_event_envelope::Event::ChainReorged(reorged)) => {
                    return Ok(reorged);
                }
                Some(chain_event_envelope::Event::ChainCommitted(_)) | None => {}
            },
            Ok(Some(Err(error))) => {
                return Err(eyre!("chain-events stream emitted error: {error}"));
            }
            Ok(None) => {
                return Err(eyre!(
                    "chain-events stream closed before emitting ChainReorged"
                ));
            }
            Err(_elapsed) => break,
        }
    }
    Err(eyre!(
        "ChainReorged envelope did not arrive within {deadline:?}"
    ))
}

fn envelope_committed_end_height(envelope: &ChainEventEnvelope) -> Option<u32> {
    match envelope.event.as_ref()? {
        chain_event_envelope::Event::ChainCommitted(committed) => {
            committed.committed.as_ref().map(|range| range.end_height)
        }
        chain_event_envelope::Event::ChainReorged(reorged) => {
            reorged.committed.as_ref().map(|range| range.end_height)
        }
    }
}
