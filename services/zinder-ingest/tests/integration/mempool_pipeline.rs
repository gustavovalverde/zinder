#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;

use eyre::Result;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{Code, transport::Server};
use zinder_core::wire::{encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex};
use zinder_core::{
    AuthDigest, BlockHash, MempoolEntry, MempoolEvictionReason, Network, RawTransactionBytes,
    TransactionId, TransparentAddressScriptHash, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, UnixTimestampMillis,
};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolApplyOutcome, MempoolIndex};
use zinder_proto::v1::{
    ingest::{MempoolTransactionRequest, ingest_control_client::IngestControlClient},
    wallet::{
        MempoolEventStreamFamily, MempoolEventsRequest,
        MempoolSnapshotRequest as ControlMempoolSnapshotRequest, mempool_event_envelope,
        transaction_location,
    },
};
use zinder_store::{
    DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS, EventStreamStartPosition, MempoolEvent,
    MempoolEventEnvelope, MempoolEventHistoryRequest, MempoolEventPosition, PrimaryChainStore,
    StoreError, StreamCursorTokenV1, event_stream_start_message,
};
use zinder_testkit::StoreFixture;

fn append_mempool_event(
    store: &PrimaryChainStore,
    event: MempoolEvent,
) -> Result<MempoolEventEnvelope> {
    Ok(store.append_mempool_event(event, UnixTimestampMillis::now())?)
}

fn earliest_start() -> zinder_proto::v1::wallet::EventStreamStart {
    event_stream_start_message(&EventStreamStartPosition::EarliestRetained)
}

fn after_cursor_start(cursor: &StreamCursorTokenV1) -> zinder_proto::v1::wallet::EventStreamStart {
    event_stream_start_message(&EventStreamStartPosition::AfterCursor(cursor.clone()))
}

fn synthetic_applied_position(transaction_id: TransactionId) -> MempoolEventPosition {
    MempoolEventPosition {
        event_sequence: 1,
        transaction_id,
    }
}

fn retained_mempool_event_count(store: &PrimaryChainStore) -> Result<u64> {
    Ok(store.mempool_event_retention_report()?.retained_event_count)
}

fn read_mempool_envelopes(
    store: &PrimaryChainStore,
    cursor: Option<&StreamCursorTokenV1>,
) -> std::result::Result<Vec<MempoolEventEnvelope>, StoreError> {
    store.mempool_event_history(MempoolEventHistoryRequest::new(
        cursor,
        DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS,
    ))
}

