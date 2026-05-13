#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use prost::Message;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::{ZcashDeserializeInto, ZcashSerialize as _};
use zinder_core::{
    AuthDigest, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, Network,
    RawTransactionBytes, TransactionId, UnixTimestampMillis,
};
use zinder_ingest::{MempoolIndex, build_mempool_entry, run_mempool_orchestrator};
use zinder_proto::compat::lightwalletd::CompactTx;
use zinder_source::{
    MempoolSourceEntry, NodeSource, ZebraIndexerMempoolSource, ZebraIndexerMempoolSourceOptions,
    ZebraIndexerSourceTarget, ZebraJsonRpcSource, ZebraJsonRpcSourceOptions,
};
use zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION;
use zinder_testkit::StoreFixture;
use zinder_testkit::live::{LiveTestEnv, init, require_live, require_live_for};

/// Validates that the canonical hydration step (`build_mempool_entry`) decodes
/// a real Zebra-emitted transaction into a well-formed `MempoolEntry` whose
/// raw bytes round-trip, compact-tx parses as `lightwalletd::CompactTx`,
/// transparent overlays match the parsed transaction, and identifiers agree
/// with `zebra-chain`'s view.
///
/// The test exercises the same parsing pipeline that the streaming
/// orchestrator runs on every observed `Added` notification, but against the
/// regtest tip's coinbase transaction so it requires no wallet setup. The
/// end-to-end broadcast cycle (a real `z_sendmany`-shaped transaction observed
/// through `MempoolSourceEvent::Added` and reconciled with the resulting
/// `MempoolEntry`) is covered by `mempool_broadcast_cycle.rs`.
#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn build_mempool_entry_decodes_real_zebra_coinbase_into_canonical_form() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live()? else {
        return Ok(());
    };
    let json_rpc = json_rpc_source(&env)?;

    let tip_height = NodeSource::tip_id(&json_rpc).await?.height;
    let coinbase = fetch_tip_coinbase(&json_rpc, tip_height).await?;

    let synthetic_observed_at_unix_millis = UnixTimestampMillis::new(1_700_000_000_000);
    let source_entry = MempoolSourceEntry {
        transaction_id: coinbase.transaction_id,
        auth_digest: coinbase.auth_digest,
        raw_transaction_bytes: RawTransactionBytes::new(coinbase.raw_bytes.clone()),
        observed_at_unix_millis: synthetic_observed_at_unix_millis,
    };
    let chain_epoch = synthetic_chain_epoch_at(env.network(), tip_height);
    let mempool_entry = build_mempool_entry(source_entry, chain_epoch)?;

    assert_eq!(mempool_entry.transaction_id, coinbase.transaction_id);
    assert_eq!(mempool_entry.auth_digest, coinbase.auth_digest);
    assert_eq!(
        mempool_entry.first_seen_unix_millis,
        synthetic_observed_at_unix_millis
    );
    assert_eq!(mempool_entry.first_seen_chain_epoch, chain_epoch);
    assert_eq!(
        mempool_entry.raw_transaction_bytes.as_slice(),
        coinbase.raw_bytes.as_slice()
    );
    assert!(
        mempool_entry.transparent_spends.is_empty(),
        "coinbase contributes no transparent spends; got {} spends",
        mempool_entry.transparent_spends.len()
    );
    assert!(
        !mempool_entry.transparent_outputs.is_empty(),
        "coinbase has at least one transparent output to the miner address"
    );
    for transparent_output in &mempool_entry.transparent_outputs {
        assert_eq!(
            transparent_output.outpoint.transaction_id, coinbase.transaction_id,
            "transparent overlay outpoint must reference the coinbase txid"
        );
    }

    let compact_tx = CompactTx::decode(mempool_entry.compact_transaction_bytes.as_slice())
        .map_err(|error| eyre!("compact-tx decode failed: {error}"))?;
    assert_eq!(compact_tx.index, 0, "mempool compact-tx must use index 0");
    assert_eq!(
        compact_tx.txid.as_slice(),
        coinbase.transaction_id.as_bytes(),
        "compact-tx txid must match the coinbase transaction id"
    );
    Ok(())
}

