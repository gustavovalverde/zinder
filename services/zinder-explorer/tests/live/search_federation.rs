//! Live federation tests for [`ExplorerQuery::Search`].
//!
//! Validates the typed search surface end to end against a real upstream
//! node: classification, wallet-side confirmation for hash and height
//! candidates, and the privacy refusal arms for shielded inputs. The
//! classifier in `zinder-core` is exercised by unit tests; this file
//! covers the federated handler in `services/zinder-explorer/src/grpc/search.rs`.

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
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto as _;
use zinder_core::wire::{
    encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex, encode_zinder_native_chain_name,
};
use zinder_core::{BlockHash, BlockHeight, Network, TransactionId};
use zinder_explorer::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
use zinder_ingest::{IngestControlGrpcAdapter, MempoolIndex, run_bulk_catchup};
use zinder_proto::capabilities::EXPLORER_SEARCH_V1;
use zinder_proto::v1::explorer::{
    NotPubliclyIndexableReason, SearchRequest, SearchResponse,
    explorer_query_server::ExplorerQuery as ExplorerQueryService, search_candidate,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_source::{NodeSource as _, SourceBlock};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_bulk_catchup_run_config,
    zebra_source_from_bulk_catchup,
};

const BACKFILL_DEPTH_BLOCKS: u32 = 50;
const REGTEST_TRANSPARENT_ADDRESS: &str = "tmDpFafuBHKGUYmuwLsrxWJrwcnSyzEEtYx";
const MAINNET_TRANSPARENT_ADDRESS: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
const MAINNET_SAPLING_ADDRESS: &str =
    "zs1z7rejlpsa98s2rrrfkwmaxu53e4ue0ulcrw0h4x5g8jl04tak0d3mm47vdtahatqrlkngh9slya";
const VIEWING_KEY_LIKE_PREFIX: &str = "uivk1examplenotarealkeyjustaprefix";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn search_classifies_block_height_transaction_id_and_addresses() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = SearchFixture::open(&env).await?;
    let network = fixture.network;
    let tip = fixture.sample_block_height.value();

    assert_block_height_resolves(&fixture, tip).await?;
    assert_block_hash_resolves(&fixture).await?;
    assert_transaction_id_resolves(&fixture, tip).await?;
    assert_transparent_address_resolves(&fixture, network).await?;

    tracing::info!(
        target: "zinder::live",
        event = "search_classifies_validated",
        network = %encode_zinder_native_chain_name(network),
        sampled_height = tip,
        "explorer search validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

async fn assert_block_height_resolves(fixture: &SearchFixture, tip: u32) -> Result<()> {
    let response = fixture.search(&tip.to_string()).await?;
    assert_freshness(&response)?;
    let block_candidate = find_block_match(&response)
        .ok_or_else(|| eyre!("expected BlockMatch arm for height {tip}"))?;
    assert_eq!(block_candidate.block_height, tip);
    assert_eq!(
        block_candidate.block_hash,
        encode_rpc_block_hash_hex(fixture.sample_block_hash)
    );
    Ok(())
}

async fn assert_block_hash_resolves(fixture: &SearchFixture) -> Result<()> {
    let query = encode_rpc_block_hash_hex(fixture.sample_block_hash);
    let response = fixture.search(&query).await?;
    assert!(
        find_block_match(&response).is_some(),
        "expected BlockMatch arm for block hash {query}",
    );
    Ok(())
}

async fn assert_transaction_id_resolves(fixture: &SearchFixture, tip: u32) -> Result<()> {
    let query = encode_rpc_transaction_id_hex(fixture.sample_coinbase_id);
    let response = fixture.search(&query).await?;
    let transaction = find_transaction_match(&response)
        .ok_or_else(|| eyre!("expected TransactionMatch arm for coinbase {query}"))?;
    assert!(!transaction.in_mempool, "coinbase is mined");
    assert_eq!(transaction.mined_block_height, tip);
    Ok(())
}

async fn assert_transparent_address_resolves(
    fixture: &SearchFixture,
    network: Network,
) -> Result<()> {
    let query = transparent_query_for_network(network)?;
    let response = fixture.search(query).await?;
    let transparent = find_transparent_match(&response)
        .ok_or_else(|| eyre!("expected TransparentAddress arm for {query}"))?;
    assert_eq!(transparent.canonical_form, query);
    assert!(transparent.is_p2pkh);
    Ok(())
}

fn find_block_match(response: &SearchResponse) -> Option<&zinder_proto::v1::explorer::BlockMatch> {
    response.candidates.iter().find_map(|candidate| {
        if let Some(search_candidate::Match::Block(block_match)) = candidate.r#match.as_ref() {
            Some(block_match)
        } else {
            None
        }
    })
}

