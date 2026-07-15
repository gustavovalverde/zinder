#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Live acceptance for `ExplorerQuery.TransactionDetail`.
//!
//! Bulk-catches-up a small window ending at the upstream tip, picks the coinbase
//! transaction from the tip block, and asserts that the federated explorer
//! response binds the [`zinder_core::TransactionPublicFacts`] shape consumers
//! depend on:
//!
//! - `freshness.chain_view.chain_epoch` is populated by the wallet-plane read.
//! - `facts.transaction_id` matches the txid we asked for.
//! - `facts.is_coinbase` is `true` and `facts.privacy_shape` is the
//!   coinbase classifier output.
//! - `facts.version.effective_version` matches the wire integer.
//! - `location.mined.location.block_height` matches the tip height the
//!   coinbase was sampled from.
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
use tonic::Request;
use tonic::transport::Channel;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::wire::{encode_rpc_transaction_id_hex, encode_zinder_native_chain_name};
use zinder_core::{BlockHeight, Network, TransactionId};
use zinder_explorer::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolIndex, run_bulk_catchup};
use zinder_proto::v1::explorer::{
    PrivacyShape as WirePrivacyShape, TransactionDetailRequest, TransactionVersionKind,
    explorer_query_server::ExplorerQuery as ExplorerQueryService,
};
use zinder_proto::v1::wallet::transaction_location;
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_source::{NodeSource as _, SourceBlock};
use zinder_store::{ChainStoreOptions, PrimaryChainStore, SecondaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_bulk_catchup_run_config,
    zebra_source_from_bulk_catchup,
};

/// Number of blocks below the tip to bulk catch up.
///
/// Wide enough to ensure the sampled coinbase has been committed; narrow
/// enough to keep the test under a minute on mainnet.
const BACKFILL_DEPTH_BLOCKS: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn transaction_detail_returns_typed_facts_for_sampled_coinbase() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = TransactionDetailFixture::open(&env).await?;
    let response = fixture.query_transaction_detail().await?;
    assert_response_invariants(&fixture, &response)?;
    fixture.shutdown().await;
    Ok(())
}

fn assert_response_invariants(
    fixture: &TransactionDetailFixture,
    response: &zinder_proto::v1::explorer::TransactionDetailResponse,
) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("response missing freshness envelope"))?;
    freshness
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| eyre!("freshness envelope missing chain_view.chain_epoch"))?;
    let facts = response
        .facts
        .as_ref()
        .ok_or_else(|| eyre!("response missing public facts"))?;
    assert_eq!(
        facts.transaction_id,
        encode_rpc_transaction_id_hex(fixture.coinbase_transaction_id),
        "transaction_id must match the txid the explorer was asked for",
    );
    assert!(facts.is_coinbase);
    let privacy_shape = WirePrivacyShape::try_from(facts.privacy_shape)
        .map_err(|error| eyre!("privacy_shape proto-decode failed: {error}"))?;
    assert!(matches!(
        privacy_shape,
        WirePrivacyShape::Coinbase | WirePrivacyShape::ShieldedCoinbase,
    ));
    let version = facts
        .version
        .as_ref()
        .ok_or_else(|| eyre!("missing version"))?;
    let version_kind = TransactionVersionKind::try_from(version.kind)
        .map_err(|error| eyre!("version.kind proto-decode failed: {error}"))?;
    assert!(!matches!(
        version_kind,
        TransactionVersionKind::Unspecified | TransactionVersionKind::Unsupported,
    ));
    assert_eq!(
        version.effective_version,
        version_to_effective_integer(version_kind)
    );
    let location = response
        .location
        .as_ref()
        .ok_or_else(|| eyre!("missing location"))?;
    let mined_block_location = match &location.location {
        Some(transaction_location::Location::Mined(mined)) => mined
            .location
            .as_ref()
            .ok_or_else(|| eyre!("mined transaction missing block location"))?,
        Some(
            transaction_location::Location::InMempool(_)
            | transaction_location::Location::Conflicting(_),
        )
        | None => {
            return Err(eyre!("expected mined location for tip-block coinbase"));
        }
    };
    assert_eq!(
        BlockHeight::new(mined_block_location.block_height),
        fixture.sample_block_height,
    );
    assert!(facts.size_bytes > 0);
    tracing::info!(
        target: "zinder::live",
        event = "transaction_detail_validated",
        network = %encode_zinder_native_chain_name(fixture.network),
        height = fixture.sample_block_height.value(),
        version = ?version_kind,
        privacy_shape = ?privacy_shape,
        size_bytes = facts.size_bytes,
        "explorer transaction detail validated against live node",
    );
    Ok(())
}