/// End-to-end wire path: a hydrated mempool entry written through the
/// `MempoolIndex` and `MempoolEventLog` is observable via the
/// `IngestControl` `MempoolSnapshot` and `MempoolEvents` RPCs.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_mempool_snapshot_and_events() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;

    let mempool_index = MempoolIndex::new();
    let store = store_fixture.chain_store().clone();

    let admitted = synthetic_entry(0xAA, chain_epoch);
    let added_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: admitted.clone(),
        },
    )?;
    assert_eq!(
        mempool_index.apply_added(admitted.clone(), added_envelope.position()),
        MempoolApplyOutcome::Applied
    );
    let _invalidated_envelope = append_mempool_event(
        &store,
        MempoolEvent::Invalidated {
            transaction_id: admitted.transaction_id,
            reason: MempoolEvictionReason::Conflict,
        },
    )?;
    assert_eq!(retained_mempool_event_count(&store)?, 2);

    let listen_addr = spawn_ingest_control(store, mempool_index.clone()).await?;

    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    let snapshot = client
        .mempool_snapshot(ControlMempoolSnapshotRequest {
            max_entries: 0,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    let chain_epoch_in_response = snapshot
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| eyre::eyre!("snapshot.chain_view.chain_epoch is missing"))?;
    assert_eq!(chain_epoch_in_response.network_name, "zcash-regtest");
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(
        snapshot.events_resume_cursor,
        added_envelope.cursor.as_bytes().to_vec()
    );
    let observed_entry = snapshot
        .entries
        .first()
        .ok_or_else(|| eyre::eyre!("snapshot has no entry"))?;
    assert_eq!(
        observed_entry.transaction_id,
        encode_rpc_transaction_id_hex(admitted.transaction_id)
    );

    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(earliest_start()),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();
    let first_event = next_envelope(&mut event_stream).await?;
    assert_eq!(first_event.event_sequence, 1);
    assert!(matches!(
        first_event
            .event
            .ok_or_else(|| eyre::eyre!("first envelope event missing"))?,
        mempool_event_envelope::Event::Added(_)
    ));

    let second_event = next_envelope(&mut event_stream).await?;
    assert_eq!(second_event.event_sequence, 2);
    assert!(matches!(
        second_event
            .event
            .ok_or_else(|| eyre::eyre!("second envelope event missing"))?,
        mempool_event_envelope::Event::Invalidated(_)
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_mempool_transaction_by_id() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();
    let mempool_index = MempoolIndex::new();
    let entry = synthetic_entry(0xAB, chain_epoch);
    let added_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry.clone(),
        },
    )?;
    assert_eq!(
        mempool_index.apply_added(entry.clone(), added_envelope.position()),
        MempoolApplyOutcome::Applied
    );

    let listen_addr = spawn_ingest_control(store, mempool_index).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let response = client
        .mempool_transaction(MempoolTransactionRequest {
            transaction_id: encode_rpc_transaction_id_hex(entry.transaction_id),
        })
        .await?
        .into_inner();
    let response_epoch = response
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| eyre::eyre!("response.chain_view.chain_epoch is missing"))?;
    assert_eq!(response_epoch.chain_epoch_id, chain_epoch.id.value());
    let location = response
        .location
        .and_then(|location| location.location)
        .ok_or_else(|| eyre::eyre!("response.location is missing"))?;
    let transaction_location::Location::InMempool(mempool_transaction) = location else {
        return Err(eyre::eyre!("expected in-mempool transaction location"));
    };
    assert_eq!(
        mempool_transaction.payload_bytes,
        entry.raw_transaction_bytes.as_slice()
    );
    assert_eq!(mempool_transaction.first_seen_unix_seconds, 1_700_000_000);

    let Err(missing_status) = client
        .mempool_transaction(MempoolTransactionRequest {
            transaction_id: encode_rpc_transaction_id_hex(TransactionId::from_bytes([0xCD; 32])),
        })
        .await
    else {
        return Err(eyre::eyre!("unknown transaction unexpectedly resolved"));
    };
    assert_eq!(missing_status.code(), Code::NotFound);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_pages_mempool_snapshot_from_live_index() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let store = store_fixture.chain_store().clone();

    let entry_one = synthetic_entry(0x01, chain_epoch);
    let entry_two = synthetic_entry(0x02, chain_epoch);
    let one_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry_one.clone(),
        },
    )?;
    assert_eq!(
        mempool_index.apply_added(entry_one, one_envelope.position()),
        MempoolApplyOutcome::Applied
    );
    let two_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry_two.clone(),
        },
    )?;
    assert_eq!(
        mempool_index.apply_added(entry_two, two_envelope.position()),
        MempoolApplyOutcome::Applied
    );

    let listen_addr = spawn_ingest_control(store, mempool_index).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    let first_page = client
        .mempool_snapshot(ControlMempoolSnapshotRequest {
            max_entries: 1,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    assert_eq!(first_page.entries.len(), 1);
    assert_eq!(
        first_page.events_resume_cursor,
        two_envelope.cursor.as_bytes().to_vec()
    );
    assert!(
        first_page.snapshot_age_millis < 60_000,
        "snapshot age should come from the live mempool index, got {}ms",
        first_page.snapshot_age_millis
    );
    assert!(
        !first_page.next_cursor.is_empty(),
        "first page should carry a next cursor"
    );

    let second_page = client
        .mempool_snapshot(ControlMempoolSnapshotRequest {
            max_entries: 1,
            from_cursor: first_page.next_cursor,
        })
        .await?
        .into_inner();
    assert_eq!(second_page.entries.len(), 1);
    assert_eq!(
        second_page.events_resume_cursor, first_page.events_resume_cursor,
        "every page of one walk returns the identical events-resume cursor"
    );
    assert!(
        second_page.next_cursor.is_empty(),
        "second page should finish the two-entry snapshot"
    );
    Ok(())
}

/// A `MempoolEvents` request without a start message, and one whose
/// `position` oneof is unset, are both rejected with `INVALID_ARGUMENT`.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_mempool_events_reject_unset_start() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let listen_addr =
        spawn_ingest_control(store_fixture.chain_store().clone(), MempoolIndex::new()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    for start in [
        None,
        Some(zinder_proto::v1::wallet::EventStreamStart { position: None }),
    ] {
        let outcome = client
            .mempool_events(MempoolEventsRequest {
                start,
                family: MempoolEventStreamFamily::Mempool as i32,
            })
            .await;
        let status = outcome
            .err()
            .ok_or_else(|| eyre::eyre!("expected unset-start rejection"))?;
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    Ok(())
}

/// Events appended before a `live_tail` subscribe are skipped; appends after
/// it are delivered in sequence order.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_mempool_events_live_tail_delivers_only_post_subscribe_events() -> Result<()>
{
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();
    let _pre_subscribe = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: synthetic_entry(0x01, chain_epoch),
        },
    )?;

    let listen_addr = spawn_ingest_control(store.clone(), MempoolIndex::new()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::LiveTail,
            )),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();

    let _second = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: synthetic_entry(0x02, chain_epoch),
        },
    )?;
    let first_delivered = next_envelope(&mut event_stream).await?;
    assert_eq!(first_delivered.event_sequence, 2);

    let _third = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: synthetic_entry(0x03, chain_epoch),
        },
    )?;
    let second_delivered = next_envelope(&mut event_stream).await?;
    assert_eq!(second_delivered.event_sequence, 3);

    Ok(())
}