fn json_rpc_source(env: &LiveTestEnv) -> Result<ZebraJsonRpcSource> {
    Ok(ZebraJsonRpcSource::with_options(
        env.target.network,
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?)
}

struct ZebraCoinbase {
    transaction_id: TransactionId,
    auth_digest: Option<AuthDigest>,
    raw_bytes: Vec<u8>,
}

async fn fetch_tip_coinbase(
    json_rpc: &ZebraJsonRpcSource,
    tip_height: BlockHeight,
) -> Result<ZebraCoinbase> {
    let source_block = json_rpc.fetch_block_by_height(tip_height).await?;
    let parsed_block: ZebraBlock = source_block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|error| eyre!("zebra-chain block parse failed: {error}"))?;
    let coinbase_transaction = parsed_block
        .transactions
        .first()
        .ok_or_else(|| eyre!("regtest tip block has no coinbase transaction"))?;
    let raw_bytes = coinbase_transaction
        .zcash_serialize_to_vec()
        .map_err(|error| eyre!("coinbase serialize failed: {error}"))?;
    Ok(ZebraCoinbase {
        transaction_id: TransactionId::from_bytes(coinbase_transaction.hash().0),
        auth_digest: coinbase_transaction
            .auth_digest()
            .map(|digest| AuthDigest::from_bytes(digest.0)),
        raw_bytes,
    })
}

fn synthetic_chain_epoch_at(network: Network, tip_height: BlockHeight) -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(1),
        network,
        tip_height,
        tip_hash: BlockHash::from_bytes([0x42; 32]),
        finalized_height: tip_height,
        finalized_hash: BlockHash::from_bytes([0x42; 32]),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_700_000_000_000),
    }
}

/// Validates the orchestrator wiring against a live Zebra indexer:
/// `ZebraIndexerMempoolSource` → `run_mempool_orchestrator` →
/// `MempoolIndex` + canonical mempool-event store runs cleanly for a few
/// seconds without panicking, hangs, or fatal errors.
///
/// This is a smoke test for the streaming integration: no transactions are
/// expected on an idle regtest mempool, so the test verifies the loop stays
/// alive (orchestrator task not finished) and that any events that do fire
/// reach the in-memory state through the canonical pipeline.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn mempool_orchestrator_runs_against_real_zebra_indexer_with_in_memory_state() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };
    let Some(indexer_endpoint_url) = env.target.indexer_grpc_addr.clone() else {
        return Err(eyre!(
            "this test needs ZINDER_NODE__INDEXER_GRPC_ADDR; skipping"
        ));
    };

    let store_fixture = StoreFixture::with_single_block(env.network())?;
    let chain_store = store_fixture.chain_store().clone();
    let mempool_index = MempoolIndex::new();
    let mempool_source: Arc<ZebraIndexerMempoolSource> =
        Arc::new(build_indexer_mempool_source(&env, indexer_endpoint_url)?);

    let orchestrator_index = mempool_index.clone();
    let chain_store_for_orchestrator = chain_store.clone();
    let orchestrator_handle = tokio::spawn(async move {
        run_mempool_orchestrator(
            mempool_source,
            chain_store_for_orchestrator,
            orchestrator_index,
            |_outcome| {},
        )
        .await
    });

    // Run the orchestrator long enough to confirm the source is wired up
    // without immediately erroring; on an idle regtest mempool no events
    // fire so the index/log stay empty.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !orchestrator_handle.is_finished(),
        "orchestrator should still be running on an idle mempool but the task already returned"
    );

    orchestrator_handle.abort();
    match orchestrator_handle.await {
        Ok(orchestrator_outcome) => {
            return Err(eyre!(
                "orchestrator returned before abort took effect: {orchestrator_outcome:?}"
            ));
        }
        Err(join_error) if join_error.is_cancelled() => {}
        Err(join_error) => {
            return Err(eyre!(
                "orchestrator task did not cancel cleanly: {join_error}"
            ));
        }
    }

    assert_eq!(
        mempool_index.entry_count(),
        0,
        "idle regtest mempool should not have applied any entries to the index"
    );
    let retention = chain_store.mempool_event_retention_report()?;
    assert_eq!(
        retention.retained_event_count, 0,
        "idle regtest mempool should not have appended any envelopes to the event store"
    );
    Ok(())
}

