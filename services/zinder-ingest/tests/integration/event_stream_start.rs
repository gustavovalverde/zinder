#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

//! `IngestControl.ChainEvents` start-position contract: `live_tail`,
//! `earliest_retained`, and `after_cursor` resolution over the wire.

use std::time::Duration;

use eyre::Result;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_ingest::IngestControlGrpcAdapter;
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{self, ChainEventStreamFamily, ChainEventsRequest},
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, ChainStoreOptions,
    EventStreamStartPosition, PrimaryChainStore, ReorgWindowChange, StreamCursorTokenV1,
    event_stream_start_message,
};
use zinder_testkit::encode_fixture_block_replay;

/// Events committed before a `live_tail` subscribe are skipped; commits after
/// it are delivered in sequence order.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_live_tail_delivers_only_post_subscribe_events() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    commit_synthetic_epoch(&store, 1, 1)?;

    let listen_addr = spawn_ingest_control(store.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::LiveTail,
            )),
            family: ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();

    commit_synthetic_epoch(&store, 2, 2)?;
    let first_event = next_chain_envelope(&mut event_stream).await?;
    assert_eq!(first_event.event_sequence, 2);

    commit_synthetic_epoch(&store, 3, 3)?;
    let second_event = next_chain_envelope(&mut event_stream).await?;
    assert_eq!(second_event.event_sequence, 3);

    Ok(())
}

/// `live_tail` on the safe family delivers post-subscribe events on
/// safe-family cursors.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_live_tail_serves_the_safe_family() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    commit_synthetic_epoch(&store, 1, 1)?;

    let listen_addr = spawn_ingest_control(store.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::LiveTail,
            )),
            family: ChainEventStreamFamily::Safe as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();

    commit_synthetic_epoch(&store, 2, 2)?;
    let first_event = next_chain_envelope(&mut event_stream).await?;
    assert_eq!(first_event.event_sequence, 2);
    assert_eq!(first_event.cursor[49], 0x1);

    Ok(())
}

/// `earliest_retained` replays from the retention floor, not from sequence 1.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_earliest_retained_replays_from_the_retention_floor() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    for height in 1..=3 {
        commit_synthetic_epoch(&store, u64::from(height), height)?;
    }
    let report = store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_200_003))?;
    assert_eq!(report.oldest_retained_sequence, Some(3));

    let listen_addr = spawn_ingest_control(store).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::EarliestRetained,
            )),
            family: ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();

    let first_event = next_chain_envelope(&mut event_stream).await?;
    assert_eq!(first_event.event_sequence, 3);

    Ok(())
}

/// A request without a start message, and one whose `position` oneof is
/// unset, are both rejected with `INVALID_ARGUMENT`.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_rejects_unset_start() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    commit_synthetic_epoch(&store, 1, 1)?;

    let listen_addr = spawn_ingest_control(store).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    for start in [None, Some(wallet::EventStreamStart { position: None })] {
        let outcome = client
            .chain_events(ChainEventsRequest {
                start,
                family: ChainEventStreamFamily::Tip as i32,
                address_filter: Vec::new(),
            })
            .await;
        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("expected unset-start rejection"))?;
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    Ok(())
}

/// A non-default request family that disagrees with the cursor's encoded
/// family is rejected with `INVALID_ARGUMENT`.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_rejects_after_cursor_family_mismatch() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let commit_outcome = commit_synthetic_epoch(&store, 1, 1)?;
    let tip_cursor = commit_outcome.event_envelope.cursor;

    let listen_addr = spawn_ingest_control(store).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let outcome = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::AfterCursor(tip_cursor),
            )),
            family: ChainEventStreamFamily::Safe as i32,
            address_filter: Vec::new(),
        })
        .await;

    let status = outcome
        .err()
        .ok_or_else(|| eyre::eyre!("expected family-mismatch rejection"))?;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// An `after_cursor` start whose branch was reorged out and whose event row
/// is pruned self-heals: the first streamed envelope is a synthetic
/// `ChainReorged` covering the reverted range.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_self_heals_reorged_out_cursor_past_retention() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let pre_reorg_cursor = commit_reorgable_chain_and_cursor(&store)?;
    commit_height_two_reorg(&store)?;
    let report = store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_300_000))?;
    assert_eq!(report.oldest_retained_sequence, Some(3));

    let listen_addr = spawn_ingest_control(store).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::AfterCursor(pre_reorg_cursor),
            )),
            family: ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();

    let first_event = next_chain_envelope(&mut event_stream).await?;
    let wallet::chain_event_envelope::Event::ChainReorged(reorged) = first_event
        .event
        .ok_or_else(|| eyre::eyre!("first envelope event missing"))?
    else {
        return Err(eyre::eyre!("expected a synthetic ChainReorged first"));
    };
    let reverted = reorged
        .reverted
        .ok_or_else(|| eyre::eyre!("ChainReorged carries no reverted range"))?;
    assert_eq!(reverted.start_height, 2);
    assert_eq!(reverted.end_height, 2);

    Ok(())
}

