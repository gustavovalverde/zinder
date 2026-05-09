#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Network-agnostic acceptance for the M5 Slice B federated transparent
//! address balance read path.
//!
//! The test backfills a small window ending at the upstream tip, samples one
//! transparent coinbase output from the tip block, derives its
//! `address_script_hash` with the same `SHA-256(scriptPubKey)` rule the
//! ingest pipeline uses, and asserts that:
//!
//! - `WalletQueryApi::transparent_address_utxos` and
//!   `ExplorerQuery::TransparentAddressBalance` agree on `confirmed_zat` for
//!   the same address; the federated balance is the sum of the visible UTXO
//!   values.
//! - The federated response binds the same `chain_epoch` the wallet read
//!   answered against.
//! - With no mempool federation wired (no `IngestControl` proxy on the
//!   `WalletQuery` adapter), `unconfirmed_delta_zat` is zero. The Shape C
//!   compute path falls through cleanly when the mempool point lookups are
//!   unavailable.
//!
//! Mainnet is opt-in: the test reads `ZINDER_NETWORK` and dispatches via
//! `require_live_for(...)`. Operators set `ZINDER_NETWORK=zcash-mainnet`
//! plus the standard `ZINDER_NODE__*` schema.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::time::Duration;

use eyre::{Result, eyre};
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Request;
use tonic::transport::Channel;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::{
    BlockHeight, BroadcastAccepted, ChainEpoch, ChainEpochId, Network, RawTransactionBytes,
    TransactionBroadcastResult, TransactionId, TransparentAddressScriptHash, UnixTimestampMillis,
};
use zinder_derive::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
use zinder_ingest::{
    BackfillOutcome, IngestControlGrpcAdapter, MempoolApplyOutcome, MempoolIndex, backfill,
    build_mempool_entry,
};
use zinder_proto::v1::explorer::explorer_query_server::ExplorerQuery as ExplorerQueryService;
use zinder_proto::v1::wallet::{
    AddressLookup, TransparentAddressBalanceRequest, TransparentAddressBalanceResponse,
    address_lookup,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter};
use zinder_source::{
    MempoolSourceEntry, NodeSource as _, SourceBlock, TransactionBroadcaster, ZebraJsonRpcSource,
    ZebraJsonRpcSourceOptions,
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{LiveTestEnv, init, require_live_for};
use zinder_testkit::{P2pkhSpendArgs, TransparentAddress, TransparentTestKey};

use crate::common::{
    fetch_live_tip_height, live_backfill_config, regtest_generate_blocks,
    zebra_source_from_backfill,
};

/// Number of blocks below the tip to backfill.
///
/// Small enough to keep the test under a minute against mainnet; large enough
/// that the sampled coinbase has been finalized by the time the federated
/// balance reads it back.
const BACKFILL_DEPTH_BLOCKS: u32 = 50;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn federated_balance_matches_utxo_sum_for_sampled_coinbase_address() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?;
    let mut fixture = FederatedBalanceFixture::open(&env).await?;
    let response = fixture.query_federated_balance().await?;

    assert_eq!(
        response.confirmed_zat, fixture.expected_confirmed_zat,
        "federated confirmed_zat must match the sum of visible UTXOs for the sampled address",
    );
    assert_eq!(
        response.unconfirmed_delta_zat, 0,
        "no IngestControl proxy is wired on this WalletQuery adapter; \
         the Shape C mempool overlay must fall through to zero",
    );
    assert_eq!(response.address_count, 1);
    let chain_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("federated balance response missing chain_epoch"))?;
    assert_eq!(
        chain_epoch.chain_epoch_id,
        fixture.expected_chain_epoch_id.value(),
        "federated chain_epoch must bind to the same epoch the wallet UTXO read answered",
    );

    tracing::info!(
        target: "zinder::live",
        event = "federated_balance_validated",
        network = %fixture.network.name(),
        height = fixture.sample_block_height.value(),
        confirmed_zat = response.confirmed_zat,
        chain_epoch_id = chain_epoch.chain_epoch_id,
        "federated transparent address balance validated against live node",
    );

    fixture.shutdown().await;
    Ok(())
}