/// `earliest_retained` replays from the retention floor after pruning, not
/// from sequence 1.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_mempool_events_earliest_retained_replays_from_retention_floor() -> Result<()>
{
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();

    let _mined = append_mempool_event(
        &store,
        MempoolEvent::Mined {
            transaction_id: TransactionId::from_bytes([0xE1; 32]),
            mined_height: zinder_core::BlockHeight::new(101),
            block_hash: BlockHash::from_bytes([0xE1; 32]),
        },
    )?;
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    let _recent = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: synthetic_entry(0xE2, chain_epoch),
        },
    )?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let report = store.prune_mempool_events_before(
        zinder_core::UnixTimestampMillis::new(now.saturating_add(80)),
        zinder_store::MempoolEventRetentionConfig::new(Some(Duration::from_millis(50)), None),
    )?;
    assert!(
        report.pruned_mined_count >= 1,
        "expected mined envelope to be pruned; report: {report:?}"
    );

    let listen_addr = spawn_ingest_control(store, MempoolIndex::new()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(earliest_start()),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();

    let first_delivered = next_envelope(&mut event_stream).await?;
    assert_eq!(first_delivered.event_sequence, 2);

    Ok(())
}

/// Snapshot-anchored resume exactness across a paged walk.
///
/// Every page re-mints one identical `events_resume_cursor` even when events
/// land mid-walk, and replaying `MempoolEvents` from it delivers exactly the
/// events past the first-page anchor: mid-walk events appear, pre-anchor
/// events do not repeat.
#[allow(
    clippy::too_many_lines,
    reason = "Walk → mid-walk append → walk continuation → replay is one linear contract; splitting it would hide the anchor invariant under test."
)]
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_snapshot_resume_cursor_is_stable_and_replays_exactly() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let store = store_fixture.chain_store().clone();

    let mut anchor_envelope = None;
    for transaction_id_byte in [0x01, 0x02, 0x03] {
        let entry = synthetic_entry(transaction_id_byte, chain_epoch);
        let envelope = append_mempool_event(
            &store,
            MempoolEvent::Added {
                entry: entry.clone(),
            },
        )?;
        assert_eq!(
            mempool_index.apply_added(entry, envelope.position()),
            MempoolApplyOutcome::Applied
        );
        anchor_envelope = Some(envelope);
    }
    let anchor_envelope =
        anchor_envelope.ok_or_else(|| eyre::eyre!("no mempool event was appended"))?;

    let listen_addr = spawn_ingest_control(store.clone(), mempool_index.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    let first_page = client
        .mempool_snapshot(ControlMempoolSnapshotRequest {
            max_entries: 1,
            from_cursor: Vec::new(),
        })
        .await?
        .into_inner();
    let resume_cursor = first_page.events_resume_cursor.clone();
    assert_eq!(resume_cursor, anchor_envelope.cursor.as_bytes().to_vec());

    // Mid-walk admission: the event lands after the first page anchored the
    // walk, so it must replay from the resume cursor.
    let mid_walk_entry = synthetic_entry(0x04, chain_epoch);
    let mid_walk_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: mid_walk_entry.clone(),
        },
    )?;
    assert_eq!(
        mempool_index.apply_added(mid_walk_entry, mid_walk_envelope.position()),
        MempoolApplyOutcome::Applied
    );

    let mut next_cursor = first_page.next_cursor;
    let mut walked_entry_count = first_page.entries.len();
    while !next_cursor.is_empty() {
        let later_page = client
            .mempool_snapshot(ControlMempoolSnapshotRequest {
                max_entries: 1,
                from_cursor: next_cursor,
            })
            .await?
            .into_inner();
        assert_eq!(
            later_page.events_resume_cursor, resume_cursor,
            "every page of one walk returns the identical events-resume cursor"
        );
        walked_entry_count += later_page.entries.len();
        next_cursor = later_page.next_cursor;
    }
    assert_eq!(walked_entry_count, 4);

    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(after_cursor_start(&StreamCursorTokenV1::from_bytes(
                resume_cursor,
            ))),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();
    let replayed = next_envelope(&mut event_stream).await?;
    assert_eq!(
        replayed.event_sequence, mid_walk_envelope.event_sequence,
        "replay starts at the first event past the anchor, with no pre-anchor duplicates"
    );
    let no_more = tokio::time::timeout(Duration::from_millis(500), event_stream.next()).await;
    assert!(
        no_more.is_err(),
        "no further events exist past the mid-walk admission; got {no_more:?}"
    );

    Ok(())
}

