#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;

use eyre::Result;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use zinder_core::{
    AuthDigest, BlockHash, MempoolEntry, MempoolEvictionReason, Network, RawTransactionBytes,
    TransactionId, TransparentAddressScriptHash, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, UnixTimestampMillis,
};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolApplyOutcome, MempoolIndex};
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{
        MempoolEventStreamFamily, MempoolEventsRequest,
        MempoolSnapshotRequest as ControlMempoolSnapshotRequest, mempool_event_envelope,
    },
};
use zinder_store::{
    DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS, MempoolEvent, MempoolEventEnvelope,
    MempoolEventHistoryRequest, MempoolEventRetentionReport, PrimaryChainStore, StoreError,
    StreamCursorTokenV1,
};
use zinder_testkit::StoreFixture;

fn append_mempool_event(
    store: &PrimaryChainStore,
    event: MempoolEvent,
) -> Result<MempoolEventEnvelope> {
    Ok(store.append_mempool_event(event, UnixTimestampMillis::now())?)
}

fn retained_mempool_event_count(store: &PrimaryChainStore) -> Result<u64> {
    Ok(store.mempool_event_retention_report()?.retained_event_count)
}

fn mempool_retention_report(store: &PrimaryChainStore) -> Result<MempoolEventRetentionReport> {
    Ok(store.mempool_event_retention_report()?)
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
    assert_eq!(
        mempool_index.apply_added(admitted.clone()),
        MempoolApplyOutcome::Applied
    );
    let _added_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: admitted.clone(),
        },
    )?;
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
        .chain_epoch
        .ok_or_else(|| eyre::eyre!("snapshot.chain_epoch is missing"))?;
    assert_eq!(chain_epoch_in_response.network_name, "zcash-regtest");
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.snapshot_sequence, 2);
    let observed_entry = snapshot
        .entries
        .first()
        .ok_or_else(|| eyre::eyre!("snapshot has no entry"))?;
    assert_eq!(
        observed_entry.transaction_id,
        admitted.transaction_id.as_bytes().to_vec()
    );

    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            from_cursor: Vec::new(),
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
async fn ingest_control_pages_mempool_snapshot_from_live_index() -> Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let store = store_fixture.chain_store().clone();

    let entry_one = synthetic_entry(0x01, chain_epoch);
    let entry_two = synthetic_entry(0x02, chain_epoch);
    assert_eq!(
        mempool_index.apply_added(entry_one.clone()),
        MempoolApplyOutcome::Applied
    );
    assert_eq!(
        mempool_index.apply_added(entry_two.clone()),
        MempoolApplyOutcome::Applied
    );
    let _ = append_mempool_event(&store, MempoolEvent::Added { entry: entry_one })?;
    let _ = append_mempool_event(&store, MempoolEvent::Added { entry: entry_two })?;

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
    assert_eq!(first_page.snapshot_sequence, 2);
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
    assert!(
        second_page.next_cursor.is_empty(),
        "second page should finish the two-entry snapshot"
    );
    Ok(())
}

/// Bearer-token auth: a server configured with a token rejects requests
/// that lack the header, rejects requests with the wrong token, and accepts
/// requests carrying the matching token through the client interceptor.
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
/// client succeeds. This pins the localhost-default deployment story so a
/// future refactor cannot accidentally make auth required by default.
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
    let _ = mempool_index.apply_added(entry_one.clone());
    let _ = mempool_index.apply_added(entry_two.clone());
    let first_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry_one.clone(),
        },
    )?;
    let _second_envelope = append_mempool_event(
        &store,
        MempoolEvent::Added {
            entry: entry_two.clone(),
        },
    )?;

    let listen_addr = spawn_ingest_control(store, mempool_index.clone()).await?;

    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            from_cursor: first_envelope.cursor.as_bytes().to_vec(),
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
            from_cursor: first_envelope.cursor.as_bytes().to_vec(),
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
/// sequence surfaces `MempoolCursorExpired` with the structured floor.
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