struct FederatedBalanceFixture {
    network: Network,
    address_script_hash: TransparentAddressScriptHash,
    sample_block_height: BlockHeight,
    expected_confirmed_zat: u64,
    expected_chain_epoch_id: ChainEpochId,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    _tempdir: TempDir,
}

impl FederatedBalanceFixture {
    async fn open(env: &LiveTestEnv) -> Result<Self> {
        let network = env.network();
        let (tempdir, store, sample) = backfill_and_sample_tip_coinbase(env).await?;
        let wallet_query = WalletQuery::new(store.clone(), ());
        let utxos = wallet_query
            .transparent_address_utxos(
                zinder_query::TransparentAddressUtxosRequest {
                    address_script_hash: sample.address_script_hash,
                    start_height: sample.backfill_from_height,
                    max_entries: NonZeroU32::new(1024)
                        .ok_or_else(|| eyre!("invalid max entries"))?,
                    from_cursor: None,
                },
                None,
            )
            .await?;
        let expected_confirmed_zat = utxos
            .utxos
            .iter()
            .map(|utxo| utxo.value_zat)
            .fold(0_u64, u64::saturating_add);
        let expected_chain_epoch_id = utxos.chain_epoch.id;

        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(network, store, MempoolIndex::new()).await?;
        let (wallet_grpc_addr, wallet_server_handle) =
            serve_wallet_query_grpc(wallet_query, format!("http://{ingest_control_addr}")).await?;
        let explorer_adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
            network: network.name().to_owned(),
        })
        .with_wallet_query_endpoint(format!("http://{wallet_grpc_addr}"));
        Ok(Self {
            network,
            address_script_hash: sample.address_script_hash,
            sample_block_height: sample.block_height,
            expected_confirmed_zat,
            expected_chain_epoch_id,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            _tempdir: tempdir,
        })
    }

    async fn query_federated_balance(&self) -> Result<TransparentAddressBalanceResponse> {
        let request = TransparentAddressBalanceRequest {
            addresses: vec![AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    self.address_script_hash.as_bytes().to_vec(),
                )),
            }],
            at_epoch: None,
        };
        let response = ExplorerQueryService::transparent_address_balance(
            &self.explorer_adapter,
            Request::new(request),
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

/// Sampled output from the tip block used as the federated balance target.
#[derive(Clone, Debug)]
struct SampledCoinbase {
    address_script_hash: TransparentAddressScriptHash,
    block_height: BlockHeight,
    backfill_from_height: BlockHeight,
}

async fn backfill_and_sample_tip_coinbase(
    env: &LiveTestEnv,
) -> Result<(TempDir, PrimaryChainStore, SampledCoinbase)> {
    let tip_height = fetch_live_tip_height(env).await?;
    if tip_height.value() <= BACKFILL_DEPTH_BLOCKS {
        return Err(eyre!(
            "tip height {} is at or below the minimum {BACKFILL_DEPTH_BLOCKS}; \
             upstream node is not synced or {network} is too young",
            tip_height.value(),
            network = env.network().name(),
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
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);
    let BackfillOutcome::Committed(_) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!(
            "expected committed backfill outcome against live {network}",
            network = env.network().name(),
        ));
    };

    let tip_source_block = source.fetch_block_by_height(tip_height).await?;
    let sample = sample_first_transparent_coinbase_output(&tip_source_block, from_height)?;

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store, sample))
}

fn sample_first_transparent_coinbase_output(
    block: &SourceBlock,
    backfill_from_height: BlockHeight,
) -> Result<SampledCoinbase> {
    let parsed: ZebraBlock = block.raw_block_bytes.as_slice().zcash_deserialize_into()?;
    let coinbase = parsed
        .transactions
        .first()
        .ok_or_else(|| eyre!("tip block has no coinbase transaction"))?;
    let outputs = coinbase.outputs();
    let (_, output) = outputs
        .iter()
        .enumerate()
        .find(|(_, output)| !output.lock_script.as_raw_bytes().is_empty())
        .ok_or_else(|| eyre!("tip coinbase has no transparent outputs"))?;
    let script_pub_key = output.lock_script.as_raw_bytes().to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&script_pub_key);
    let address_script_hash = TransparentAddressScriptHash::from_bytes(hasher.finalize().into());
    Ok(SampledCoinbase {
        address_script_hash,
        block_height: block.height,
        backfill_from_height,
    })
}