/// A snapshot cursor anchored ahead of the writer's applied mempool-event
/// sequence names an event this writer never emitted; the walk is rejected
/// as expired.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_rejects_snapshot_cursor_anchored_ahead_of_applied_events() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let store = store_fixture.chain_store().clone();
    let ahead_cursor = store.encode_snapshot_page_cursor(
        Some(MempoolEventPosition {
            event_sequence: 7,
            transaction_id: TransactionId::from_bytes([0xAA; 32]),
        }),
        TransactionId::from_bytes([0xA0; 32]),
    )?;

    let listen_addr = spawn_ingest_control(store, MempoolIndex::new()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let status = match client
        .mempool_snapshot(ControlMempoolSnapshotRequest {
            max_entries: 1,
            from_cursor: ahead_cursor.as_bytes().to_vec(),
        })
        .await
    {
        Ok(response) => {
            return Err(eyre::eyre!(
                "expected expired-cursor rejection, got {response:?}"
            ));
        }
        Err(status) => status,
    };
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);

    Ok(())
}

/// Bearer-token auth: a server configured with a token rejects requests
/// that lack the header, rejects requests with the wrong token, and accepts
/// requests carrying the matching token.
///
/// Matching-token requests pass through the client interceptor.
///
/// This test pins the contract that `with_bearer_token` actually wires the
/// server-side interceptor; the unit tests in `zinder-runtime` cover the
/// interceptor in isolation, but a regression here would only surface on a
/// remote deployment otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_bearer_token_rejects_unauthenticated_clients() -> Result<()> {
    use std::str::FromStr as _;
    use tonic::transport::Endpoint;
    use zinder_proto::v1::ingest::WriterStatusRequest;
    use zinder_runtime::{BearerToken, BearerTokenClientInterceptor};

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let mempool_index = MempoolIndex::new();
    let server_token =
        BearerToken::from_str("expected").map_err(|error| eyre::eyre!("token parse: {error}"))?;
    let listen_addr = spawn_ingest_control_with_options(
        store_fixture.chain_store().clone(),
        mempool_index,
        Some(server_token.clone()),
    )
    .await?;

    let unauthenticated_outcome = IngestControlClient::connect(format!("http://{listen_addr}"))
        .await?
        .writer_status(WriterStatusRequest {})
        .await;
    let unauthenticated_status = unauthenticated_outcome
        .err()
        .ok_or_else(|| eyre::eyre!("expected unauthenticated rejection"))?;
    assert_eq!(unauthenticated_status.code(), tonic::Code::Unauthenticated);

    let wrong_token =
        BearerToken::from_str("wrong").map_err(|error| eyre::eyre!("token parse: {error}"))?;
    let wrong_channel = Endpoint::from_shared(format!("http://{listen_addr}"))?
        .connect()
        .await?;
    let wrong_interceptor = BearerTokenClientInterceptor::new(Some(&wrong_token))
        .map_err(|error| eyre::eyre!("interceptor build: {error}"))?;
    let mut wrong_client = IngestControlClient::with_interceptor(wrong_channel, wrong_interceptor);
    let wrong_outcome = wrong_client.writer_status(WriterStatusRequest {}).await;
    let wrong_status = wrong_outcome
        .err()
        .ok_or_else(|| eyre::eyre!("expected wrong-token rejection"))?;
    assert_eq!(wrong_status.code(), tonic::Code::Unauthenticated);

    let correct_channel = Endpoint::from_shared(format!("http://{listen_addr}"))?
        .connect()
        .await?;
    let correct_interceptor = BearerTokenClientInterceptor::new(Some(&server_token))
        .map_err(|error| eyre::eyre!("interceptor build: {error}"))?;
    let mut correct_client =
        IngestControlClient::with_interceptor(correct_channel, correct_interceptor);
    let correct_outcome = correct_client
        .writer_status(WriterStatusRequest {})
        .await?
        .into_inner();
    assert_eq!(correct_outcome.network_name, "zcash-regtest");
    Ok(())
}