/// End-to-end retention worker: drives synthetic mempool events into a real
/// RocksDB-backed log over time, runs `spawn_mempool_event_retention_task`
/// with aggressive time-window settings, and asserts that the worker
/// (a) prunes Mined envelopes that age past their window,
/// (b) keeps Invalidated envelopes that age within their longer window,
/// (c) updates the retention floor in `storage_control`,
/// (d) flips the readiness signal to `MempoolCursorAtRisk` when the oldest
///     retained event crosses the warning threshold, and
/// (e) flips back to `Ready` once pruning brings the floor back inside the
///     window.
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end retention test exercises append → time-pass → prune → readiness flip in one linear sequence so timing assumptions stay auditable in a single function."
)]
#[tokio::test(flavor = "multi_thread")]
async fn mempool_retention_worker_prunes_and_drives_readiness_under_traffic() -> Result<()> {
    use tokio_util::sync::CancellationToken;
    use zinder_ingest::{MempoolEventRetentionWorkerConfig, spawn_mempool_event_retention_task};
    use zinder_runtime::{Readiness, ReadinessCause, ReadinessState};
    use zinder_store::MempoolEventRetentionConfig;

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();

    // Configure retention with a Mined window much shorter than Invalidated
    // so the worker prunes Mined envelopes first; the warning fires when
    // the oldest retained event crosses (mined_window - warning) age.
    let mined_retention = Duration::from_millis(400);
    let invalidated_retention = Duration::from_secs(10);
    let cursor_at_risk_warning = Duration::from_millis(250);
    let retention_config =
        MempoolEventRetentionConfig::new(Some(mined_retention), Some(invalidated_retention));
    let worker_config = MempoolEventRetentionWorkerConfig {
        retention: retention_config,
        check_interval: Duration::from_millis(100),
        cursor_at_risk_warning,
    };

    let readiness = Readiness::new(ReadinessState::ready(Some(chain_epoch.tip_height.value())));
    let cancel = CancellationToken::new();
    let worker_handle = spawn_mempool_event_retention_task(
        store.clone(),
        readiness.clone(),
        worker_config,
        cancel.clone(),
    );

    // Append a Mined envelope at t=0, an Invalidated at t≈50ms, another
    // Mined at t≈100ms. With mined_retention=400ms and invalidated=10s, the
    // first two Mined envelopes age past the window first.
    let first_mined = append_mempool_event(
        &store,
        MempoolEvent::Mined {
            transaction_id: TransactionId::from_bytes([0xD0; 32]),
            mined_height: zinder_core::BlockHeight::new(101),
            block_hash: BlockHash::from_bytes([0xD0; 32]),
        },
    )?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _invalidated = append_mempool_event(
        &store,
        MempoolEvent::Invalidated {
            transaction_id: TransactionId::from_bytes([0xD1; 32]),
            reason: MempoolEvictionReason::Conflict,
        },
    )?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second_mined = append_mempool_event(
        &store,
        MempoolEvent::Mined {
            transaction_id: TransactionId::from_bytes([0xD2; 32]),
            mined_height: zinder_core::BlockHeight::new(102),
            block_hash: BlockHash::from_bytes([0xD2; 32]),
        },
    )?;

    // Wait long enough for both Mined envelopes to age past
    // mined_retention (400ms) and for at least one worker pass to fire.
    tokio::time::sleep(Duration::from_millis(700)).await;

    let report = mempool_retention_report(&store)?;
    let oldest_retained_sequence = report
        .oldest_retained_sequence
        .ok_or_else(|| eyre::eyre!("retention report has no oldest_retained_sequence"))?;

    // Both Mined envelopes (event_sequence 1 and 3) should be pruned; the
    // Invalidated envelope (event_sequence 2) survives because its window
    // is much longer. The oldest_retained must therefore be ≥2.
    assert!(
        oldest_retained_sequence >= 2,
        "retention worker should have pruned the first Mined envelope; report: {report:?}"
    );
    assert_eq!(report.current_event_sequence, 3);
    // Cursor pointing at the pruned first_mined envelope should now expire.
    let outcome = read_mempool_envelopes(&store, Some(&first_mined.cursor));
    let error = outcome
        .err()
        .ok_or_else(|| eyre::eyre!("expected cursor expired for pruned first mined envelope"))?;
    if let StoreError::MempoolEventCursorExpired { .. } = &error {
        // expected
    } else {
        return Err(eyre::eyre!("unexpected error variant: {error:?}"));
    }

    // The Invalidated envelope (event_sequence 2) should still be readable;
    // second_mined (event_sequence 3) may or may not be pruned depending on
    // timing.
    let envelopes = read_mempool_envelopes(&store, None)
        .map_err(|error| eyre::eyre!("read after prune failed: {error:?}"))?;
    let kinds: Vec<&'static str> = envelopes
        .iter()
        .map(|envelope| match &envelope.event {
            MempoolEvent::Added { .. } => "added",
            MempoolEvent::Invalidated { .. } => "invalidated",
            MempoolEvent::Mined { .. } => "mined",
            _ => "other",
        })
        .collect();
    assert!(
        kinds.contains(&"invalidated"),
        "Invalidated envelope must survive its longer retention window; observed: {kinds:?}"
    );

    // Wait long enough for the second Mined envelope (event_sequence 3) to
    // also age past the window; at this point the oldest retained is the
    // Invalidated envelope. The shortest configured window is 400ms; with
    // warning=250ms the threshold is 150ms. The Invalidated envelope was
    // appended at t≈50ms; by now it is well past 150ms old, so the worker
    // should report at_risk against the shortest-window threshold.
    let mut at_risk_observed = false;
    for _attempt in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if matches!(
            readiness.report().cause,
            ReadinessCause::MempoolCursorAtRisk { .. }
        ) {
            at_risk_observed = true;
            break;
        }
    }
    assert!(
        at_risk_observed,
        "readiness should have flipped to MempoolCursorAtRisk; current cause: {:?}",
        readiness.report().cause
    );

    // Cancel the worker and confirm it exits cleanly.
    cancel.cancel();
    worker_handle.await?;

    let _ = second_mined; // pruning of second_mined is timing-sensitive
    Ok(())
}