/// Test seed for the mempool overlay test's transparent test key.
///
/// Matches `services/zinder-ingest/tests/live/mempool_broadcast_cycle.rs`.
/// The regtest sidecar must mine to the address derived from this seed.
const MEMPOOL_OVERLAY_TEST_SEED: [u8; 32] = [0x42_u8; 32];

/// Number of blocks to mine before sampling a spendable coinbase.
///
/// 101 puts the first newly mined block at `tip + 1` with 100 confirmations
/// after the loop, so the funded coinbase is mature for the live spend.
const MEMPOOL_OVERLAY_MINE_COUNT: u32 = 101;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn federated_balance_subtracts_pending_spend_overlay() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[Network::ZcashRegtest])?;
    let test_key = TransparentTestKey::from_seed(&MEMPOOL_OVERLAY_TEST_SEED)
        .map_err(|error| eyre!("could not derive test key: {error}"))?;
    let test_address = test_key.address_base58();
    tracing::info!(
        target: "zinder::live",
        event = "mempool_overlay_test_address",
        address = %test_address,
        "regtest mempool overlay: configure mining.miner_address to this value",
    );
    let fixture = MempoolOverlayFixture::open(&env, &test_key, &test_address).await?;
    let outcome = assert_baseline_then_overlay(&fixture).await;
    fixture.shutdown().await;
    outcome
}

struct MempoolOverlayFixture {
    address_script_hash: TransparentAddressScriptHash,
    funded_value_zat: u64,
    explorer_adapter: ExplorerQueryGrpcAdapter,
    wallet_server_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    ingest_control_handle: JoinHandle<Result<(), tonic::transport::Error>>,
    visible_chain_epoch: ChainEpoch,
    pending_entry: zinder_core::MempoolEntry,
    _tempdir: TempDir,
}

impl MempoolOverlayFixture {
    async fn open(
        env: &LiveTestEnv,
        test_key: &TransparentTestKey,
        test_address: &str,
    ) -> Result<Self> {
        let json_rpc = zebra_source(env)?;
        let tip_before = json_rpc.tip_id().await?.height;
        regtest_generate_blocks(env, MEMPOOL_OVERLAY_MINE_COUNT).await?;
        let funded_height = BlockHeight::new(tip_before.value() + 1);
        let target_height = tip_before.value() + MEMPOOL_OVERLAY_MINE_COUNT + 1;
        let coinbase =
            locate_test_coinbase(&json_rpc, test_key, test_address, funded_height).await?;
        let (tempdir, store) = backfill_after_mining(env, tip_before).await?;

        let recipient = scratch_recipient_address(test_key);
        let raw_tx = test_key
            .build_p2pkh_spend(&P2pkhSpendArgs {
                coinbase_txid_be: coinbase.txid_be,
                coinbase_vout: coinbase.vout,
                coinbase_value_zats: coinbase.value_zats,
                recipient: &recipient,
                target_height,
            })
            .map_err(|error| eyre!("transparent signer rejected the spend: {error}"))?;
        let address_script_hash = sha256_address_script_hash(&test_key.address_script_bytes());
        let broadcast_txid = broadcast_signed_spend(&json_rpc, raw_tx.clone()).await?;
        let mempool_index = MempoolIndex::new();
        let visible_chain_epoch = visible_chain_epoch(&store)?;
        let pending_entry = hydrate_and_apply_pending_spend(
            &mempool_index,
            broadcast_txid,
            raw_tx,
            visible_chain_epoch,
        )?;

        let wallet_query = WalletQuery::new(store.clone(), ());
        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(env.network(), store, mempool_index.clone()).await?;
        let (wallet_grpc_addr, wallet_server_handle) =
            serve_wallet_query_grpc(wallet_query, format!("http://{ingest_control_addr}")).await?;
        let explorer_adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
            network: env.network().name().to_owned(),
        })
        .with_wallet_query_endpoint(format!("http://{wallet_grpc_addr}"));
        Ok(Self {
            address_script_hash,
            funded_value_zat: coinbase.value_zats,
            explorer_adapter,
            wallet_server_handle,
            ingest_control_handle,
            visible_chain_epoch,
            pending_entry,
            _tempdir: tempdir,
        })
    }

    async fn query_federated_balance(&self) -> Result<TransparentAddressBalanceResponse> {
        let request = TransparentAddressBalanceRequest {
            addresses: vec![AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    self.address_script_hash.as_bytes().to_vec(),
                )),
            }],
            at_epoch: None,
        };
        let response = ExplorerQueryService::transparent_address_balance(
            &self.explorer_adapter,
            Request::new(request),
        )
        .await?
        .into_inner();
        Ok(response)
    }

    async fn shutdown(mut self) {
        self.wallet_server_handle.abort();
        self.ingest_control_handle.abort();
        let _ = (&mut self.wallet_server_handle).await;
        let _ = (&mut self.ingest_control_handle).await;
    }
}

