#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Live acceptance for `ExplorerQuery.BlockSummariesInRange` and
//! `ExplorerQuery.BlockDetail`.
//!
//! Drives the first real `BlockSummaryConsumer` end-to-end:
//!
//! 1. Backfills a small window ending at the upstream tip so the wallet
//!    plane has canonical block artifacts to serve.
//! 2. Spins up the wallet-query gRPC adapter so the consumer can subscribe
//!    to `ChainEvents` and call `FullBlock` per height.
//! 3. Spins up the explorer gRPC adapter wired to a fresh `DeriveStore`
//!    plus the `BlockSummaryConsumer` background task.
//! 4. Waits until the consumer materializes the sampled tip height.
//! 5. Calls `BlockSummariesInRange` and `BlockDetail` and asserts the
//!    response shape, freshness envelope, and transaction-id list bound to
//!    the materialized record.
//!
//! Mainnet is opt-in via `require_live_for([Network::ZcashMainnet])`.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::transport::Channel;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::wire::{encode_internal_transaction_id, encode_zinder_native_chain_name};
use zinder_core::{BlockHash, BlockHeight, Network, TransactionId};
use zinder_explorer::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BlockSummaryConsumer, DeriveStore, DeriveStoreOptions,
    ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings, run_chain_events_subscriber,
};
use zinder_ingest::{BackfillOutcome, IngestControlGrpcAdapter, MempoolIndex, backfill};
use zinder_proto::capabilities::{EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1};
use zinder_proto::v1::explorer::{
    BlockDetailRequest, BlockDetailResponse, BlockSummariesInRangeRequest,
    BlockSummariesInRangeResponse, block_detail_request,
    explorer_query_server::ExplorerQuery as ExplorerQueryService,
};
use zinder_proto::v1::wallet::{
    ChainEventStreamFamily as WireChainEventStreamFamily, ChainEventsRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_runtime::connect_authenticated_channel;
use zinder_source::{NodeSource as _, SourceBlock};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{fetch_live_tip_height, live_backfill_config, zebra_source_from_backfill};

const BACKFILL_DEPTH_BLOCKS: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn block_view_returns_summary_and_detail_after_consumer_catches_up() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = BlockViewFixture::open(&env).await?;
    fixture.wait_until_consumer_caught_up().await?;

    let range_response = fixture.query_block_summaries_in_range().await?;
    assert_range_invariants(&fixture, &range_response)?;

    let detail_response = fixture.query_block_detail_by_height().await?;
    assert_detail_invariants(&fixture, &detail_response)?;

    let detail_by_hash = fixture.query_block_detail_by_hash().await?;
    assert_eq!(
        detail_by_hash.summary.as_ref().map(|summary| summary.block_height),
        Some(fixture.sample_block_height.value()),
    );

    fixture.shutdown().await;
    Ok(())
}

fn assert_range_invariants(
    fixture: &BlockViewFixture,
    response: &BlockSummariesInRangeResponse,
) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("range response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_BLOCK_SUMMARY_V1);
    assert!(
        !response.summaries.is_empty(),
        "BlockSummariesInRange returned no summaries; consumer not caught up?",
    );
    let last = response
        .summaries
        .last()
        .ok_or_else(|| eyre!("summaries empty"))?;
    assert!(
        last.block_height <= fixture.sample_block_height.value(),
        "last summary {} exceeds sampled tip {}",
        last.block_height,
        fixture.sample_block_height.value(),
    );
    let mut prev: Option<u32> = None;
    for summary in &response.summaries {
        if let Some(prev_height) = prev {
            assert!(
                summary.block_height > prev_height,
                "summaries not ordered by ascending height",
            );
        }
        prev = Some(summary.block_height);
        assert_eq!(summary.block_hash.len(), 32);
        assert_eq!(summary.previous_block_hash.len(), 32);
        assert!(summary.transaction_count >= 1, "block must have >= 1 tx (coinbase)");
    }
    Ok(())
}

