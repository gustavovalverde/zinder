use std::{
    error::Error,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::json;
use tempfile::tempdir;
use zinder_testkit::{JsonRpcTestServer, RpcReply, method};

use crate::common::zinder_ingest_command;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_micros(50);

#[test]
fn disabled_ingest_control_publishes_no_ops_capabilities() -> Result<(), Box<dyn Error>> {
    let node = admitted_node()?;
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("canonical");
    let ops_addr = unused_loopback_addr()?;
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        ingest_config_toml(&storage_path, &node.url(), ops_addr, None)?,
    )?;

    let mut command = zinder_ingest_command();
    command
        .args(["--config", path_str(&config_path)?])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut process = ChildProcess::spawn(&mut command)?;

    let healthz = wait_for_healthz(&mut process, ops_addr)?;
    assert_eq!(healthz["status"], "alive");
    assert_eq!(healthz["service"], "zinder-ingest");
    assert_eq!(healthz["network"], "zcash-regtest");
    assert_eq!(healthz["capabilities"], json!([]));

    let requested_methods = node
        .requests()?
        .into_iter()
        .map(|request| request.method)
        .collect::<Vec<_>>();
    assert!(requested_methods.iter().any(|name| name == "rpc.discover"));
    assert!(
        requested_methods
            .iter()
            .any(|name| name == "getblockchaininfo")
    );

    Ok(())
}

#[test]
fn occupied_ingest_control_prevents_ops_publication() -> Result<(), Box<dyn Error>> {
    let node = admitted_node()?;
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("canonical");
    let occupied_control = TcpListener::bind("127.0.0.1:0")?;
    let control_addr = occupied_control.local_addr()?;
    let ops_addr = unused_loopback_addr()?;
    let config_path = tempdir.path().join("zinder-ingest.toml");
    fs::write(
        &config_path,
        ingest_config_toml(&storage_path, &node.url(), ops_addr, Some(control_addr))?,
    )?;

    let mut command = zinder_ingest_command();
    command
        .args(["--config", path_str(&config_path)?])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut process = ChildProcess::spawn(&mut command)?;
    let status = wait_for_exit_without_ops_publication(&mut process, ops_addr)?;
    assert!(
        !status.success(),
        "occupied control listener must fail startup"
    );
    assert!(TcpStream::connect(ops_addr).is_err());

    let stderr = process.read_stderr()?;
    assert!(
        stderr.contains(&format!(
            "failed to bind IngestControl listener at {control_addr}"
        )),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

fn admitted_node() -> Result<JsonRpcTestServer, Box<dyn Error>> {
    Ok(JsonRpcTestServer::start([
        method("rpc.discover").reply(RpcReply::result(json!({
            "openrpc": "1.3.2",
            "info": {"title": "Zebra", "version": "test"},
            "methods": [
                {"name": "getblock"},
                {"name": "getbestblockheightandhash"},
                {"name": "z_gettreestate"},
                {"name": "z_getsubtreesbyindex"},
                {"name": "getblockchaininfo"},
                {"name": "rpc.discover"}
            ]
        }))),
        method("getblockchaininfo").reply(RpcReply::result(json!({"upgrades": {}}))),
        method("getbestblockheightandhash").reply(RpcReply::result(json!({
            "height": 0,
            "hash": vec![0_u8; 32]
        }))),
    ])?)
}

fn ingest_config_toml(
    storage_path: &Path,
    node_url: &str,
    ops_addr: SocketAddr,
    ingest_control_addr: Option<SocketAddr>,
) -> Result<String, Box<dyn Error>> {
    let ingest_control_addr = ingest_control_addr.map_or_else(String::new, |addr| addr.to_string());
    Ok(format!(
        r#"[network]
name = "zcash-regtest"

[node]
json_rpc_addr = "{node_url}"
request_timeout_secs = 5

[node.auth]
method = "none"

[storage]
path = "{}"

[ingest]
source = "zebra-json-rpc"
reorg_window_blocks = 100

[ingest_control]
listen_addr = "{ingest_control_addr}"

[ops]
listen_addr = "{ops_addr}"
"#,
        path_str(storage_path)?
    ))
}

fn unused_loopback_addr() -> Result<SocketAddr, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

fn wait_for_healthz(
    process: &mut ChildProcess,
    ops_addr: SocketAddr,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = process.try_wait()? {
            let stderr = process.read_stderr()?;
            return Err(format!(
                "zinder-ingest exited before /healthz was published: {status}: {stderr}"
            )
            .into());
        }
        if let Ok(healthz) = fetch_healthz(ops_addr) {
            return Ok(healthz);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for zinder-ingest /healthz".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_exit_without_ops_publication(
    process: &mut ChildProcess,
    ops_addr: SocketAddr,
) -> Result<std::process::ExitStatus, Box<dyn Error>> {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        assert!(
            TcpStream::connect(ops_addr).is_err(),
            "ops listener published before IngestControl admission completed"
        );
        if let Some(status) = process.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for occupied IngestControl startup failure".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn fetch_healthz(address: SocketAddr) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(
        format!("GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let body_offset = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| offset + 4)
        .ok_or("operations response omitted HTTP body delimiter")?;
    Ok(serde_json::from_slice(&response[body_offset..])?)
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}

struct ChildProcess {
    child: Child,
}

impl ChildProcess {
    fn spawn(command: &mut std::process::Command) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            child: command.spawn()?,
        })
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, Box<dyn Error>> {
        Ok(self.child.try_wait()?)
    }

    fn read_stderr(&mut self) -> Result<String, Box<dyn Error>> {
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .ok_or("zinder-ingest stderr was not captured")?
            .read_to_string(&mut stderr)?;
        Ok(stderr)
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
