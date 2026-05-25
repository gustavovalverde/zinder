#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::sync::Arc;
use std::{fs, io, num::NonZeroU32};

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{BlockHeight, BlockHeightRange, Network};
use zinder_query::{WalletQuery, WalletQueryApi};
use zinder_source::NodeSource;
use zinder_store::{ChainStoreOptions, PrimaryChainStore, StoreError};
use zinder_testkit::live::{init, require_live, require_live_for};

use crate::common::{
    BoundedIngestConfigToml, WalletServingIngestConfigToml, assert_native_wallet_read_responses,
    basic_auth_credentials, bounded_ingest_config_toml, fetch_live_network_upgrade_activations,
    live_bulk_catchup_run_config, wallet_serving_ingest_config_toml,
    zebra_source_from_bulk_catchup, zinder_ingest_command,
};

const WALLET_SERVING_BOUNDED_DEPTH_BLOCKS: u32 = 150;

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn cli_runs_bounded_ingest_loop_from_config() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live()? else {
        return Ok(());
    };
    let (username, password) = basic_auth_credentials(&env)?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let target_height = match env.network() {
        Network::ZcashRegtest => BlockHeight::new(1),
        Network::ZcashTestnet | Network::ZcashMainnet => BlockHeight::new(2),
        other => return Err(eyre!("unsupported network for CLI test: {other:?}")),
    };
    fs::write(
        &config_path,
        bounded_ingest_config_toml(&BoundedIngestConfigToml {
            network_name: encode_zinder_native_chain_name(env.network()),
            json_rpc_addr: &env.target.json_rpc_addr,
            node_auth_username: username,
            node_auth_password: password,
            storage_path: &storage_path,
            target_height: target_height.value(),
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
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("event=\"chain_committed\""), "{stderr}");
    assert!(stderr.contains("chain_epoch_id=1"), "{stderr}");
    assert!(
        stderr.contains(&format!("tip_height={}", target_height.value())),
        "{stderr}"
    );

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let reader = store.current_chain_epoch_reader()?;
    let compact_block = reader
        .compact_block_at(target_height)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tip compact block artifact"))?;

    assert_eq!(compact_block.height, target_height);
    let activations = fetch_live_network_upgrade_activations(&env).await?;
    assert_native_wallet_read_responses(
        &store,
        env.network(),
        1,
        target_height.value(),
        activations,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "live CLI smoke keeps node-derived floor resolution, process execution, and wallet-read assertions in one auditable scenario"
)]
async fn cli_runs_bounded_wallet_serving_loop_from_config() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashTestnet, Network::ZcashMainnet])? else {
        return Ok(());
    };
    let (username, password) = basic_auth_credentials(&env)?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let activations = fetch_live_network_upgrade_activations(&env).await?;
    let probe_config = live_bulk_catchup_run_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        BlockHeight::new(1),
        NonZeroU32::new(1).ok_or_else(|| eyre!("invalid probe batch size"))?,
        true,
        Arc::clone(&activations),
    );
    let source = zebra_source_from_bulk_catchup(&probe_config)?;
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
        wallet_serving_ingest_config_toml(&WalletServingIngestConfigToml {
            network_name: encode_zinder_native_chain_name(env.network()),
            json_rpc_addr: &env.target.json_rpc_addr,
            node_auth_username: username,
            node_auth_password: password,
            storage_path: &storage_path,
            target_height: to_height.value(),
            request_timeout_secs: env.target.request_timeout.as_secs(),
        })?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| eyre!("config path not utf-8"))?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    let checkpoint_height = BlockHeight::new(wallet_serving_floor.value() - 1);
    assert!(
        stderr.contains("event=\"wallet_serving_modifiers_resolved\""),
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
            .tree_state_checkpoint_at_or_before(to_height)?
            .ok_or_else(|| eyre!("missing bounded wallet-serving tip tree state"))?;
        assert_eq!(first_compact_block.height, wallet_serving_floor);
        assert_eq!(tip_tree_state.height, to_height);
    }

    let wallet_query = WalletQuery::new(store, (), Arc::clone(&activations));
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
    let _tree_state = wallet_query
        .tree_state_checkpoint_at_or_before(to_height, None)
        .await?;
    Ok(())
}