fn assert_detail_invariants(
    fixture: &BlockViewFixture,
    response: &BlockDetailResponse,
) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("detail response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_BLOCK_DETAIL_V1);
    let summary = response
        .summary
        .as_ref()
        .ok_or_else(|| eyre!("detail response missing summary"))?;
    assert_eq!(summary.block_height, fixture.sample_block_height.value());
    assert_eq!(
        u32::try_from(response.transaction_ids.len()).unwrap_or(u32::MAX),
        summary.transaction_count,
        "transaction_ids count must match summary.transaction_count",
    );
    let coinbase_id = response
        .transaction_ids
        .first()
        .ok_or_else(|| eyre!("detail response carries no transaction ids"))?;
    let expected_coinbase_id: [u8; 32] = encode_internal_transaction_id(fixture.sample_coinbase_id);
    assert_eq!(
        coinbase_id.as_slice(),
        expected_coinbase_id.as_slice(),
        "first transaction id must be the coinbase the fixture sampled",
    );
    tracing::info!(
        target: "zinder::live",
        event = "block_view_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        height = fixture.sample_block_height.value(),
        transaction_count = summary.transaction_count,
        derive_cursor_lag_blocks = freshness.derive_cursor_lag_blocks,
        "explorer block view validated against live node",
    );
    Ok(())
}

struct BlockViewFixture {
    network: Network,
    sample_block_height: BlockHeight,
    sample_block_hash: BlockHash,
    sample_coinbase_id: TransactionId,
    derive_store: DeriveStore,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    consumer_handle: JoinHandle<()>,
    consumer_cancel: CancellationToken,
    _tempdir: TempDir,
    _store_tempdir: TempDir,
}

