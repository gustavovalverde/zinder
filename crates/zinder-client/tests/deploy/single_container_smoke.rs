//! Builds the `deploy/single-container` Zinder image and asserts the
//! integrated `zinder-ingest` + `zinder-query` stack serves the public
//! surface end-to-end against the operator's regtest Zebra sidecar.
//!
//! The test sequence captures the operator self-hosting contract:
//! `docker run` the recommended image, point it at a real Zebra, and have
//! a typed Rust client read the wallet-query surface through it without
//! any out-of-band glue.
//!
//! The test is intentionally tolerant of slow builds (the Rust workspace
//! compiles in release mode inside the builder stage) and is skipped
//! silently when Docker is unreachable so a contributor without Docker can
//! still run the rest of the workspace.

#![allow(
    missing_docs,
    reason = "Deploy-test names describe the behavior under test."
)]
#![allow(
    clippy::print_stderr,
    reason = "Deploy smoke surfaces container logs to the test runner on failure for triage."
)]

use std::fs;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use eyre::{Result, eyre};
use tokio::process::Command;
use zinder_client::{ChainIndex, Network, RemoteChainIndex, RemoteOpenOptions};
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_testkit::deploy::require_docker;
use zinder_testkit::live::{init, require_live_for};

const IMAGE_TAG: &str = "zinder-test:single-container-smoke";
const READYZ_DEADLINE: Duration = Duration::from_mins(2);
const READYZ_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "deploy test; see CLAUDE.md §Live Node Tests"]
async fn single_container_image_serves_walletquery_end_to_end() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
        return Ok(());
    };
    let Some(_docker) = require_docker().await? else {
        eprintln!(
            "skipping single-container smoke: docker unreachable. Configure the daemon \
             and rerun `cargo nextest run --profile=ci-deploy --run-ignored=all`."
        );
        return Ok(());
    };

    let workspace_root = workspace_root()?;
    build_image(&workspace_root).await?;

    let query_port = reserve_free_tcp_port()?;
    let ops_port = reserve_free_tcp_port()?;
    // Use a tempdir *inside the workspace* so Docker Desktop on macOS bind-
    // mounts it without the operator having to add `/var/folders/...` to its
    // file-sharing whitelist. `.tmp/` is gitignored workspace-wide; live and
    // perf tests stage their fixtures here too.
    let workspace_tmp = workspace_root.join(".tmp");
    fs::create_dir_all(&workspace_tmp)?;
    let temp_root = tempfile::Builder::new()
        .prefix("zinder-smoke-")
        .tempdir_in(&workspace_tmp)?;
    let store_dir = temp_root.path().join("store");
    let secondary_dir = temp_root.path().join("secondary");
    let config_dir = temp_root.path().join("config");
    fs::create_dir_all(&store_dir)?;
    fs::create_dir_all(&secondary_dir)?;
    fs::create_dir_all(&config_dir)?;
    let ingest_config_path = config_dir.join("ingest.toml");
    let query_config_path = config_dir.join("query.toml");
    let json_rpc_url = rewrite_to_host_gateway(&env.target.json_rpc_addr)?;
    fs::write(
        &ingest_config_path,
        smoke_ingest_toml(env.network(), &json_rpc_url),
    )?;
    fs::write(
        &query_config_path,
        smoke_query_toml(env.network(), &json_rpc_url),
    )?;

    let container_name = format!("zinder-smoke-{}", std::process::id());
    let container_started = start_container(
        &container_name,
        &env,
        query_port,
        ops_port,
        &ingest_config_path,
        &query_config_path,
    )
    .await;
    let smoke_outcome = match container_started {
        Ok(()) => exercise_running_stack(query_port, ops_port).await,
        Err(error) => Err(error),
    };

    let cleanup_result = stop_container(&container_name).await;
    smoke_outcome?;
    cleanup_result?;
    Ok(())
}

