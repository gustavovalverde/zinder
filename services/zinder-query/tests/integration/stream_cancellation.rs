//! Stream-cancellation and concurrent-reader smoke for the native
//! `WalletQuery` server.
//!
//! These tests cover two production-blast-radius properties of the gRPC
//! adapter that the in-process trait tests cannot:
//!
//! - A client that disconnects mid-stream must not leave the server in a
//!   broken state. The next request through the same server has to succeed
//!   with no cooldown.
//! - The server must serve many concurrent streamed reads without
//!   serializing them or returning errors. The default deployment shape
//!   puts one `zinder-query` process behind several `zinder-compat-lightwalletd`
//!   processes, each fanning out wallet scan-back ranges over the same
//!   store; if the read path can't handle parallel streams, that topology
//!   is unusable.
//!
//! Runs under the standard `ci` profile. Tests open a private TCP listener
//! per run so they parallelize freely.

#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tonic::transport::Server;
use zinder_core::BlockHeight;
use zinder_proto::v1::wallet::{self, wallet_query_client::WalletQueryClient};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::{ChainEpochArtifacts, PrimaryChainStore};
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::synthetic_chain_epoch;

/// Number of pre-committed blocks the test store carries. Picked so each
/// streamed range yields multiple chunks but the fixture build stays fast.
const COMMITTED_BLOCK_COUNT: u32 = 32;

/// Number of concurrent streamed reads the parallel-readers test fans out.
///
/// Sized to exceed `tokio`'s default executor thread count on typical CI
/// hosts so the test would fail if the server quietly serialized streams.
const CONCURRENT_READER_COUNT: u32 = 16;

/// Bound on how long the parallel-readers test waits for all streams to
/// complete. Sized so a stuck server fails the test instead of hanging the
/// runner indefinitely.
const PARALLEL_READERS_DEADLINE: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn dropping_compact_block_range_stream_does_not_break_subsequent_requests() -> Result<()> {
    let server_addr = commit_store_and_spawn_grpc().await?;
    let mut client = WalletQueryClient::connect(server_addr).await?;

    let mut first_stream = client
        .compact_block_range(wallet::CompactBlockRangeRequest {
            start_height: 1,
            end_height: COMMITTED_BLOCK_COUNT,
            at_epoch: None,
        })
        .await?
        .into_inner();
    let first_chunk = first_stream
        .next()
        .await
        .ok_or_else(|| eyre!("server closed compact-block-range stream without any chunk"))??;
    assert!(
        first_chunk.compact_block.is_some(),
        "first stream's first chunk must carry a compact block"
    );

    // Drop the stream mid-flight. The server must observe the cancellation
    // and tear down its handler without poisoning the adapter.
    drop(first_stream);

    let mut second_stream = client
        .compact_block_range(wallet::CompactBlockRangeRequest {
            start_height: 1,
            end_height: COMMITTED_BLOCK_COUNT,
            at_epoch: None,
        })
        .await?
        .into_inner();
    let mut second_chunk_count: u32 = 0;
    while let Some(chunk) = second_stream.next().await {
        let chunk = chunk?;
        assert!(
            chunk.compact_block.is_some(),
            "second-stream chunk must carry a compact block"
        );
        second_chunk_count = second_chunk_count.saturating_add(1);
    }
    assert_eq!(
        second_chunk_count, COMMITTED_BLOCK_COUNT,
        "post-cancellation request must drain the full committed range"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_compact_block_range_readers_all_drain_to_completion() -> Result<()> {
    let server_addr = commit_store_and_spawn_grpc().await?;

    let reader_tasks: Vec<_> = (0..CONCURRENT_READER_COUNT)
        .map(|_| {
            let server_addr = server_addr.clone();
            tokio::spawn(async move {
                let mut client = WalletQueryClient::connect(server_addr).await?;
                let mut stream = client
                    .compact_block_range(wallet::CompactBlockRangeRequest {
                        start_height: 1,
                        end_height: COMMITTED_BLOCK_COUNT,
                        at_epoch: None,
                    })
                    .await?
                    .into_inner();
                let mut received: u32 = 0;
                while let Some(chunk) = stream.next().await {
                    let _chunk = chunk?;
                    received = received.saturating_add(1);
                }
                Ok::<u32, eyre::Report>(received)
            })
        })
        .collect();

    let collect_outcomes = async {
        let mut outcomes = Vec::with_capacity(reader_tasks.len());
        for task in reader_tasks {
            outcomes.push(task.await);
        }
        outcomes
    };
    let outcomes = tokio::time::timeout(PARALLEL_READERS_DEADLINE, collect_outcomes)
        .await
        .map_err(|_| {
            eyre!(
                "concurrent compact-block-range readers did not all complete within {:?}",
                PARALLEL_READERS_DEADLINE
            )
        })?;

    for outcome in outcomes {
        let received = outcome??;
        assert_eq!(
            received, COMMITTED_BLOCK_COUNT,
            "each concurrent reader must drain the full committed range; got {received}"
        );
    }
    Ok(())
}

async fn commit_store_and_spawn_grpc() -> Result<String> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_synthetic_chain(&store, COMMITTED_BLOCK_COUNT)?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    spawn_wallet_query_server(grpc_adapter).await
}

fn commit_synthetic_chain(store: &PrimaryChainStore, block_count: u32) -> Result<()> {
    for height in 1..=block_count {
        let (chain_epoch, block, compact_block) = synthetic_chain_epoch(u64::from(height), height);
        store.commit_chain_epoch(ChainEpochArtifacts::new(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
    }
    Ok(())
}

async fn spawn_wallet_query_server(
    grpc_adapter: WalletQueryGrpcAdapter<WalletQuery<PrimaryChainStore>>,
) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(grpc_adapter.into_server())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
    });
    // Avoid clippy::needless_pass_by_value; BlockHeight import keeps the
    // crate's documented vocabulary even though this helper does not use
    // it directly.
    let _ = BlockHeight::new(0);
    Ok(format!("http://{listen_addr}"))
}
