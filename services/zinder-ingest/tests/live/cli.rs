#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::{fs, io};

use eyre::{Result, eyre};
use tempfile::tempdir;
use zinder_core::{BlockHeight, Network};
use zinder_store::{ChainStoreOptions, PrimaryChainStore};
use zinder_testkit::live::{init, require_live};

use crate::common::{
    BackfillConfigToml, assert_native_wallet_read_responses, backfill_config_toml,
    basic_auth_credentials, zinder_ingest_command,
};

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