impl BlockViewFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store, sample) = backfill_and_sample_tip(env).await?;
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );

        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(network, store, MempoolIndex::new()).await?;
        let (wallet_grpc_addr, wallet_server_handle) =
            serve_wallet_query_grpc(wallet_query, format!("http://{ingest_control_addr}")).await?;
        let wallet_endpoint = format!("http://{wallet_grpc_addr}");

        let derive_tempdir = tempdir()?;
        let derive_store = DeriveStore::open(
            derive_tempdir.path(),
            DeriveStoreOptions {
                sync_writes: false,
                consumer_column_families: &[BLOCK_SUMMARY_COLUMN_FAMILY],
            },
        )?;

        let consumer_cancel = CancellationToken::new();
        let consumer_handle = spawn_consumer(
            derive_store.clone(),
            wallet_endpoint.clone(),
            consumer_cancel.clone(),
        );

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_derive_store(derive_store.clone())
                .with_wallet_query_endpoint(wallet_endpoint);

        Ok(Self {
            network,
            sample_block_height: sample.block_height,
            sample_block_hash: sample.block_hash,
            sample_coinbase_id: sample.coinbase_transaction_id,
            derive_store,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            consumer_handle,
            consumer_cancel,
            _tempdir: derive_tempdir,
            _store_tempdir: store_tempdir,
        })
    }

    async fn wait_until_consumer_caught_up(&self) -> Result<()> {
        let key = self.sample_block_height.value().to_be_bytes();
        for _ in 0..200 {
            let payload = self
                .derive_store
                .get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &key)?;
            if payload.is_some() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(eyre!(
            "BlockSummaryConsumer did not materialize height {} within timeout",
            self.sample_block_height.value(),
        ))
    }

    async fn query_block_summaries_in_range(&self) -> Result<BlockSummariesInRangeResponse> {
        let tip = self.sample_block_height.value();
        let start = tip.saturating_sub(4);
        let request = BlockSummariesInRangeRequest {
            start_height: start,
            end_height: tip,
            at_epoch: None,
        };
        let response = ExplorerQueryService::block_summaries_in_range(
            &self.explorer_adapter,
            Request::new(request),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn query_block_detail_by_height(&self) -> Result<BlockDetailResponse> {
        let request = BlockDetailRequest {
            selector: Some(block_detail_request::Selector::BlockHeight(
                self.sample_block_height.value(),
            )),
            at_epoch: None,
        };
        let response = ExplorerQueryService::block_detail(
            &self.explorer_adapter,
            Request::new(request),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn query_block_detail_by_hash(&self) -> Result<BlockDetailResponse> {
        let request = BlockDetailRequest {
            selector: Some(block_detail_request::Selector::BlockHash(
                self.sample_block_hash.as_bytes().to_vec(),
            )),
            at_epoch: None,
        };
        let response = ExplorerQueryService::block_detail(
            &self.explorer_adapter,
            Request::new(request),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn shutdown(&mut self) {
        self.consumer_cancel.cancel();
        let _ = (&mut self.consumer_handle).await;
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
    }
}

fn spawn_consumer(
    store: DeriveStore,
    wallet_endpoint: String,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = drive_once(&store, &wallet_endpoint, &cancel).await {
            tracing::warn!(
                target: "zinder::live",
                event = "block_view_consumer_failed",
                error = %error,
                "consumer driver returned an error",
            );
        }
    })
}

async fn drive_once(
    store: &DeriveStore,
    wallet_endpoint: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let cursor = store
        .get_cursor(zinder_explorer::BLOCK_SUMMARY_CONSUMER_NAME)?
        .unwrap_or_default();
    let channel_for_stream = connect_authenticated_channel(wallet_endpoint, None)
        .await
        .map_err(|error| eyre!("consumer stream connect: {error}"))?;
    let mut stream_client = WalletQueryClient::new(channel_for_stream);
    let stream = stream_client
        .chain_events(Request::new(ChainEventsRequest {
            from_cursor: cursor,
            family: WireChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }))
        .await?
        .into_inner();
    let channel_for_consumer = connect_authenticated_channel(wallet_endpoint, None)
        .await
        .map_err(|error| eyre!("consumer fetch connect: {error}"))?;
    let mut consumer = BlockSummaryConsumer::new(WalletQueryClient::new(channel_for_consumer));
    tokio::select! {
        outcome = run_chain_events_subscriber(&mut consumer, store, stream) => {
            outcome.map(|_| ()).map_err(|error| eyre!("subscriber: {error}"))
        }
        () = cancel.cancelled() => Ok(()),
    }
}

async fn serve_wallet_query_grpc(
    wallet_query: WalletQuery<PrimaryChainStore>,
    ingest_control_endpoint: String,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        wallet_query,
        ServerInfoSettings::default(),
        ingest_control_endpoint,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    await_grpc_endpoint(addr).await?;
    Ok((addr, handle))
}

async fn serve_ingest_control_grpc(
    network: Network,
    store: PrimaryChainStore,
    mempool_index: MempoolIndex,
) -> Result<(SocketAddr, JoinHandle<Result<(), tonic::transport::Error>>)> {
    let adapter = IngestControlGrpcAdapter::new(network, store).with_mempool(mempool_index);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    await_grpc_endpoint(addr).await?;
    Ok((addr, handle))
}

async fn await_grpc_endpoint(addr: SocketAddr) -> Result<()> {
    let endpoint = format!("http://{addr}");
    for _ in 0..20 {
        if Channel::from_shared(endpoint.clone())?
            .connect()
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre!("gRPC endpoint at {addr} never accepted connections"))
}

#[derive(Clone, Debug)]
struct SampledTipBlock {
    block_height: BlockHeight,
    block_hash: BlockHash,
    coinbase_transaction_id: TransactionId,
}

async fn backfill_and_sample_tip(
    env: &LiveTestEnv,
) -> Result<(TempDir, PrimaryChainStore, SampledTipBlock)> {
    let tip_height = fetch_live_tip_height(env).await?;
    if tip_height.value() <= BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "tip height {} is at or below the minimum {BACKFILL_DEPTH_BLOCKS}",
            tip_height.value(),
        ));
    }
    let checkpoint_height = BlockHeight::new(tip_height.value() - BACKFILL_DEPTH_BLOCKS - 1);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let mut backfill_config = live_backfill_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(1000).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);
    let BackfillOutcome::Committed(_) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!("expected committed backfill outcome"));
    };

    let tip_source_block = source.fetch_block_by_height(tip_height).await?;
    let sample = sample_tip(&tip_source_block)?;

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, sample))
}

fn sample_tip(block: &SourceBlock) -> Result<SampledTipBlock> {
    let parsed: ZebraBlock = block.raw_block_bytes.as_slice().zcash_deserialize_into()?;
    let coinbase = parsed
        .transactions
        .first()
        .ok_or_else(|| eyre!("tip block has no coinbase transaction"))?;
    Ok(SampledTipBlock {
        block_height: block.height,
        block_hash: block.hash,
        coinbase_transaction_id: TransactionId::from_bytes(coinbase.hash().0),
    })
}
