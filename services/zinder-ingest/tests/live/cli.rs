#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::sync::Arc;
use std::{fs, io, num::NonZeroU32};

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{BlockHeight, BlockHeightRange, Network};
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_source::NodeSource;
use zinder_store::{ChainStoreOptions, PrimaryChainStore, StoreError};
use zinder_testkit::live::{init, require_live, require_live_for};
use zinder_testkit::sample_regtest_upgrade_activations;

use crate::common::{
    BackfillConfigToml, WalletServingBackfillConfigToml, assert_native_wallet_read_responses,
    backfill_config_toml, basic_auth_credentials, live_backfill_config,
    wallet_serving_backfill_config_toml, zebra_source_from_backfill, zinder_ingest_command,
};

const WALLET_SERVING_BOUNDED_DEPTH_BLOCKS: u32 = 150;

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn cli_backfills_initial_range_from_config() -> Result<()> {
    let _guard = init();
    let env = require_live()?;
    let (username, password) = basic_auth_credentials(&env)?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let to_height = match env.network() {
        Network::ZcashRegtest => BlockHeight::new(1),
        Network::ZcashTestnet | Network::ZcashMainnet => BlockHeight::new(2),
        other => return Err(eyre!("unsupported network for CLI test: {other:?}")),
    };
    fs::write(
        &config_path,
        backfill_config_toml(&BackfillConfigToml {
            network_name: env.network().name(),
            json_rpc_addr: &env.target.json_rpc_addr,
            node_auth_username: username,
            node_auth_password: password,
            storage_path: &storage_path,
            from_height: 1,
            to_height: to_height.value(),
            request_timeout_secs: env.target.request_timeout.as_secs(),
            allow_near_tip_finalize: true,
        })?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| eyre!("config path not utf-8"))?,
            "backfill",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("event=\"chain_committed\""), "{stderr}");
    assert!(stderr.contains("chain_epoch_id=1"), "{stderr}");
    assert!(
        stderr.contains(&format!("tip_height={}", to_height.value())),
        "{stderr}"
    );

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let reader = store.current_chain_epoch_reader()?;
    let compact_block = reader
        .compact_block_at(to_height)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tip compact block artifact"))?;

    assert_eq!(compact_block.height, to_height);
    assert_native_wallet_read_responses(&store, env.network(), 1, to_height.value()).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "live CLI smoke keeps node-derived floor resolution, process execution, and wallet-read assertions in one auditable scenario"
)]
async fn cli_backfills_bounded_wallet_serving_floor_from_config() -> Result<()> {
    let _guard = init();
    let env = require_live_for(&[Network::ZcashTestnet, Network::ZcashMainnet])?;
    let (username, password) = basic_auth_credentials(&env)?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let probe_config = live_backfill_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        BlockHeight::new(1),
        NonZeroU32::new(1).ok_or_else(|| eyre!("invalid probe batch size"))?,
        true,
    );
    let source = zebra_source_from_backfill(&probe_config)?;
    let activations = source.fetch_network_upgrade_activations().await?;
    let wallet_serving_floor = activations
        .earliest_wallet_servable_activation()
        .ok_or_else(|| eyre!("node did not advertise Sapling or NU5 activation heights"))?
        .activation_height;
    if wallet_serving_floor == BlockHeight::new(0) {
        return Err(eyre!("wallet-serving floor cannot be genesis"));
    }
    let to_height = BlockHeight::new(
        wallet_serving_floor
            .value()
            .checked_add(WALLET_SERVING_BOUNDED_DEPTH_BLOCKS - 1)
            .ok_or_else(|| eyre!("wallet-serving bounded height overflowed"))?,
    );
    let node_tip = NodeSource::tip_id(&source).await?.height;
    if node_tip.value() <= to_height.value() {
        return Err(eyre!(
            "wallet-serving bounded test needs tip above {}; got {}",
            to_height.value(),
            node_tip.value()
        ));
    }

    fs::write(
        &config_path,
        wallet_serving_backfill_config_toml(&WalletServingBackfillConfigToml {
            network_name: env.network().name(),
            json_rpc_addr: &env.target.json_rpc_addr,
            node_auth_username: username,
            node_auth_password: password,
            storage_path: &storage_path,
            to_height: to_height.value(),
            request_timeout_secs: env.target.request_timeout.as_secs(),
        })?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| eyre!("config path not utf-8"))?,
            "backfill",
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    let checkpoint_height = BlockHeight::new(wallet_serving_floor.value() - 1);
    assert!(
        stderr.contains("event=\"wallet_serving_backfill_floor_resolved\""),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("from_height={}", wallet_serving_floor.value())),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("checkpoint_height={}", checkpoint_height.value())),
        "{stderr}"
    );
    assert!(stderr.contains("event=\"chain_committed\""), "{stderr}");
    assert!(
        stderr.contains(&format!("tip_height={}", to_height.value())),
        "{stderr}"
    );

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    {
        let reader = store.current_chain_epoch_reader()?;
        assert!(matches!(
            reader.compact_block_at(checkpoint_height),
            Err(StoreError::ArtifactMissing { .. })
        ));
        let first_compact_block = reader
            .compact_block_at(wallet_serving_floor)?
            .ok_or_else(|| eyre!("missing first wallet-serving compact block"))?;
        let tip_tree_state = reader
            .tree_state_at(to_height)?
            .ok_or_else(|| eyre!("missing bounded wallet-serving tip tree state"))?;
        assert_eq!(first_compact_block.height, wallet_serving_floor);
        assert_eq!(tip_tree_state.height, to_height);
    }

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let latest_block = wallet_query.latest_block(None).await?;
    assert_eq!(latest_block.height, to_height);
    let compact_blocks = wallet_query
        .compact_block_range(
            BlockHeightRange::inclusive(wallet_serving_floor, to_height),
            None,
        )
        .await?;
    assert_eq!(
        compact_blocks.compact_blocks.len(),
        usize::try_from(WALLET_SERVING_BOUNDED_DEPTH_BLOCKS)?
    );
    let _tree_state = wallet_query.tree_state_at(to_height, None).await?;
    Ok(())
}