/// Open-server default: when no token is configured, an unauthenticated
/// client succeeds.
///
/// This pins the localhost-default deployment story so a future refactor
/// cannot accidentally make auth required by default.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_without_bearer_token_accepts_unauthenticated_clients() -> Result<()> {
    use zinder_proto::v1::ingest::WriterStatusRequest;

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let mempool_index = MempoolIndex::new();
    let listen_addr =
        spawn_ingest_control_with_options(store_fixture.chain_store().clone(), mempool_index, None)
            .await?;

    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let response = client
        .writer_status(WriterStatusRequest {})
        .await?
        .into_inner();
    assert_eq!(response.network_name, "zcash-regtest");
    Ok(())
}

/// Cursor resume: a client that has consumed one envelope can reconnect
/// with that cursor and observe only events that follow.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_mempool_events_resume_strictly_after_cursor() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let store = store_fixture.chain_store().clone();

    let entry_one = synthetic_entry(0x01, chain_epoch);
    let entry_two = synthetic_entry(0x02, chain_epoch);
    let first_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry_one.clone(),
        },
    )?;
    let _ = mempool_index.apply_added(entry_one.clone(), first_envelope.position());
    let second_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry_two.clone(),
        },
    )?;
    let _ = mempool_index.apply_added(entry_two.clone(), second_envelope.position());

    let listen_addr = spawn_ingest_control(store, mempool_index.clone()).await?;

    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(after_cursor_start(&first_envelope.cursor)),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();
    let resumed = next_envelope(&mut event_stream).await?;
    assert_eq!(resumed.event_sequence, 2);
    Ok(())
}

/// Restart durability: appended envelopes survive a writer restart, and a
/// cursor minted before the restart still resolves to events that follow it.
#[tokio::test(flavor = "multi_thread")]
async fn mempool_event_log_resumes_after_writer_restart() -> Result<()> {
    let tempdir = tempfile::TempDir::new()?;
    let storage_path = tempdir.path().to_path_buf();
    let chain_fixture = zinder_testkit::ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let chain_epoch_artifacts = chain_fixture
        .chain_epoch_artifacts(zinder_core::ChainEpochId::new(1))
        .ok_or_else(|| eyre::eyre!("chain fixture has no blocks"))?;
    let committed_chain_epoch = chain_epoch_artifacts.chain_epoch;

    let first_envelope = {
        let store = zinder_store::PrimaryChainStore::open(
            &storage_path,
            zinder_store::ChainStoreOptions::for_local_tests(),
        )?;
        store.commit_chain_epoch(chain_epoch_artifacts)?;
        let entry_one = synthetic_entry(0xAA, committed_chain_epoch);
        let entry_two = synthetic_entry(0xBB, committed_chain_epoch);
        let first = append_mempool_event(&store, MempoolEvent::Added { entry: entry_one })?;
        let _second = append_mempool_event(&store, MempoolEvent::Added { entry: entry_two })?;
        assert_eq!(retained_mempool_event_count(&store)?, 2);
        first
    };

    // Reopen the store; the previous handle has been dropped so the lock is
    // released. Verify the persisted envelopes survive and a cursor from the
    // previous session still resumes correctly.
    let reopened_store = zinder_store::PrimaryChainStore::open(
        &storage_path,
        zinder_store::ChainStoreOptions::for_local_tests(),
    )?;
    let mempool_index = MempoolIndex::new();
    assert_eq!(retained_mempool_event_count(&reopened_store)?, 2);

    let listen_addr = spawn_ingest_control(reopened_store, mempool_index).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(after_cursor_start(&first_envelope.cursor)),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();
    let resumed = next_envelope(&mut event_stream).await?;
    assert_eq!(resumed.event_sequence, 2);
    Ok(())
}