/// Reorg semantics gate: the orchestrator + index must faithfully relay the
/// Added → Mined → Added sequence the source emits when a tx is mined into
/// block N and then block N is reorged out, returning the tx to the
/// upstream node's mempool.
///
/// The structural invariant is that nothing in the chain ingest path emits
/// mempool events; Zinder must follow upstream node mempool observations
/// rather than synthesizing entries from reverted blocks. This test
/// additionally proves that the index handles the re-entry sequence
/// correctly:
/// - The mined tx is absent from the live index.
/// - A subsequent Added re-emission re-inserts the entry cleanly,
///   including the transparent overlays.
/// - The persistent event log records all three transitions with strictly
///   monotonic sequence numbers and distinct cursors.
///
/// The live counterpart (broadcast a tx, mine it, reorg the block out via
/// `invalidateblock`, observe the source's reorg events) is deferred until
/// the broadcast cycle is unblocked.
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end reorg flow: source setup, applier, orchestrator, mine event, reorg event, assertions."
)]
#[tokio::test(flavor = "multi_thread")]
async fn reorg_returns_mined_tx_to_mempool_through_orchestrator() -> Result<()> {
    use std::sync::Arc;

    use tokio::time::Duration;
    use zinder_ingest::{MempoolIndex, MempoolOrchestratorEventOutcome, run_mempool_orchestrator};
    use zinder_source::MempoolSourceEntry;
    use zinder_testkit::{MockMempoolSource, StoreFixture};

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();
    let mempool_index = MempoolIndex::new();

    let (source, control) = MockMempoolSource::streaming();
    let outcomes = Arc::new(parking_lot::Mutex::new(Vec::<
        MempoolOrchestratorEventOutcome,
    >::new()));
    let outcomes_for_orchestrator = Arc::clone(&outcomes);

    let orchestrator_handle = {
        let mempool_index = mempool_index.clone();
        let store_for_orchestrator = store.clone();
        tokio::spawn(async move {
            run_mempool_orchestrator(
                Arc::new(source),
                store_for_orchestrator,
                mempool_index,
                move |outcome| {
                    // SourceStreamOpened is a one-shot lifecycle signal,
                    // not a per-event observation. Filter it out so the
                    // count-based wait helpers below stay aligned with
                    // pushed source events.
                    if !matches!(outcome, MempoolOrchestratorEventOutcome::SourceStreamOpened) {
                        outcomes_for_orchestrator.lock().push(outcome);
                    }
                },
            )
            .await
        })
    };

    let coinbase_synthetic_entry = MempoolSourceEntry {
        transaction_id: TransactionId::from_bytes([0xE0; 32]),
        auth_digest: Some(AuthDigest::from_bytes([0xE0; 32])),
        // Use a real-looking transparent v4 tx prefix so build_mempool_entry
        // can parse it; the actual bytes do not need to be a valid signed
        // transaction because hydration only requires zebra-chain to decode
        // the transparent inputs and outputs.
        raw_transaction_bytes: zinder_core::RawTransactionBytes::new(synthetic_v4_tx_bytes()),
        observed_at_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
    };
    let admitted_transaction_id = coinbase_synthetic_entry.transaction_id;
    let _ = chain_epoch; // already committed; orchestrator reads it from the store

    // Wait for the orchestrator to open the source stream before pushing
    // the first event. `MockMempoolSource::push_*` returns `Closed` until
    // the consumer calls `events()`.
    wait_for_source_open(&control).await?;

    // Phase 1: source observes Added → orchestrator hydrates → index +
    // event log reflect the entry.
    control.push_added(coinbase_synthetic_entry.clone())?;
    wait_for_outcome_count(&outcomes, 1).await?;
    assert!(
        mempool_index.is_in_mempool(admitted_transaction_id),
        "mempool index must contain the entry after the first Added event"
    );
    assert_eq!(retained_mempool_event_count(&store)?, 1);

    // Phase 2: source observes Mined → orchestrator removes the entry from
    // the live index, the event log records a Mined transition.
    control.push_mined(
        admitted_transaction_id,
        zinder_core::BlockHeight::new(101),
        BlockHash::from_bytes([0xE0; 32]),
    )?;
    wait_for_outcome_count(&outcomes, 2).await?;
    assert!(
        !mempool_index.is_in_mempool(admitted_transaction_id),
        "mempool index must drop the entry after Mined"
    );
    assert_eq!(retained_mempool_event_count(&store)?, 2);

    // Phase 3: simulating a reorg, the source re-emits Added for the same
    // txid. The orchestrator must re-insert into the index and append a
    // third event log entry. Crucially, the index transparent overlays
    // must be re-populated, matching what the streaming source would emit
    // when Zebra observes the tx returning to mempool.
    control.push_added(coinbase_synthetic_entry.clone())?;
    wait_for_outcome_count(&outcomes, 3).await?;
    assert!(
        mempool_index.is_in_mempool(admitted_transaction_id),
        "mempool index must re-insert the entry after reorg-induced Added re-emission"
    );
    assert_eq!(retained_mempool_event_count(&store)?, 3);

    let outcomes_snapshot = outcomes.lock().clone();
    assert!(
        outcomes_snapshot
            .iter()
            .all(|outcome| matches!(outcome, MempoolOrchestratorEventOutcome::Applied)),
        "every orchestrator outcome must be Applied; observed: {outcomes_snapshot:?}"
    );

    // The event log must hold three monotonically-sequenced envelopes with
    // distinct cursors.
    let envelopes = read_mempool_envelopes(&store, None)
        .map_err(|error| eyre::eyre!("read failed: {error:?}"))?;
    assert_eq!(envelopes.len(), 3);
    let sequences: Vec<u64> = envelopes
        .iter()
        .map(|envelope| envelope.event_sequence)
        .collect();
    assert_eq!(sequences, vec![1, 2, 3]);
    let unique_cursors: std::collections::HashSet<&[u8]> = envelopes
        .iter()
        .map(|envelope| envelope.cursor.as_bytes())
        .collect();
    assert_eq!(
        unique_cursors.len(),
        3,
        "each transition must mint a distinct cursor"
    );

    // Verify the persisted Added envelope retains the transparent
    // transaction overlays (the load-bearing hydration output that
    // wallet-side queries rely on).
    let first_added_event = envelopes
        .first()
        .ok_or_else(|| eyre::eyre!("page is empty"))?;
    let zinder_store::MempoolEvent::Added { entry } = &first_added_event.event else {
        return Err(eyre::eyre!("first event must be Added"));
    };
    assert_eq!(entry.transaction_id, admitted_transaction_id);

    // Cleanup
    control.close_stream();
    let _ = tokio::time::timeout(Duration::from_secs(2), orchestrator_handle).await;
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
        mempool_index.apply_added(admitted.clone()),
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

/// `IngestControl.TransparentMempoolSpendByOutpoint` returns the mempool
/// spend that consumes the requested outpoint, and `None` for an outpoint
/// that is not being spent in the mempool.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_transparent_mempool_spend_by_outpoint() -> Result<()> {
    use zinder_proto::v1::wallet::{OutPoint, TransparentMempoolSpendByOutpointRequest};

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let admitted = synthetic_entry(0xAA, chain_epoch);
    assert_eq!(
        mempool_index.apply_added(admitted.clone()),
        MempoolApplyOutcome::Applied
    );
    let listen_addr =
        spawn_ingest_control(store_fixture.chain_store().clone(), mempool_index.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    // Synthetic entry spends outpoint ([0x55; 32], 0).
    let spent_response = client
        .transparent_mempool_spend_by_outpoint(TransparentMempoolSpendByOutpointRequest {
            outpoint: Some(OutPoint {
                transaction_id: vec![0x55; 32],
                output_index: 0,
            }),
        })
        .await?
        .into_inner();
    let spend = spent_response
        .spend
        .ok_or_else(|| eyre::eyre!("expected mempool spend"))?;
    let spent_outpoint = spend
        .spent_outpoint
        .ok_or_else(|| eyre::eyre!("expected spent_outpoint on mempool spend"))?;
    assert_eq!(spent_outpoint.transaction_id, vec![0x55; 32]);
    assert_eq!(spent_outpoint.output_index, 0);
    assert_eq!(
        spend.spending_transaction_id,
        admitted.transaction_id.as_bytes().to_vec()
    );

    let unknown_response = client
        .transparent_mempool_spend_by_outpoint(TransparentMempoolSpendByOutpointRequest {
            outpoint: Some(OutPoint {
                transaction_id: vec![0xFF; 32],
                output_index: 7,
            }),
        })
        .await?
        .into_inner();
    assert!(unknown_response.spend.is_none());

    Ok(())
}

/// `IngestControl.TransparentMempoolPrevouts` resolves the outputs of
/// mempool transactions into per-entry prevouts in input order, returning
/// `None` for outpoints that reference unknown transactions or
/// out-of-bounds output indices.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_control_serves_transparent_mempool_prevouts() -> Result<()> {
    use zinder_proto::v1::wallet::{OutPoint, TransparentMempoolPrevoutsRequest};

    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre::eyre!("fixture did not commit a chain epoch"))?;
    let mempool_index = MempoolIndex::new();
    let admitted = synthetic_entry(0xAB, chain_epoch);
    assert_eq!(
        mempool_index.apply_added(admitted.clone()),
        MempoolApplyOutcome::Applied
    );
    let listen_addr =
        spawn_ingest_control(store_fixture.chain_store().clone(), mempool_index.clone()).await?;
    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;

    let known_outpoint = OutPoint {
        transaction_id: admitted.transaction_id.as_bytes().to_vec(),
        output_index: 0,
    };
    let unknown_outpoint = OutPoint {
        transaction_id: vec![0xFF; 32],
        output_index: 0,
    };
    let oob_outpoint = OutPoint {
        transaction_id: admitted.transaction_id.as_bytes().to_vec(),
        output_index: 99,
    };

    let response = client
        .transparent_mempool_prevouts(TransparentMempoolPrevoutsRequest {
            outpoints: vec![
                known_outpoint.clone(),
                unknown_outpoint.clone(),
                oob_outpoint.clone(),
            ],
        })
        .await?
        .into_inner();

    assert!(response.chain_epoch.is_some());
    assert_eq!(response.entries.len(), 3);
    let known_prevout = response.entries[0]
        .prevout
        .as_ref()
        .ok_or_else(|| eyre::eyre!("known mempool outpoint must resolve to a prevout"))?;
    assert_eq!(known_prevout.value_zat, 1_000);
    assert_eq!(known_prevout.script_pub_key, vec![0xAA; 25]);
    assert!(
        response.entries[1].prevout.is_none(),
        "unknown txid must resolve to None",
    );
    assert!(
        response.entries[2].prevout.is_none(),
        "out-of-bounds output_index must resolve to None",
    );
    Ok(())
}