async fn assert_baseline_then_overlay(fixture: &MempoolOverlayFixture) -> Result<()> {
    let response = fixture.query_federated_balance().await?;
    let confirmed_pre = response.confirmed_zat;
    assert!(
        confirmed_pre >= fixture.funded_value_zat,
        "confirmed_zat must include the funded coinbase before any spend is mined: \
         confirmed_pre={confirmed_pre}, funded_value_zat={funded_value_zat}",
        funded_value_zat = fixture.funded_value_zat,
    );
    let signed_funded = i64::try_from(fixture.funded_value_zat)
        .map_err(|_| eyre!("funded coinbase value did not fit i64"))?;
    assert_eq!(
        response.unconfirmed_delta_zat,
        signed_funded.saturating_neg(),
        "federated unconfirmed_delta_zat must equal the negated value of the funded coinbase \
         being spent in mempool",
    );
    let chain_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("federated overlay response missing chain_epoch"))?;
    assert_eq!(
        chain_epoch.chain_epoch_id,
        fixture.visible_chain_epoch.id.value(),
    );

    tracing::info!(
        target: "zinder::live",
        event = "mempool_overlay_validated",
        confirmed_zat = confirmed_pre,
        unconfirmed_delta_zat = response.unconfirmed_delta_zat,
        pending_txid = %hex::encode(fixture.pending_entry.transaction_id.as_bytes()),
        "federated balance overlay reflects the mempool spend",
    );
    Ok(())
}

