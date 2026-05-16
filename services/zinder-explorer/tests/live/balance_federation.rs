#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

//! Network-agnostic acceptance for the federated transparent-address
//! balance read path.
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
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Request;
use tonic::transport::Channel;
use zebra_chain::block::Block as ZebraBlock;
use zebra_chain::serialization::ZcashDeserializeInto;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{
    BlockHeight, BroadcastAccepted, ChainEpoch, ChainEpochId, Network, RawTransactionBytes,
    TransactionBroadcastResult, TransactionId, TransparentAddressScriptHash, UnixTimestampMillis,
};
use zinder_explorer::{ExplorerQueryGrpcAdapter, ExplorerServerInfoSettings};
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
use zinder_testkit::{
    P2pkhSpendArgs, TransparentAddress, TransparentTestKey, ZIP317_FEE_ONE_IN_ONE_OUT_ZATS,
    local_network_from_activations, sample_regtest_upgrade_activations,
};

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
    let Some(env) = require_live_for(&[
        Network::ZcashRegtest,
        Network::ZcashTestnet,
        Network::ZcashMainnet,
    ])?
    else {
        return Ok(());
    };
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
        network = %encode_zinder_native_chain_name(fixture.network),
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
        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );
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
            network: encode_zinder_native_chain_name(network).to_owned(),
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
            network = encode_zinder_native_chain_name(env.network()),
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
        return Err(eyre!(
            "expected committed backfill outcome against live {network}",
            network = encode_zinder_native_chain_name(env.network()),
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
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };
    let probe_config = live_backfill_config(
        &env,
        std::path::Path::new("/tmp/zinder-mempool-overlay-schedule-probe"),
        BlockHeight::new(1),
        BlockHeight::new(1),
        NonZeroU32::new(1).ok_or_else(|| eyre!("invalid probe batch size"))?,
        false,
    );
    let schedule = zebra_source_from_backfill(&probe_config)?
        .fetch_network_upgrade_activations()
        .await
        .map_err(|error| eyre!("could not fetch node-advertised upgrade schedule: {error}"))?;
    let test_key = TransparentTestKey::from_seed_with_local_network(
        &MEMPOOL_OVERLAY_TEST_SEED,
        local_network_from_activations(&schedule),
    )
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
    let _ = regtest_generate_blocks(&env, 1).await;
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
        let coinbase = locate_spendable_test_coinbase(env, &json_rpc, test_address).await?;
        let recipient = scratch_recipient_address(test_key, UnixTimestampMillis::now().value());
        let raw_tx = test_key
            .build_p2pkh_spend(&P2pkhSpendArgs {
                coinbase_txid_be: coinbase.txid_be,
                coinbase_vout: coinbase.vout,
                coinbase_value_zats: coinbase.value_zats,
                recipient: &recipient,
                target_height: coinbase.target_height,
            })
            .map_err(|error| eyre!("transparent signer rejected the spend: {error}"))?;
        let address_script_hash = sha256_address_script_hash(&test_key.address_script_bytes());
        let broadcast_txid = broadcast_signed_spend(&json_rpc, raw_tx.clone()).await?;
        let (tempdir, store) = backfill_from_coinbase(env, coinbase.height).await?;
        let mempool_index = MempoolIndex::new();
        let visible_chain_epoch = visible_chain_epoch(&store)?;
        let pending_entry = hydrate_and_apply_pending_spend(
            &mempool_index,
            broadcast_txid,
            raw_tx,
            visible_chain_epoch,
        )?;

        let wallet_query = WalletQuery::new(
            store.clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        );
        let (ingest_control_addr, ingest_control_handle) =
            serve_ingest_control_grpc(env.network(), store, mempool_index.clone()).await?;
        let (wallet_grpc_addr, wallet_server_handle) =
            serve_wallet_query_grpc(wallet_query, format!("http://{ingest_control_addr}")).await?;
        let explorer_adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings {
            network: encode_zinder_native_chain_name(env.network()).to_owned(),
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
    height: BlockHeight,
    target_height: u32,
}

#[derive(Debug, Deserialize)]
struct ZebraAddressUtxo {
    txid: String,
    #[serde(rename = "outputIndex")]
    output_index: u32,
    satoshis: u64,
    height: u32,
}

async fn locate_spendable_test_coinbase(
    env: &LiveTestEnv,
    json_rpc: &ZebraJsonRpcSource,
    test_address: &str,
) -> Result<TestCoinbase> {
    if let Some(coinbase) = select_spendable_test_coinbase(env, json_rpc, test_address).await? {
        return Ok(coinbase);
    }

    regtest_generate_blocks(env, MEMPOOL_OVERLAY_MINE_COUNT).await?;
    select_spendable_test_coinbase(env, json_rpc, test_address)
        .await?
        .ok_or_else(|| {
            eyre!(
                "no matured UTXO for test address {test_address} exceeds the ZIP-317 fee; \
                 configure Zebra `[mining] miner_address` to that value and restart from a \
                 fresh or funded regtest sidecar"
            )
        })
}

async fn select_spendable_test_coinbase(
    env: &LiveTestEnv,
    json_rpc: &ZebraJsonRpcSource,
    test_address: &str,
) -> Result<Option<TestCoinbase>> {
    let tip_height = json_rpc.tip_id().await?.height;
    let target_height = tip_height.value().saturating_add(1);
    let maturity_cutoff = target_height.saturating_sub(100);
    let mut utxos = fetch_address_utxos(env, test_address).await?;
    // Largest UTXO first; see `mempool_broadcast_cycle::select_spendable_test_coinbase`
    // for the rationale (height-first sort self-poisons after repeated runs leave
    // scratch dust at recent heights ahead of the big coinbase outputs).
    utxos.sort_by_key(|utxo| utxo.satoshis);
    utxos.reverse();

    for utxo in utxos {
        if utxo.height <= maturity_cutoff
            && utxo.satoshis > ZIP317_FEE_ONE_IN_ONE_OUT_ZATS
            && address_utxo_is_unspent(env, &utxo).await?
        {
            return Ok(Some(TestCoinbase {
                txid_be: display_txid_to_wire_bytes(&utxo.txid)?,
                vout: utxo.output_index,
                value_zats: utxo.satoshis,
                height: BlockHeight::new(utxo.height),
                target_height,
            }));
        }
    }
    Ok(None)
}

async fn fetch_address_utxos(
    env: &LiveTestEnv,
    test_address: &str,
) -> Result<Vec<ZebraAddressUtxo>> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"getaddressutxos","params":[{{"addresses":["{test_address}"]}}]}}"#
    );
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre!(
            "getaddressutxos curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| eyre!("getaddressutxos response is not JSON: {error}; body={body}"))?;
    if let Some(error_field) = parsed.get("error")
        && !error_field.is_null()
    {
        return Err(eyre!("getaddressutxos RPC returned error: {error_field}"));
    }
    let result_field = parsed
        .get("result")
        .ok_or_else(|| eyre!("getaddressutxos response missing result field; body={body}"))?;
    serde_json::from_value(result_field.clone())
        .map_err(|error| eyre!("getaddressutxos result shape is invalid: {error}; body={body}"))
}