/// Aggressive retention: with a tiny mined window, mined envelopes are
/// pruned by the next retention pass and a cursor pointing at the pruned
/// sequence expires.
///
/// The expired cursor surfaces `MempoolCursorExpired` with the structured
/// floor.
#[tokio::test(flavor = "multi_thread")]
async fn mempool_event_log_prunes_mined_under_short_retention() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();

    let entry_old = synthetic_entry(0xC0, chain_epoch);
    let mined_envelope = append_mempool_event(
        &store,
        MempoolEvent::Mined {
            transaction_id: entry_old.transaction_id,
            mined_height: zinder_core::BlockHeight::new(123),
            block_hash: BlockHash::from_bytes([0xC0; 32]),
        },
    )?;
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    let _entry_recent = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: synthetic_entry(0xC1, chain_epoch),
        },
    )?;

    // Wait until the mined envelope is older than 50ms.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let report = store.prune_mempool_events_before(
        zinder_core::UnixTimestampMillis::new(now.saturating_add(80)),
        zinder_store::MempoolEventRetentionConfig::new(Some(Duration::from_millis(50)), None),
    )?;

    assert!(
        report.pruned_mined_count >= 1,
        "expected mined envelope to be pruned; report: {report:?}"
    );

    // Cursor pointing at the pruned mined envelope should now expire.
    let outcome = read_mempool_envelopes(&store, Some(&mined_envelope.cursor));
    let error = outcome
        .err()
        .ok_or_else(|| eyre::eyre!("expected cursor expired"))?;
    let oldest_retained_sequence = if let StoreError::MempoolEventCursorExpired {
        oldest_retained_sequence,
        ..
    } = &error
    {
        *oldest_retained_sequence
    } else {
        return Err(eyre::eyre!("unexpected error: {error:?}"));
    };
    assert!(oldest_retained_sequence > mined_envelope.event_sequence);
    Ok(())
}

/// `IngestControl.TransparentMempoolOutputsByAddress` returns the outputs
/// of an admitted mempool entry that fund the requested address, and
/// returns an empty list for an address with no mempool footprint.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_transparent_mempool_outputs_by_address() -> Result<()> {
    use zinder_proto::v1::wallet::{
        AddressLookup, TransparentMempoolOutputsByAddressRequest, address_lookup,
    };

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let admitted = synthetic_entry(0xAA, chain_epoch);
    assert_eq!(
        mempool_index.apply_added(
            admitted.clone(),
            synthetic_applied_position(admitted.transaction_id)
        ),
        MempoolApplyOutcome::Applied
    );
    let listen_addr =
        spawn_ingest_control(store_fixture.chain_store().clone(), mempool_index.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    // Synthetic entry funds address script-hash [0xAA; 32].
    let funded_response = client
        .transparent_mempool_outputs_by_address(TransparentMempoolOutputsByAddressRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(vec![0xAA; 32])),
            }),
            max_entries: None,
        })
        .await?
        .into_inner();
    assert_eq!(funded_response.outputs.len(), 1);
    let output = funded_response
        .outputs
        .first()
        .ok_or_else(|| eyre::eyre!("response.outputs is empty"))?;
    assert_eq!(output.address_script_hash, vec![0xAA; 32]);
    assert_eq!(output.value_zat, 1_000);

    // Address with no mempool footprint resolves to an empty list, not an error.
    let unknown_response = client
        .transparent_mempool_outputs_by_address(TransparentMempoolOutputsByAddressRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(vec![0xFF; 32])),
            }),
            max_entries: None,
        })
        .await?
        .into_inner();
    assert!(unknown_response.outputs.is_empty());

    Ok(())
}

