#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{fs, net::SocketAddr, path::Path, process::Command, time::Duration};

use serde_json::json;
use tempfile::tempdir;
use tokio::{process::Command as TokioCommand, time::sleep};
use tonic::transport::Endpoint;
use zinder_core::{ChainEpochId, Network, wire::encode_height_key_ascending};
use zinder_derive::{
    DeriveStore, DeriveStoreOptions, ProjectionPreset,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
};
use zinder_proto::{
    capabilities::{WALLET_ADDRESS_TRANSPARENT_HISTORY_V1, WALLET_READ_TRANSPARENT_SPENDS_V1},
    v1::wallet::{ServerInfoRequest, wallet_query_client::WalletQueryClient},
};
use zinder_store::{ChainStoreOptions, PrimaryChainStore, RocksDbResourceBudget};
use zinder_testkit::{ChainFixture, JsonRpcTestServer, RpcReply, method};

#[test]
fn print_config_renders_resolved_toml_to_stdout() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-print-config-store");
    let secondary_path = tempdir.path().join("query-print-config-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("[network]"), "{stdout}");
    assert!(stdout.contains("name = \"zcash-regtest\""), "{stdout}");
    assert!(stdout.contains("[query]"), "{stdout}");
    assert!(
        stdout.contains("listen_addr = \"127.0.0.1:9101\""),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "secondary_path = \"{}\"",
            path_str(&secondary_path)?
        )),
        "{stdout}"
    );
    assert!(stdout.contains("[ingest_control]"), "{stdout}");
    assert!(
        stdout.contains("addr = \"http://127.0.0.1:9100\""),
        "{stdout}"
    );
    assert!(
        stdout.contains("mempool_mined_retention_minutes = 60"),
        "{stdout}"
    );
    assert!(
        stdout.contains("mempool_invalidated_retention_hours = 24"),
        "{stdout}"
    );
    assert!(!stderr.contains("ERROR"), "{stderr}");

    Ok(())
}

#[test]
fn public_listen_addr_without_opt_in_is_refused() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-public-bind-store");
    let secondary_path = tempdir.path().join("query-public-bind-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_with_public_listen_addr_toml(&storage_path, &secondary_path, false)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("query.listen_addr"), "{stderr}");
    assert!(stderr.contains("security.allow_public_bind"), "{stderr}");

    Ok(())
}

#[test]
fn public_listen_addr_with_opt_in_validates_and_warns() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-public-bind-optin-store");
    let secondary_path = tempdir.path().join("query-public-bind-optin-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_with_public_listen_addr_toml(&storage_path, &secondary_path, true)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stdout.contains("listen_addr = \"0.0.0.0:9101\""),
        "{stdout}"
    );
    assert!(stderr.contains("query.listen_addr"), "{stderr}");

    Ok(())
}

#[test]
fn storage_path_default_resolves_to_canonical_zinder_layout() -> eyre::Result<()> {
    // The binary's default for `storage.path` matches the canonical Zinder
    // layout under `/var/lib/zinder/store`. The default exists so the
    // single-container Docker image works with env-only configuration and
    // no `--config` argument. Operators on other deployment shapes override
    // via `ZINDER_STORAGE__PATH` or the `--storage-path` flag.
    let output = zinder_query_command()
        .args(["--print-config", "--network", "zcash-regtest"])
        .output()?;

    assert!(
        output.status.success(),
        "print-config failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("path = \"/var/lib/zinder/store\""),
        "stdout does not carry the canonical storage.path default:\n{stdout}"
    );

    Ok(())
}

#[test]
fn secondary_path_default_resolves_to_canonical_zinder_layout() -> eyre::Result<()> {
    // Same rationale as `storage_path_default_resolves_to_canonical_zinder_layout`:
    // the wallet-query reader opens its RocksDB secondary at
    // `/var/lib/zinder/secondary` by default. Operators on shared-store
    // deployments override via `ZINDER_STORAGE__SECONDARY_PATH` or the
    // `--secondary-path` flag.
    let output = zinder_query_command()
        .args(["--print-config", "--network", "zcash-regtest"])
        .output()?;

    assert!(
        output.status.success(),
        "print-config failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("secondary_path = \"/var/lib/zinder/secondary\""),
        "stdout does not carry the canonical storage.secondary_path default:\n{stdout}"
    );

    Ok(())
}

#[test]
fn ingest_only_section_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-node-source-store");
    let secondary_path = tempdir.path().join("query-node-source-secondary");
    let config_path = tempdir.path().join("zinder-query.toml");
    fs::write(
        &config_path,
        query_config_with_ingest_section_toml(&storage_path, &secondary_path)?,
    )?;

    let output = zinder_query_command()
        .args(["--print-config", "--config", path_str(&config_path)?])
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("unknown field `ingest`"), "{stderr}");

    Ok(())
}