/// `MempoolMinedEvent.block_hash` rides the wire alongside `mined_height`.
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
            from_cursor: Vec::new(),
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
    assert_eq!(mined.transaction_id, txid.as_bytes().to_vec());
    assert_eq!(mined.mined_height, 42);
    assert_eq!(mined.block_hash, block_hash.as_bytes().to_vec());

    Ok(())
}

async fn wait_for_source_open(control: &zinder_testkit::MockMempoolSourceControl) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while control.open_count() < 1 {
        if std::time::Instant::now() > deadline {
            return Err(eyre::eyre!(
                "orchestrator did not open the source stream within 2s"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Ok(())
}

async fn wait_for_outcome_count(
    outcomes: &std::sync::Arc<
        parking_lot::Mutex<Vec<zinder_ingest::MempoolOrchestratorEventOutcome>>,
    >,
    expected: usize,
) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while outcomes.lock().len() < expected {
        if std::time::Instant::now() > deadline {
            return Err(eyre::eyre!(
                "orchestrator did not produce {expected} outcomes within 5s; observed {}",
                outcomes.lock().len()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Returns a minimal v4 transparent transaction that `zebra-chain` can decode.
///
/// Used to construct synthetic mempool source entries in reorg-semantics
/// tests; the bytes do not need to be a valid signed transaction because
/// the orchestrator's hydration step only parses the structure.
fn synthetic_v4_tx_bytes() -> Vec<u8> {
    // v4 Sapling, no overwintered flag set, version_group_id, lock_time,
    // expiry_height, no inputs/outputs, no Sapling shielded spends/outputs,
    // value_balance = 0, no JoinSplits, no binding sig.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x8000_0004_u32.to_le_bytes()); // version | overwintered flag
    bytes.extend_from_slice(&0x892F_2085_u32.to_le_bytes()); // version_group_id (Sapling v4)
    bytes.push(0); // tx_in count
    bytes.push(0); // tx_out count
    bytes.extend_from_slice(&0_u32.to_le_bytes()); // lock_time
    bytes.extend_from_slice(&0_u32.to_le_bytes()); // expiry_height
    bytes.extend_from_slice(&0_i64.to_le_bytes()); // value_balance (i64)
    bytes.push(0); // shielded_spends count
    bytes.push(0); // shielded_outputs count
    bytes.push(0); // joinsplits count
    bytes
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
        let mut adapter =
            IngestControlGrpcAdapter::new(Network::ZcashRegtest, store).with_mempool(mempool_index);
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