fn build_indexer_mempool_source(
    env: &LiveTestEnv,
    indexer_endpoint_url: String,
) -> Result<ZebraIndexerMempoolSource> {
    let hydration_json_rpc = json_rpc_source(env)?;
    Ok(ZebraIndexerMempoolSource::with_options(
        ZebraIndexerSourceTarget::new(indexer_endpoint_url),
        hydration_json_rpc,
        ZebraIndexerMempoolSourceOptions::default(),
    ))
}

/// End-to-end persistence: a `MempoolEntry` built from a real Zebra-emitted
/// coinbase transaction is appended to the canonical mempool event store,
/// served through the `IngestControl.MempoolEvents` gRPC, then rediscovered
/// via cursor resume after the writer process is dropped and the same
/// `RocksDB` store path is reopened.
///
/// The real-data ingredients:
/// - A live regtest Zebra mines coinbase transactions; the test fetches one
///   from the current tip and runs it through `build_mempool_entry`,
///   producing the same `MempoolEntry` shape the streaming orchestrator
///   would produce on an `Added` notification.
/// - The envelope is written into a real `RocksDB` store
///   (`StorageTable::MempoolEvent`) opened on a tempdir.
/// - An `IngestControl` gRPC server backed by that store streams the
///   envelope to a tonic client.
/// - The same `RocksDB` path is reopened by a fresh `PrimaryChainStore`
///   handle to prove durability across writer restarts.
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end persistence test composes Zebra fetch + entry build + store write + writer restart + gRPC client read inline so the sequence of real-data ingredients stays auditable in one function."
)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn mempool_event_log_persists_real_zebra_entry_across_writer_restart() -> Result<()> {
    use std::time::Duration;

    use tokio::net::TcpListener;
    use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
    use tokio_util::sync::CancellationToken;
    use tonic::transport::Server;
    use zebra_chain::serialization::{ZcashDeserializeInto, ZcashSerialize};
    use zinder_core::{
        AuthDigest, BlockHash, ChainEpoch, ChainEpochId, ChainTipMetadata, RawTransactionBytes,
        TransactionId, UnixTimestampMillis,
    };
    use zinder_ingest::IngestControlGrpcAdapter;
    use zinder_proto::v1::{
        ingest::ingest_control_client::IngestControlClient,
        wallet::{
            MempoolEventStreamFamily as ProtoMempoolEventStreamFamily, MempoolEventsRequest,
            mempool_event_envelope,
        },
    };
    use zinder_source::{MempoolSourceEntry, NodeSource};
    use zinder_store::{
        CURRENT_ARTIFACT_SCHEMA_VERSION, ChainStoreOptions, MempoolEvent, PrimaryChainStore,
    };

    let _guard = init();
    let Some(env) = require_live()? else {
        return Ok(());
    };

    let json_rpc = json_rpc_source(&env)?;
    let tip_id = NodeSource::tip_id(&json_rpc).await?;
    let coinbase_block = json_rpc.fetch_block_by_height(tip_id.height).await?;
    let parsed_block: zebra_chain::block::Block = coinbase_block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into()?;
    let coinbase_transaction = parsed_block
        .transactions
        .first()
        .ok_or_else(|| eyre!("regtest tip block has no coinbase transaction"))?;
    let coinbase_raw_bytes = coinbase_transaction.zcash_serialize_to_vec()?;
    let coinbase_transaction_id = TransactionId::from_bytes(coinbase_transaction.hash().0);
    let coinbase_auth_digest = coinbase_transaction
        .auth_digest()
        .map(|digest| AuthDigest::from_bytes(digest.0));

    let synthetic_chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: env.network(),
        tip_height: tip_id.height,
        tip_hash: BlockHash::from_bytes([0x42; 32]),
        finalized_height: tip_id.height,
        finalized_hash: BlockHash::from_bytes([0x42; 32]),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_700_000_000_000),
    };
    let mempool_entry = build_mempool_entry(
        MempoolSourceEntry {
            transaction_id: coinbase_transaction_id,
            auth_digest: coinbase_auth_digest,
            raw_transaction_bytes: RawTransactionBytes::new(coinbase_raw_bytes),
            observed_at_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        },
        synthetic_chain_epoch,
    )?;

    let tempdir = tempfile::tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");

    // Phase 1: open the store, append one envelope, mint its cursor, drop
    // the writer.
    let pre_restart_cursor = {
        let store =
            PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
        let envelope = store.append_mempool_event(
            MempoolEvent::Added {
                entry: mempool_entry.clone(),
            },
            UnixTimestampMillis::now(),
        )?;
        assert_eq!(envelope.event_sequence, 1);
        assert_eq!(
            store.mempool_event_retention_report()?.retained_event_count,
            1
        );
        envelope.cursor
    };

    // Phase 2: reopen the same path, append a second envelope, serve both
    // via IngestControl gRPC, and verify the pre-restart cursor resumes
    // strictly after the original event.
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    assert_eq!(
        store.mempool_event_retention_report()?.retained_event_count,
        1
    );

    let invalidated_envelope = store.append_mempool_event(
        MempoolEvent::Invalidated {
            transaction_id: mempool_entry.transaction_id,
            reason: zinder_core::MempoolEvictionReason::Conflict,
        },
        UnixTimestampMillis::now(),
    )?;
    assert_eq!(invalidated_envelope.event_sequence, 2);
    assert_eq!(
        store.mempool_event_retention_report()?.retained_event_count,
        2
    );

    let mempool_index = MempoolIndex::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let cancel_for_server = cancel.clone();
    let adapter = IngestControlGrpcAdapter::new(env.network(), store).with_mempool(mempool_index);
    let server_handle = tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                cancel_for_server.cancelled_owned(),
            )
            .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = IngestControlClient::connect(format!("http://{listen_addr}")).await?;
    let mut event_stream = client
        .mempool_events(MempoolEventsRequest {
            from_cursor: pre_restart_cursor.as_bytes().to_vec(),
            family: ProtoMempoolEventStreamFamily::Mempool as i32,
        })
        .await?
        .into_inner();
    let resumed_envelope_outcome =
        tokio::time::timeout(Duration::from_secs(5), event_stream.next()).await?;
    let resumed_envelope = resumed_envelope_outcome
        .ok_or_else(|| eyre!("ingest-control mempool_events closed before resume"))??;
    assert_eq!(
        resumed_envelope.event_sequence, 2,
        "cursor resume must skip the pre-restart envelope and yield the post-restart one"
    );
    assert!(matches!(
        resumed_envelope
            .event
            .ok_or_else(|| eyre!("resumed envelope event missing"))?,
        mempool_event_envelope::Event::Invalidated(_)
    ));

    // Drop the client-side stream first so the gRPC framework releases its
    // response handler; the server-side `stream_mempool_events` task then
    // observes `event_sender.closed()` and exits, freeing
    // `serve_with_incoming_shutdown` to finalize.
    drop(event_stream);
    drop(client);
    cancel.cancel();
    server_handle.abort();
    let _ = server_handle.await;

    // Sanity check: the persisted Added envelope decodes the real coinbase
    // transparent overlay.
    assert!(
        !mempool_entry.transparent_outputs.is_empty(),
        "regtest coinbase always has at least one transparent output"
    );
    Ok(())
}