/// An `after_cursor` start on a still-canonical branch whose event row is
/// pruned degrades with the typed `CHAIN_EVENT_CURSOR_EXPIRED` failure.
#[tokio::test(flavor = "multi_thread")]
async fn chain_events_expired_cursor_returns_failed_precondition() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let first_commit = commit_synthetic_epoch(&store, 1, 1)?;
    for height in 2..=3 {
        commit_synthetic_epoch(&store, u64::from(height), height)?;
    }
    store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_200_003))?;

    let listen_addr = spawn_ingest_control(store).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .chain_events(ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::AfterCursor(first_commit.event_envelope.cursor),
            )),
            family: ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        })
        .await?
        .into_inner();

    let stream_outcome = tokio::time::timeout(Duration::from_secs(2), event_stream.next())
        .await?
        .ok_or_else(|| eyre::eyre!("stream closed before cursor expiry"))?;
    let status = match stream_outcome {
        Ok(envelope) => return Err(eyre::eyre!("expected cursor expiry, got {envelope:?}")),
        Err(status) => status,
    };
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("cursor expired"),
        "expected expiry message, got {:?}",
        status.message()
    );

    Ok(())
}

fn commit_synthetic_epoch(
    store: &PrimaryChainStore,
    chain_epoch_id: u64,
    height: u32,
) -> Result<zinder_store::ChainEpochCommitOutcome> {
    let (chain_epoch, block, compact_block) =
        synthetic_epoch_with_settled_tip(chain_epoch_id, height, height, block_hash(height));
    let replay_envelope = encode_fixture_block_replay(&block, &[]);
    Ok(store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay_envelope],
        vec![compact_block],
    ))?)
}

/// Commits a two-block chain under settled tip 1 and returns the height-2
/// cursor, whose locator lets a reorg of height 2 resolve the fork at
/// height 1.
fn commit_reorgable_chain_and_cursor(store: &PrimaryChainStore) -> Result<StreamCursorTokenV1> {
    commit_synthetic_epoch(store, 1, 1)?;
    let (second_epoch, second_block, second_compact_block) =
        synthetic_epoch_with_settled_tip(2, 2, 1, block_hash(2));
    let second_replay_envelope = encode_fixture_block_replay(&second_block, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_replay_envelope],
        vec![second_compact_block],
    ))?;

    let page = store.chain_event_history(zinder_store::ChainEventHistoryRequest::new(
        None,
        std::num::NonZeroU32::new(2).ok_or_else(|| eyre::eyre!("invalid max events"))?,
    ))?;
    Ok(page
        .get(1)
        .ok_or_else(|| eyre::eyre!("expected a height-2 event"))?
        .cursor
        .clone())
}

fn commit_height_two_reorg(store: &PrimaryChainStore) -> Result<()> {
    let (mut replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch_with_settled_tip(3, 2, 1, block_hash(20));
    replacement_epoch.created_at = UnixTimestampMillis::new(1_774_668_300_000);
    let replacement_replay_envelope = encode_fixture_block_replay(&replacement_block, &[]);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_replay_envelope],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;
    Ok(())
}

fn synthetic_epoch_with_settled_tip(
    chain_epoch_id: u64,
    height: u32,
    settled_tip_height: u32,
    source_hash: BlockHash,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            visible_tip_height: block_height,
            visible_tip_hash: source_hash,
            settled_tip_height: BlockHeight::new(settled_tip_height),
            settled_tip_hash: block_hash(settled_tip_height),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_200_000 + u64::from(height)),
        },
        BlockHeaderArtifact::new(
            block_height,
            source_hash,
            parent_hash,
            [0; 32],
            [0; 32],
            0,
            0,
            [0; 32],
            0,
            16,
        ),
        CompactBlockArtifact::new(
            block_height,
            source_hash,
            format!("compact-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
    )
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}

async fn spawn_ingest_control(store: PrimaryChainStore) -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let adapter = IngestControlGrpcAdapter::new(
        Network::ZcashRegtest,
        store,
        zinder_runtime::Readiness::default(),
    );
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                cancel.cancelled_owned(),
            )
            .await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(listen_addr)
}

async fn next_chain_envelope<S>(stream: &mut S) -> Result<wallet::ChainEventEnvelope>
where
    S: tokio_stream::Stream<Item = std::result::Result<wallet::ChainEventEnvelope, tonic::Status>>
        + Unpin,
{
    let stream_outcome = tokio::time::timeout(Duration::from_secs(5), stream.next()).await?;
    let envelope_outcome = stream_outcome.ok_or_else(|| eyre::eyre!("event stream closed"))?;
    Ok(envelope_outcome?)
}