#[tokio::test]
async fn production_reader_discovers_and_reports_the_wallet_workload() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("query-wallet-store");
    let secondary_path = tempdir.path().join("query-wallet-secondary");
    seed_wallet_workload_store(&storage_path)?;
    let node = wallet_reader_node()?;
    let listen_addr = unused_loopback_addr()?;
    let config_path = tempdir.path().join("zinder-query-wallet.toml");
    fs::write(
        &config_path,
        query_runtime_config_toml(&storage_path, &secondary_path, listen_addr, &node.url())?,
    )?;
    let mut child = zinder_query_tokio_command();
    child
        .args(["--config", path_str(&config_path)?])
        .kill_on_drop(true);
    let mut child = child.spawn()?;

    let endpoint = Endpoint::from_shared(format!("http://{listen_addr}"))?;
    let mut client = None;
    for _attempt in 0..100 {
        match endpoint.connect().await {
            Ok(channel) => {
                client = Some(WalletQueryClient::new(channel));
                break;
            }
            Err(_error) => sleep(Duration::from_millis(25)).await,
        }
    }
    let mut client = client.ok_or_else(|| eyre::eyre!("query reader did not start"))?;
    let info = client
        .server_info(ServerInfoRequest {})
        .await?
        .into_inner()
        .info
        .and_then(|info| info.common)
        .ok_or_else(|| eyre::eyre!("query reader omitted common server information"))?;

    assert_eq!(info.projection_preset, "wallet");
    assert_eq!(
        info.projection_identities,
        vec![
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME
                .as_str()
                .to_owned(),
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME.as_str().to_owned(),
        ]
    );
    assert!(
        info.capabilities
            .iter()
            .any(|capability| capability == WALLET_ADDRESS_TRANSPARENT_HISTORY_V1)
    );
    assert!(
        info.capabilities
            .iter()
            .any(|capability| capability == WALLET_READ_TRANSPARENT_SPENDS_V1)
    );

    child.kill().await?;
    Ok(())
}

fn seed_wallet_workload_store(storage_path: &Path) -> eyre::Result<()> {
    let canonical_store = PrimaryChainStore::open(
        storage_path,
        ChainStoreOptions::for_network(Network::ZcashRegtest),
    )?;
    let artifacts = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre::eyre!("wallet workload fixture must contain one block"))?;
    let committed = canonical_store.commit_chain_epoch(artifacts)?;
    let derive_store = DeriveStore::open_with_projection_preset(
        DeriveStore::path_for_canonical(storage_path),
        ProjectionPreset::Wallet,
        DeriveStoreOptions {
            rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            ..DeriveStoreOptions::default()
        },
    )?;
    let tip_key = encode_height_key_ascending(committed.chain_epoch.visible_tip_height);
    for (projection, index_column_family) in [
        (
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
            TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
        ),
        (
            TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
            TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
        ),
    ] {
        derive_store
            .put_chain_event_cursor(projection, committed.event_envelope.cursor.as_bytes())?;
        derive_store.put_consumer(index_column_family, &tip_key, &[])?;
    }
    drop(derive_store);
    drop(canonical_store);
    Ok(())
}

fn wallet_reader_node() -> eyre::Result<JsonRpcTestServer> {
    JsonRpcTestServer::start([
        method("rpc.discover").reply(RpcReply::result(json!({
            "openrpc": "1.3.2",
            "info": {"title": "Zebra", "version": "test"},
            "methods": [
                {"name": "getblock"},
                {"name": "getbestblockhash"},
                {"name": "getblockheader"},
                {"name": "z_gettreestate"},
                {"name": "z_getsubtreesbyindex"},
                {"name": "sendrawtransaction"},
                {"name": "getblockchaininfo"},
                {"name": "rpc.discover"}
            ]
        }))),
        method("getblockchaininfo").reply(RpcReply::result(json!({
            "upgrades": {
                "5ba81b19": {"name": "Overwinter", "activationheight": 1, "status": "active"},
                "76b809bb": {"name": "Sapling", "activationheight": 1, "status": "active"},
                "2bb40e60": {"name": "Blossom", "activationheight": 1, "status": "active"},
                "f5b9230b": {"name": "Heartwood", "activationheight": 1, "status": "active"},
                "e9ff75a6": {"name": "Canopy", "activationheight": 1, "status": "active"},
                "c2d6d0b4": {"name": "NU5", "activationheight": 2, "status": "active"},
                "c8e71055": {"name": "NU6", "activationheight": 2, "status": "active"},
                "5437f330": {"name": "NU6.2", "activationheight": 3, "status": "active"}
            }
        }))),
    ])
}

fn query_config_toml(storage_path: &Path, secondary_path: &Path) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[query]
listen_addr = "127.0.0.1:9101"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn query_config_with_public_listen_addr_toml(
    storage_path: &Path,
    secondary_path: &Path,
    allow_public_bind: bool,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[query]
listen_addr = "0.0.0.0:9101"

[security]
allow_public_bind = {allow_public_bind}
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn query_config_with_ingest_section_toml(
    storage_path: &Path,
    secondary_path: &Path,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[query]
listen_addr = "127.0.0.1:9101"

[node]
json_rpc_addr = "http://127.0.0.1:18232"

[ingest]
source = "zebra-json-rpc"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn query_runtime_config_toml(
    storage_path: &Path,
    secondary_path: &Path,
    listen_addr: SocketAddr,
    node_url: &str,
) -> eyre::Result<String> {
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[storage]
path = "{}"
secondary_path = "{}"

[node]
json_rpc_addr = "{node_url}"

[node.auth]
method = "none"

[query]
listen_addr = "{listen_addr}"

[ops]
listen_addr = ""

[ingest_control]
addr = "http://127.0.0.1:1"
"#,
        path_str(storage_path)?,
        path_str(secondary_path)?,
    ))
}

fn unused_loopback_addr() -> eyre::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

fn zinder_query_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zinder-query"));
    command.env_clear();
    command
}

fn zinder_query_tokio_command() -> TokioCommand {
    let mut command = TokioCommand::new(env!("CARGO_BIN_EXE_zinder-query"));
    command.env_clear();
    command
}

fn path_str(path: &Path) -> eyre::Result<&str> {
    path.to_str()
        .ok_or_else(|| eyre::eyre!("path is not valid UTF-8: {}", path.display()))
}