async fn build_image(workspace_root: &Path) -> Result<()> {
    eprintln!("building single-container image {IMAGE_TAG} (first run can be slow)");
    let build_status = Command::new("docker")
        .args([
            "build",
            "-t",
            IMAGE_TAG,
            "-f",
            "deploy/single-container/Dockerfile",
            ".",
        ])
        .current_dir(workspace_root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|error| eyre!("docker build failed to start: {error}"))?;
    if !build_status.success() {
        return Err(eyre!(
            "docker build exited with status {:?}",
            build_status.code(),
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "Smoke run wires every host-side knob (ports + bind-mount paths + auth) into one docker invocation; collapsing into a struct adds boilerplate without clarifying the call site."
)]
async fn start_container(
    container_name: &str,
    env: &zinder_testkit::live::LiveTestEnv,
    query_port: u16,
    ops_port: u16,
    ingest_config_path: &Path,
    query_config_path: &Path,
) -> Result<()> {
    let _previous = Command::new("docker")
        .args(["rm", "-f", container_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    let mut run_args = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--name".to_owned(),
        container_name.to_owned(),
        "--add-host".to_owned(),
        "host.docker.internal:host-gateway".to_owned(),
        "-p".to_owned(),
        format!("{query_port}:9101"),
        "-p".to_owned(),
        format!("{ops_port}:9106"),
        "-v".to_owned(),
        format!(
            "{}:/etc/zinder/ingest.toml:ro",
            ingest_config_path.display(),
        ),
        "-v".to_owned(),
        format!("{}:/etc/zinder/query.toml:ro", query_config_path.display(),),
    ];
    for env_pair in container_env_vars(env)? {
        run_args.push("-e".to_owned());
        run_args.push(env_pair);
    }
    run_args.push(IMAGE_TAG.to_owned());

    let status = Command::new("docker")
        .args(&run_args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|error| eyre!("docker run failed to start: {error}"))?;
    if !status.success() {
        return Err(eyre!("docker run exited with status {:?}", status.code(),));
    }
    Ok(())
}

async fn exercise_running_stack(query_port: u16, ops_port: u16) -> Result<()> {
    wait_for_ready(ops_port).await?;

    let endpoint = format!("http://127.0.0.1:{query_port}");
    let chain_index = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint: endpoint.clone(),
        network: Network::ZcashRegtest,
    })
    .map_err(|error| eyre!("configuring WalletQuery client for {endpoint}: {error}"))?;
    let wallet_info = chain_index
        .server_info()
        .await
        .map_err(|error| eyre!("WalletQuery.ServerInfo failed: {error}"))?;
    let common = wallet_info
        .common
        .as_ref()
        .ok_or_else(|| eyre!("WalletQuery.ServerInfo missing common ops.ServerInfo"))?;
    assert_eq!(
        common.network,
        encode_zinder_native_chain_name(Network::ZcashRegtest),
        "server_info network must reflect the regtest sidecar",
    );
    assert!(
        !common.capabilities.is_empty(),
        "server_info must advertise at least one capability string",
    );
    Ok(())
}

async fn wait_for_ready(ops_port: u16) -> Result<()> {
    let started = std::time::Instant::now();
    let readyz_url = format!("http://127.0.0.1:{ops_port}/readyz");
    while started.elapsed() < READYZ_DEADLINE {
        let outcome = Command::new("curl")
            .args(["-fsS", "-o", "/dev/null", &readyz_url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if let Ok(status) = outcome
            && status.success()
        {
            return Ok(());
        }
        tokio::time::sleep(READYZ_POLL_INTERVAL).await;
    }
    Err(eyre!(
        "single-container /readyz at {readyz_url} did not return 200 within {:?}",
        READYZ_DEADLINE,
    ))
}

async fn stop_container(container_name: &str) -> Result<()> {
    let _ = Command::new("docker")
        .args(["logs", "--tail", "200", container_name])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await;
    let _stop_status = Command::new("docker")
        .args(["stop", "-t", "5", container_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let _rm_status = Command::new("docker")
        .args(["rm", "-f", container_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    Ok(())
}

fn container_env_vars(env: &zinder_testkit::live::LiveTestEnv) -> Result<Vec<String>> {
    let rewritten_json_rpc = rewrite_to_host_gateway(&env.target.json_rpc_addr)?;
    let mut pairs = vec![format!("ZINDER_NODE__JSON_RPC_ADDR={rewritten_json_rpc}")];
    match &env.target.node_auth {
        zinder_source::NodeAuth::Basic { username, password } => {
            use secrecy::ExposeSecret;
            pairs.push("ZINDER_NODE__AUTH__METHOD=basic".to_owned());
            pairs.push(format!("ZINDER_NODE__AUTH__USERNAME={username}"));
            pairs.push(format!(
                "ZINDER_NODE__AUTH__PASSWORD={}",
                password.expose_secret(),
            ));
        }
        zinder_source::NodeAuth::Cookie(_) | zinder_source::NodeAuth::None => {
            return Err(eyre!(
                "deploy smoke currently exercises basic auth only; set \
                 ZINDER_NODE__AUTH__METHOD=basic for the smoke run.",
            ));
        }
    }
    Ok(pairs)
}

fn rewrite_to_host_gateway(addr: &str) -> Result<String> {
    if let Some(after_scheme) = addr.strip_prefix("http://") {
        let trimmed = after_scheme
            .trim_start_matches("127.0.0.1")
            .trim_start_matches("localhost");
        return Ok(format!("http://host.docker.internal{trimmed}"));
    }
    Err(eyre!(
        "deploy smoke expects an http:// JSON-RPC URL; got {addr:?}"
    ))
}

fn smoke_ingest_toml(network: Network, json_rpc_url: &str) -> String {
    let chain_name = encode_zinder_native_chain_name(network);
    format!(
        r#"# Smoke-test ingest config bind-mounted at /etc/zinder/ingest.toml.

[network]
name = "{chain_name}"

[node]
json_rpc_addr = "{json_rpc_url}"
request_timeout_secs = 15
max_response_bytes = 67108864

[node.auth]
method = "basic"
username = "REPLACE_VIA_ENV"
password = "REPLACE_VIA_ENV"

[storage]
path = "/var/lib/zinder/store"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest.bulk_catchup]
canonical_batch_max_blocks = 100
canonical_batch_max_artifact_bytes = 536870912
source_segment_max_blocks = 16
source_segment_target_response_bytes = 33554432
source_fetch_max_in_flight_requests = 12
source_fetch_max_in_flight_bytes = 402653184
fact_build_concurrency = 16

[ingest_control]
listen_addr = "127.0.0.1:9100"

[ingest.tip_follow]
poll_interval_ms = 2000
"#
    )
}

fn smoke_query_toml(network: Network, json_rpc_url: &str) -> String {
    let chain_name = encode_zinder_native_chain_name(network);
    format!(
        r#"# Smoke-test query config bind-mounted at /etc/zinder/query.toml.

[network]
name = "{chain_name}"

[node]
json_rpc_addr = "{json_rpc_url}"
request_timeout_secs = 15

[node.auth]
method = "basic"
username = "REPLACE_VIA_ENV"
password = "REPLACE_VIA_ENV"

[storage]
path = "/var/lib/zinder/store"
secondary_path = "/var/lib/zinder/secondary"

[ingest_control]
addr = "http://127.0.0.1:9100"

[query]
listen_addr = "0.0.0.0:9101"
"#
    )
}

fn reserve_free_tcp_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
        .map_err(|error| eyre!("binding ephemeral port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| eyre!("reading ephemeral listener addr: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_directory
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| eyre!("CARGO_MANIFEST_DIR has no grandparent: {manifest_directory:?}"))
}