fn find_transaction_match(
    response: &SearchResponse,
) -> Option<&zinder_proto::v1::explorer::TransactionMatch> {
    response.candidates.iter().find_map(|candidate| {
        if let Some(search_candidate::Match::Transaction(transaction_match)) =
            candidate.r#match.as_ref()
        {
            Some(transaction_match)
        } else {
            None
        }
    })
}

fn find_transparent_match(
    response: &SearchResponse,
) -> Option<&zinder_proto::v1::explorer::TransparentAddressMatch> {
    response.candidates.iter().find_map(|candidate| {
        if let Some(search_candidate::Match::TransparentAddress(transparent_match)) =
            candidate.r#match.as_ref()
        {
            Some(transparent_match)
        } else {
            None
        }
    })
}

fn find_shielded_match(
    response: &SearchResponse,
) -> Option<&zinder_proto::v1::explorer::ShieldedAddressMatch> {
    response.candidates.iter().find_map(|candidate| {
        if let Some(search_candidate::Match::ShieldedAddress(shielded_match)) =
            candidate.r#match.as_ref()
        {
            Some(shielded_match)
        } else {
            None
        }
    })
}

fn find_viewing_key_match(
    response: &SearchResponse,
) -> Option<&zinder_proto::v1::explorer::ViewingKeyMatch> {
    response.candidates.iter().find_map(|candidate| {
        if let Some(search_candidate::Match::ViewingKey(viewing_key_match)) =
            candidate.r#match.as_ref()
        {
            Some(viewing_key_match)
        } else {
            None
        }
    })
}