/// End-to-end production wiring of [`spawn_ingest_control_tip_change_publisher`]
/// against a real `IngestControl.ChainEvents` stream. Verifies that:
///
/// - `tip_follow_with_primary_store` commits chain epochs to a real `RocksDB`
///   store as Zebra mines new blocks.
/// - The `IngestControl` gRPC server serves `ChainEvents` from those commits.
/// - `spawn_ingest_control_tip_change_publisher` connects to the server and
///   publishes the latest committed event sequence to a watch channel.
/// - `WatchTipChangeWatcher::await_tip_change` resolves within a bounded
///   window after Zebra mines a block via the regtest `generate` RPC.
///
/// The end-to-end path closes the gap between unit-tested `ScriptedTipChangeWatcher`
/// fixtures and the production wiring `zinder-compat-lightwalletd` consumes
/// via `IngestControlMempoolSurface`.
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end publisher test wires tip-follow + IngestControl gRPC server + chain-events publisher + watcher in one function so the linear ordering of subsystem starts and shutdowns stays auditable."
)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn ingest_control_tip_change_publisher_fires_when_zebra_mines_block() -> Result<()> {
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_util::sync::CancellationToken;
    use tonic::transport::Server;
    use zinder_compat_lightwalletd::{TipChangeWatcher, spawn_ingest_control_tip_change_publisher};
    use zinder_ingest::{IngestControlGrpcAdapter, tip_follow_with_primary_store};
    use zinder_runtime::Readiness;
    use zinder_store::{ChainStoreOptions, PrimaryChainStore};

    use crate::common::{
        live_tip_follow_config, regtest_generate_blocks, zebra_source_from_tip_follow,
    };

    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };

    // The publisher subscribes to `ChainEvents` strictly, so the writer must
    // commit at least one chain epoch before the publisher connects. Run
    // tip-follow once with a short cancellation to seed the store.
    let tempdir = tempfile::tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let tip_follow_config = live_tip_follow_config(
        &env,
        &storage_path,
        100,
        std::num::NonZeroU32::new(1).ok_or_else(|| eyre!("invalid test batch size"))?,
        Duration::from_millis(200),
    );
    let source = zebra_source_from_tip_follow(&tip_follow_config)?;
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
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
                &source,
                store,
                &readiness,
                None,
                None,
                cancel,
            )
            .await
        })
    };

    // Spin up the IngestControl gRPC server backed by the same store.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let ingest_control_addr = listener.local_addr()?;
    let ingest_control_cancel = cancel.clone();
    let ingest_adapter = IngestControlGrpcAdapter::new(env.network(), store.clone());
    let ingest_control_handle = tokio::spawn(async move {
        let _serve_outcome = Server::builder()
            .add_service(ingest_adapter.into_server())
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                ingest_control_cancel.cancelled_owned(),
            )
            .await;
    });

    // Wait for tip-follow to commit at least one chain epoch so the
    // ChainEvents subscription has something to replay.
    let tip_seeded_deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(_chain_epoch) = store.current_chain_epoch()? {
            break;
        }
        if std::time::Instant::now() > tip_seeded_deadline {
            return Err(eyre!(
                "tip-follow did not commit a chain epoch before publisher seeded"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let publisher_endpoint = format!("http://{ingest_control_addr}");
    let (tip_change_watcher, publisher_handle) =
        spawn_ingest_control_tip_change_publisher(publisher_endpoint, None, cancel.clone());

    // Drain any retained events the publisher may have replayed before the
    // watcher was constructed; these are not "tip changes after now" from
    // the watcher's perspective.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tip_change_future = {
        let watcher: Arc<dyn TipChangeWatcher> = tip_change_watcher.clone();
        tokio::spawn(async move { watcher.await_tip_change().await })
    };

    // Mine a fresh block; the writer commits it as a chain epoch, the
    // publisher receives the new ChainEvents envelope, and the watcher
    // resolves.
    let mined = regtest_generate_blocks(&env, 1).await?;
    assert_eq!(mined.len(), 1, "expected exactly one new block hash");

    let tip_change_outcome = tokio::time::timeout(Duration::from_secs(10), tip_change_future)
        .await
        .map_err(|_| eyre!("tip-change watcher did not fire within 10s of generating a block"))?;
    tip_change_outcome
        .map_err(|join_error| eyre!("tip-change task join failed: {join_error}"))?
        .map_err(|error| eyre!("tip-change watcher errored: {error}"))?;

    cancel.cancel();
    let _ = tip_follow_handle.await;
    let _ = ingest_control_handle.await;
    let _ = publisher_handle.await;

    Ok(())
}