fn zebra_source(env: &LiveTestEnv) -> Result<ZebraJsonRpcSource> {
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

struct TestCoinbase {
    txid_be: [u8; 32],
    vout: u32,
    value_zats: u64,
}

async fn locate_test_coinbase(
    json_rpc: &ZebraJsonRpcSource,
    test_key: &TransparentTestKey,
    test_address: &str,
    funded_height: BlockHeight,
) -> Result<TestCoinbase> {
    let block = json_rpc.fetch_block_by_height(funded_height).await?;
    let parsed: ZebraBlock = block.raw_block_bytes.as_slice().zcash_deserialize_into()?;
    let coinbase_tx = parsed
        .transactions
        .first()
        .ok_or_else(|| eyre!("block has no coinbase transaction"))?;
    let txid_be: [u8; 32] = coinbase_tx.hash().0;
    let expected_script = test_key.address_script_bytes();
    for (vout_index, output) in coinbase_tx.outputs().iter().enumerate() {
        if output.lock_script.as_raw_bytes() == expected_script.as_slice() {
            let value_zats = u64::try_from(i64::from(output.value))
                .map_err(|error| eyre!("coinbase output value did not fit u64: {error}"))?;
            let vout = u32::try_from(vout_index)
                .map_err(|error| eyre!("coinbase output index did not fit u32: {error}"))?;
            return Ok(TestCoinbase {
                txid_be,
                vout,
                value_zats,
            });
        }
    }
    Err(eyre!(
        "block {} has no coinbase output paying to the test address {test_address}; \
         configure Zebra `[mining] miner_address` to that value and restart the sidecar",
        funded_height.value()
    ))
}

fn scratch_recipient_address(test_key: &TransparentTestKey) -> TransparentAddress {
    let funded_hash = match test_key.address() {
        TransparentAddress::PublicKeyHash(hash) | TransparentAddress::ScriptHash(hash) => *hash,
    };
    let mut scratch = [0_u8; 20];
    for (index, byte) in funded_hash.iter().enumerate() {
        scratch[index] = byte ^ 0xFF;
    }
    TransparentAddress::PublicKeyHash(scratch)
}

async fn backfill_after_mining(
    env: &LiveTestEnv,
    tip_before_mining: BlockHeight,
) -> Result<(TempDir, PrimaryChainStore)> {
    let tip_height = fetch_live_tip_height(env).await?;
    let checkpoint_height = tip_before_mining;
    let from_height = BlockHeight::new(checkpoint_height.value() + 1);
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let mut backfill_config = live_backfill_config(
        env,
        &storage_path,
        from_height,
        tip_height,
        NonZeroU32::new(100).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&backfill_config)?;
    let checkpoint = source.fetch_chain_checkpoint(checkpoint_height).await?;
    backfill_config.checkpoint = Some(checkpoint);
    let BackfillOutcome::Committed(_) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!("expected committed backfill outcome on regtest"));
    };
    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    Ok((tempdir, store))
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TransactionBroadcastResult is #[non_exhaustive]; this test collapses every non-Accepted variant into a single failure"
)]
async fn broadcast_signed_spend(
    json_rpc: &ZebraJsonRpcSource,
    raw_tx: Vec<u8>,
) -> Result<TransactionId> {
    let outcome = json_rpc
        .broadcast_transaction(RawTransactionBytes::new(raw_tx))
        .await?;
    match outcome {
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id }) => {
            Ok(transaction_id)
        }
        other => Err(eyre!(
            "Zebra rejected the signed transparent v5 transaction: {other:?}"
        )),
    }
}

fn visible_chain_epoch(store: &PrimaryChainStore) -> Result<ChainEpoch> {
    store
        .current_chain_epoch()?
        .ok_or_else(|| eyre!("backfilled store has no visible chain epoch"))
}

fn hydrate_and_apply_pending_spend(
    mempool_index: &MempoolIndex,
    broadcast_txid: TransactionId,
    raw_tx: Vec<u8>,
    visible_chain_epoch: ChainEpoch,
) -> Result<zinder_core::MempoolEntry> {
    let entry = build_mempool_entry(
        MempoolSourceEntry {
            transaction_id: broadcast_txid,
            auth_digest: None,
            raw_transaction_bytes: RawTransactionBytes::new(raw_tx),
            observed_at_unix_millis: UnixTimestampMillis::now(),
        },
        visible_chain_epoch,
    )?;
    let outcome = mempool_index.apply_added(entry.clone());
    if !matches!(outcome, MempoolApplyOutcome::Applied) {
        return Err(eyre!(
            "MempoolIndex::apply_added returned {outcome:?} for fresh broadcast txid"
        ));
    }
    Ok(entry)
}

fn sha256_address_script_hash(script_pub_key: &[u8]) -> TransparentAddressScriptHash {
    let mut hasher = Sha256::new();
    hasher.update(script_pub_key);
    TransparentAddressScriptHash::from_bytes(hasher.finalize().into())
}

async fn await_grpc_endpoint(addr: SocketAddr) -> Result<()> {
    let endpoint = format!("http://{addr}");
    for _ in 0..40 {
        if Channel::from_shared(endpoint.clone())?
            .connect()
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre!("WalletQuery gRPC server did not accept connections"))
}
