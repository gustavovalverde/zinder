#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::num::NonZeroU32;
use std::sync::Arc;

use eyre::{Result, eyre};
use prost::Message;
use tempfile::tempdir;
use tonic::Request;
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::BlockHeight;
use zinder_core::Network;
use zinder_ingest::run_bulk_catchup;
use zinder_proto::compat::lightwalletd::{
    self, CompactBlock as LightwalletdCompactBlock, compact_tx_streamer_server::CompactTxStreamer,
};
use zinder_query::WalletQuery;
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live_for};

use crate::common::{
    fetch_live_network_upgrade_activations, fetch_live_tip_height, live_bulk_catchup_run_config,
    zebra_source_from_bulk_catchup,
};

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn bulk_catchup_deep_chain_with_by_txid_lookups() -> Result<()> {
    let _guard = init();
    // Bulk catching up [1, tip] only fits in CI budgets on regtest. Hosted networks
    // need the checkpoint-bounded bulk catchup (BulkCatchupRunConfig::checkpoint) before
    // this test can run there.
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");

    let tip_height = fetch_live_tip_height(&env).await?;
    if tip_height.value() < 10 {
        return Err(eyre!(
            "deep-chain test needs at least 10 blocks; got tip {}; advance the chain (e.g. via the regtest `generate` RPC)",
            tip_height.value()
        ));
    }

    let activations = fetch_live_network_upgrade_activations(&env).await?;
    let bulk_catchup_config = live_bulk_catchup_run_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        tip_height,
        NonZeroU32::new(25).ok_or_else(|| eyre!("invalid test batch size"))?,
        true,
        Arc::clone(&activations),
    );
    let source = zebra_source_from_bulk_catchup(&bulk_catchup_config)?;
    let commit_outcome = run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected committed deep-chain bulk-catchup outcome"))?;
    let chain_epoch = commit_outcome.chain_epoch;
    assert_eq!(chain_epoch.network, env.network());
    assert_eq!(chain_epoch.visible_tip_height, tip_height);

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let lightwalletd_adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(store.clone(), (), Arc::clone(&activations)),
        Arc::clone(&activations),
    );
    let reader = store.current_chain_epoch_reader()?;

    for height_value in [1_u32, 5, tip_height.value() / 2, tip_height.value()] {
        let block = reader
            .compact_block_at(BlockHeight::new(height_value))?
            .ok_or_else(|| eyre!("missing compact block at height {height_value}"))?;
        let lightwalletd_block = LightwalletdCompactBlock::decode(block.payload_bytes.as_slice())?;
        let txid = lightwalletd_block
            .vtx
            .first()
            .ok_or_else(|| eyre!("compact block at height {height_value} has no transactions"))?
            .txid
            .clone();
        let response = lightwalletd_adapter
            .get_transaction(Request::new(lightwalletd::TxFilter {
                block: None,
                index: 0,
                hash: txid,
            }))
            .await?
            .into_inner();
        assert_eq!(response.height, u64::from(height_value));
        assert!(
            !response.data.is_empty(),
            "by-txid transaction at height {height_value} returned empty payload"
        );
    }

    for height_value in 1..=tip_height.value() {
        let block = reader
            .compact_block_at(BlockHeight::new(height_value))?
            .ok_or_else(|| eyre!("missing compact block at height {height_value}"))?;
        assert_eq!(block.height.value(), height_value);
        let lightwalletd_block = LightwalletdCompactBlock::decode(block.payload_bytes.as_slice())?;
        assert!(
            !lightwalletd_block.header.is_empty(),
            "compact block at height {height_value} must carry a serialized header"
        );
    }
    Ok(())
}