async fn address_utxo_is_unspent(env: &LiveTestEnv, utxo: &ZebraAddressUtxo) -> Result<bool> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"gettxout","params":["{}",{}]}}"#,
        utxo.txid, utxo.output_index
    );
    let output = tokio::process::Command::new("curl")
        .arg("-s")
        .args(["-X", "POST"])
        .args(["-H", "content-type: application/json"])
        .arg("-d")
        .arg(&body)
        .arg(env.target.json_rpc_addr.as_str())
        .output()
        .await?;
    if !output.status.success() {
        return Err(eyre!(
            "gettxout curl exited with status {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let body = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| eyre!("gettxout response is not JSON: {error}; body={body}"))?;
    if let Some(error_field) = parsed.get("error")
        && !error_field.is_null()
    {
        return Err(eyre!("gettxout RPC returned error: {error_field}"));
    }
    Ok(parsed
        .get("result")
        .is_some_and(|rpc_result| !rpc_result.is_null()))
}

fn display_txid_to_wire_bytes(txid: &str) -> Result<[u8; 32]> {
    let mut bytes: [u8; 32] = hex::decode(txid)?.try_into().map_err(|decoded: Vec<u8>| {
        eyre!("txid decoded to {} bytes, expected 32", decoded.len())
    })?;
    bytes.reverse();
    Ok(bytes)
}

fn scratch_recipient_address(test_key: &TransparentTestKey, salt: u64) -> TransparentAddress {
    let funded_hash = match test_key.address() {
        TransparentAddress::PublicKeyHash(hash) | TransparentAddress::ScriptHash(hash) => *hash,
    };
    let salt_bytes = salt.to_le_bytes();
    let mut scratch = [0_u8; 20];
    for (index, byte) in funded_hash.iter().enumerate() {
        scratch[index] = byte ^ 0xFF ^ salt_bytes[index % salt_bytes.len()];
    }
    TransparentAddress::PublicKeyHash(scratch)
}

async fn backfill_from_coinbase(
    env: &LiveTestEnv,
    coinbase_height: BlockHeight,
) -> Result<(TempDir, PrimaryChainStore)> {
    let tip_height = fetch_live_tip_height(env).await?;
    let checkpoint_height = BlockHeight::new(coinbase_height.value().saturating_sub(1));
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
    backfill_config.node.request_timeout = std::cmp::max(
        backfill_config.node.request_timeout,
        Duration::from_secs(90),
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