const fn version_to_effective_integer(kind: TransactionVersionKind) -> u32 {
    match kind {
        TransactionVersionKind::V1 => 1,
        TransactionVersionKind::V2 => 2,
        TransactionVersionKind::V3 => 3,
        TransactionVersionKind::V4 => 4,
        TransactionVersionKind::V5 => 5,
        TransactionVersionKind::V6 => 6,
        TransactionVersionKind::Unspecified | TransactionVersionKind::Unsupported => 0,
    }
}

struct TransactionDetailFixture {
    network: Network,
    coinbase_transaction_id: TransactionId,
    sample_block_height: BlockHeight,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _tempdir: TempDir,
}

impl TransactionDetailFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (tempdir, store, sample) = bulk_catchup_and_sample_tip_coinbase(env).await?;
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );

        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(network, store, MempoolIndex::new()).await?;
        let (wallet_grpc_addr, wallet_server_handle) =
            serve_wallet_query_grpc(wallet_query, format!("http://{ingest_control_addr}")).await?;
        let canonical_secondary = SecondaryChainStore::open(
            tempdir.path().join("zinder-store"),
            tempdir.path().join("zinder-store-secondary-explorer"),
            ChainStoreOptions::for_network(network),
        )?;
        canonical_secondary.try_catch_up()?;
        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_canonical_store(canonical_secondary)
                .with_wallet_query_endpoint(format!("http://{wallet_grpc_addr}"));

        Ok(Self {
            network,
            coinbase_transaction_id: sample.transaction_id,
            sample_block_height: sample.block_height,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            _tempdir: tempdir,
        })
    }

    async fn query_transaction_detail(
        &self,
    ) -> Result<zinder_proto::v1::explorer::TransactionDetailResponse> {
        let request = TransactionDetailRequest {
            transaction_id: encode_rpc_transaction_id_hex(self.coinbase_transaction_id),
            at_epoch_id: None,
        };
        let response =
            ExplorerQueryService::transaction_detail(&self.explorer_adapter, Request::new(request))
                .await?
                .into_inner();
        Ok(response)
    }

    async fn shutdown(&mut self) {
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
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
    let adapter =
        IngestControlGrpcAdapter::new(network, store, zinder_runtime::Readiness::default())
            .with_mempool(mempool_index);
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
struct SampledCoinbase {
    transaction_id: TransactionId,
    block_height: BlockHeight,
}

async fn bulk_catchup_and_sample_tip_coinbase(
    env: &LiveTestEnv,
) -> Result<(TempDir, PrimaryChainStore, SampledCoinbase)> {
    let tip_height = fetch_live_tip_height(env).await?;
    if tip_height.value() <= BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "tip height {} is at or below the minimum {BACKFILL_DEPTH_BLOCKS}; \
             upstream node is not synced or {network} is too young",
            tip_height.value(),
            network = encode_zinder_native_chain_name(env.network()),
        ));
    }
    let checkpoint_height = BlockHeight::new(tip_height.value() - BACKFILL_DEPTH_BLOCKS - 1);
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let activations = fetch_live_network_upgrade_activations(env).await?;
    let mut bulk_catchup_config = live_bulk_catchup_run_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(1000).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
        activations,
    );
    let source = zebra_source_from_bulk_catchup(&bulk_catchup_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    bulk_catchup_config.checkpoint = Some(checkpoint);
    run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| {
            eyre!(
                "expected committed bulk-catchup outcome against live {network}",
                network = encode_zinder_native_chain_name(env.network()),
            )
        })?;

    let tip_source_block = source.fetch_block_at(tip_height).await?;
    let sample = sample_tip_coinbase(&tip_source_block)?;

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, sample))
}

fn sample_tip_coinbase(block: &SourceBlock) -> Result<SampledCoinbase> {
    let parsed: ZebraBlock = block.raw_block_bytes.as_slice().zcash_deserialize_into()?;
    let coinbase = parsed
        .transactions
        .first()
        .ok_or_else(|| eyre!("tip block has no coinbase transaction"))?;
    let transaction_id = TransactionId::from_bytes(coinbase.hash().0);
    Ok(SampledCoinbase {
        transaction_id,
        block_height: block.height,
    })
}