/// `IngestControl.TransparentMempoolSpendsByOutpoint` returns the mempool
/// spends that consume the requested outpoints and omits entries for
/// outpoints that are not being spent in the mempool.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_transparent_mempool_spends_by_outpoint() -> Result<()> {
    use zinder_proto::v1::wallet::{OutPoint, TransparentMempoolSpendsByOutpointRequest};

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let admitted = synthetic_entry(0xAA, chain_epoch);
    assert_eq!(
        mempool_index.apply_added(
            admitted.clone(),
            synthetic_applied_position(admitted.transaction_id)
        ),
        MempoolApplyOutcome::Applied
    );
    let listen_addr =
        spawn_ingest_control(store_fixture.chain_store().clone(), mempool_index.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    // Synthetic entry spends outpoint ([0x55; 32], 0); the wire form is the
    // RPC-byte-order hex string of that internal byte pattern (which is
    // identical for an all-0x55 hash). The unknown outpoint in the same
    // batch must produce no entry.
    let spent_response = client
        .transparent_mempool_spends_by_outpoint(TransparentMempoolSpendsByOutpointRequest {
            outpoints: vec![
                OutPoint {
                    transaction_id: "55".repeat(32),
                    output_index: 0,
                },
                OutPoint {
                    transaction_id: "ff".repeat(32),
                    output_index: 7,
                },
            ],
        })
        .await?
        .into_inner();
    assert_eq!(spent_response.spends.len(), 1);
    let spend = &spent_response.spends[0];
    let spent_outpoint = spend
        .spent_outpoint
        .as_ref()
        .ok_or_else(|| eyre::eyre!("expected spent_outpoint on mempool spend"))?;
    assert_eq!(spent_outpoint.transaction_id, "55".repeat(32));
    assert_eq!(spent_outpoint.output_index, 0);
    assert_eq!(
        spend.spending_transaction_id,
        encode_rpc_transaction_id_hex(admitted.transaction_id)
    );

    Ok(())
}

/// `IngestControl.TransparentMempoolOutputsByOutpoint` resolves the outputs of
/// mempool transactions into per-entry prevouts in input order.
///
/// Outpoints that reference unknown transactions or out-of-bounds output
/// indices return `None`.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_transparent_mempool_outputs_by_outpoint() -> Result<()> {
    use zinder_proto::v1::wallet::{OutPoint, TransparentMempoolOutputsByOutpointRequest};

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let admitted = synthetic_entry(0xAB, chain_epoch);
    assert_eq!(
        mempool_index.apply_added(
            admitted.clone(),
            synthetic_applied_position(admitted.transaction_id)
        ),
        MempoolApplyOutcome::Applied
    );
    let listen_addr =
        spawn_ingest_control(store_fixture.chain_store().clone(), mempool_index.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    let known_outpoint = OutPoint {
        transaction_id: encode_rpc_transaction_id_hex(admitted.transaction_id),
        output_index: 0,
    };
    let unknown_outpoint = OutPoint {
        transaction_id: "ff".repeat(32),
        output_index: 0,
    };
    let oob_outpoint = OutPoint {
        transaction_id: encode_rpc_transaction_id_hex(admitted.transaction_id),
        output_index: 99,
    };

    let response = client
        .transparent_mempool_outputs_by_outpoint(TransparentMempoolOutputsByOutpointRequest {
            outpoints: vec![
                known_outpoint.clone(),
                unknown_outpoint.clone(),
                oob_outpoint.clone(),
            ],
        })
        .await?
        .into_inner();

    assert!(response.chain_view.is_some());
    assert_eq!(response.entries.len(), 3);
    let known_prevout = response.entries[0]
        .output
        .as_ref()
        .ok_or_else(|| eyre::eyre!("known mempool outpoint must resolve to a prevout"))?;
    assert_eq!(known_prevout.value_zat, 1_000);
    assert_eq!(known_prevout.script_pub_key, vec![0xAA; 25]);
    assert!(
        response.entries[1].output.is_none(),
        "unknown txid must resolve to None",
    );
    assert!(
        response.entries[2].output.is_none(),
        "out-of-bounds output_index must resolve to None",
    );
    Ok(())
}