fn response_routes_to_public_history_arm(response: &SearchResponse) -> bool {
    response.candidates.iter().any(|candidate| {
        matches!(
            candidate.r#match.as_ref(),
            Some(
                search_candidate::Match::TransparentAddress(_)
                    | search_candidate::Match::TexAddress(_)
                    | search_candidate::Match::UnifiedAddress(_)
                    | search_candidate::Match::Block(_)
                    | search_candidate::Match::Transaction(_),
            )
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn search_refuses_shielded_address_and_viewing_key_without_canonical_form() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
    let mut fixture = SearchFixture::open(&env).await?;
    let shielded_response = fixture.search(MAINNET_SAPLING_ADDRESS).await?;
    if let Some(shielded_match) = find_shielded_match(&shielded_response) {
        let refusal = shielded_match
            .not_publicly_indexable
            .as_ref()
            .ok_or_else(|| eyre!("ShieldedAddressMatch missing NotPubliclyIndexable body"))?;
        let reason = NotPubliclyIndexableReason::try_from(refusal.reason)
            .map_err(|_| eyre!("unknown NotPubliclyIndexableReason code {}", refusal.reason))?;
        assert!(
            matches!(
                reason,
                NotPubliclyIndexableReason::ShieldedAddress
                    | NotPubliclyIndexableReason::ShieldedAddressMainnet
                    | NotPubliclyIndexableReason::ShieldedAddressTestnet,
            ),
            "expected a shielded-address refusal variant, got {reason:?}",
        );
        assert!(
            !refusal.human_reason.is_empty(),
            "human_reason must carry the canonical refusal string",
        );
    } else {
        // Mainnet Sapling does not parse on non-mainnet networks; the
        // classifier surfaces an Unclassified hint instead. Either
        // outcome is privacy-preserving (no probe, no echoed key).
        assert!(
            !response_routes_to_public_history_arm(&shielded_response),
            "shielded-address input must never route to a public-history arm",
        );
    }

    let viewing_response = fixture.search(VIEWING_KEY_LIKE_PREFIX).await?;
    let viewing_key_match = find_viewing_key_match(&viewing_response)
        .ok_or_else(|| eyre!("expected ViewingKeyMatch arm for {VIEWING_KEY_LIKE_PREFIX}"))?;
    let viewing_refusal = viewing_key_match
        .not_publicly_indexable
        .as_ref()
        .ok_or_else(|| eyre!("ViewingKeyMatch missing NotPubliclyIndexable body"))?;
    assert_eq!(
        viewing_refusal.reason,
        NotPubliclyIndexableReason::ViewingKey as i32,
    );
    assert!(
        viewing_refusal.canonical_form.is_none(),
        "viewing keys must never round-trip through canonical_form",
    );
    assert!(
        !response_routes_to_public_history_arm(&viewing_response),
        "viewing-key input must never route to a public-history arm",
    );

    fixture.shutdown().await;
    Ok(())
}

fn transparent_query_for_network(network: Network) -> Result<&'static str> {
    match network {
        Network::ZcashMainnet => Ok(MAINNET_TRANSPARENT_ADDRESS),
        Network::ZcashTestnet | Network::ZcashRegtest => Ok(REGTEST_TRANSPARENT_ADDRESS),
        other => Err(eyre!(
            "transparent_query_for_network called with unsupported network: {other:?}"
        )),
    }
}

struct SearchFixture {
    network: Network,
    sample_block_height: BlockHeight,
    sample_block_hash: BlockHash,
    sample_coinbase_id: TransactionId,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _store_tempdir: TempDir,
}

impl SearchFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (store_tempdir, store, sample) = bulk_catchup_and_sample_tip(env).await?;
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

        let explorer_adapter =
            ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings { network })
                .with_wallet_query_endpoint(wallet_endpoint);

        Ok(Self {
            network,
            sample_block_height: sample.block_height,
            sample_block_hash: sample.block_hash,
            sample_coinbase_id: sample.coinbase_transaction_id,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            _store_tempdir: store_tempdir,
        })
    }

    async fn search(&self, query: &str) -> Result<SearchResponse> {
        let response = ExplorerQueryService::search(
            &self.explorer_adapter,
            Request::new(SearchRequest {
                query: query.to_owned(),
            }),
        )
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

fn assert_freshness(response: &SearchResponse) -> Result<()> {
    let freshness = response
        .freshness
        .as_ref()
        .ok_or_else(|| eyre!("search response missing freshness"))?;
    assert_eq!(freshness.capability_version, EXPLORER_SEARCH_V1);
    assert!(
        freshness
            .chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
            .is_some(),
        "search freshness must carry a chain epoch",
    );
    Ok(())
}

struct SampleBlock {
    block_height: BlockHeight,
    block_hash: BlockHash,
    coinbase_transaction_id: TransactionId,
}

async fn bulk_catchup_and_sample_tip(
    env: &LiveTestEnv,
) -> Result<(TempDir, PrimaryChainStore, SampleBlock)> {
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
        .ok_or_else(|| eyre!("expected committed bulk-catchup outcome"))?;
    let tip_source_block = source.fetch_block_at(tip_height).await?;
    let sample = sample_tip(&tip_source_block)?;
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, sample))
}

fn sample_tip(block: &SourceBlock) -> Result<SampleBlock> {
    let parsed: ZebraBlock = block.raw_block_bytes.as_slice().zcash_deserialize_into()?;
    let coinbase = parsed
        .transactions
        .first()
        .ok_or_else(|| eyre!("tip block has no coinbase transaction"))?;
    Ok(SampleBlock {
        block_height: block.height,
        block_hash: block.hash,
        coinbase_transaction_id: TransactionId::from_bytes(coinbase.hash().0),
    })
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
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(eyre!("gRPC endpoint {addr} did not become reachable"))
}