/// `MempoolMinedEvent.block_hash` rides the wire alongside `mined_height`.
///
/// Source-driven enrichment: the canonical event log persists the
/// source-observed block hash and the gRPC stream replays it verbatim.
#[tokio::test(flavor = "multi_thread")]
async fn mempool_mined_event_block_hash_rides_the_wire() -> Result<()> {
    use zinder_core::BlockHash;

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let store = store_fixture.chain_store().clone();
    let mempool_index = MempoolIndex::new();

    let txid = TransactionId::from_bytes([0xCA; 32]);
    let block_hash = BlockHash::from_bytes([0xCB; 32]);
    let _mined_envelope = append_mempool_event(
        &store,
        MempoolEvent::Mined {
            transaction_id: txid,
            mined_height: zinder_core::BlockHeight::new(42),
            block_hash,
        },
    )?;

    let listen_addr = spawn_ingest_control(store, mempool_index).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            start: Some(earliest_start()),
            family: MempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();

    let envelope = next_envelope(&mut event_stream).await?;
    let event = envelope
        .event
        .ok_or_else(|| eyre::eyre!("envelope event missing"))?;
    let mempool_event_envelope::Event::Mined(mined) = event else {
        return Err(eyre::eyre!("expected Mined event"));
    };
    assert_eq!(mined.transaction_id, encode_rpc_transaction_id_hex(txid));
    assert_eq!(mined.mined_height, 42);
    assert_eq!(mined.block_hash, encode_rpc_block_hash_hex(block_hash));

    Ok(())
}

fn synthetic_entry(transaction_id_byte: u8, chain_epoch: zinder_core::ChainEpoch) -> MempoolEntry {
    let transaction_id = TransactionId::from_bytes([transaction_id_byte; 32]);
    MempoolEntry {
        transaction_id,
        auth_digest: Some(AuthDigest::from_bytes([transaction_id_byte; 32])),
        raw_transaction_bytes: RawTransactionBytes::new(vec![transaction_id_byte; 16]),
        compact_transaction_bytes: vec![transaction_id_byte; 8],
        first_seen_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        first_seen_chain_epoch: chain_epoch,
        transparent_outputs: vec![TransparentMempoolOutput {
            address_script_hash: TransparentAddressScriptHash::from_bytes([0xAA; 32]),
            script_pub_key: vec![0xAA; 25],
            outpoint: TransparentOutPoint::new(transaction_id, 0),
            value_zat: 1_000,
        }],
        transparent_spends: vec![TransparentMempoolSpend {
            spent_outpoint: TransparentOutPoint::new(TransactionId::from_bytes([0x55; 32]), 0),
            spending_transaction_id: transaction_id,
        }],
    }
}

async fn spawn_ingest_control(
    store: PrimaryChainStore,
    mempool_index: MempoolIndex,
) -> Result<std::net::SocketAddr> {
    spawn_ingest_control_with_options(store, mempool_index, None).await
}

async fn spawn_ingest_control_with_options(
    store: PrimaryChainStore,
    mempool_index: MempoolIndex,
    bearer_token: Option<zinder_runtime::BearerToken>,
) -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let adapter = {
        let mut adapter = IngestControlGrpcAdapter::new(
            Network::ZcashRegtest,
            store,
            zinder_runtime::Readiness::default(),
        )
        .with_mempool(mempool_index);
        if let Some(token) = bearer_token {
            adapter = adapter.with_bearer_token(token);
        }
        adapter
    };
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

async fn next_envelope<S>(stream: &mut S) -> Result<zinder_proto::v1::wallet::MempoolEventEnvelope>
where
    S: tokio_stream::Stream<
            Item = std::result::Result<
                zinder_proto::v1::wallet::MempoolEventEnvelope,
                tonic::Status,
            >,
        > + Unpin,
{
    let stream_outcome = tokio::time::timeout(Duration::from_secs(2), stream.next()).await?;
    let envelope_outcome = stream_outcome.ok_or_else(|| eyre::eyre!("event stream closed"))?;
    Ok(envelope_outcome?)
}
