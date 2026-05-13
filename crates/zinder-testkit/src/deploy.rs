//! Deploy-tier test gating helpers.
//!
//! Deploy tests live under each crate's `tests/deploy/` submodule and
//! exercise the real `deploy/single-container` Docker image against a
//! running regtest sidecar. They are double-gated by
//! [`DEPLOY_TEST_IGNORE_REASON`] plus a runtime [`require_docker`] probe
//! so developer machines without Docker silently skip the suite instead
//! of failing the deploy-profile run.
//!
//! # Usage
//!
//! ```ignore
//! use eyre::Result;
//! use zinder_testkit::deploy::{DEPLOY_TEST_IGNORE_REASON, require_docker};
//! use zinder_testkit::live::{init, require_live_for};
//! use zinder_core::Network;
//!
//! #[tokio::test(flavor = "multi_thread")]
//! #[ignore = DEPLOY_TEST_IGNORE_REASON]
//! async fn single_container_image_serves_walletquery_server_info() -> Result<()> {
//!     let _guard = init();
//!     let Some(env) = require_live_for(&[Network::ZcashRegtest])? else {
//!         return Ok(());
//!     };
//!     let Some(docker) = require_docker().await? else {
//!         return Ok(());
//!     };
//!     // build / run / assert against the image
//!     Ok(())
//! }
//! ```

use std::process::Stdio;

use eyre::{Result, eyre};
use tokio::process::Command;

/// Canonical `#[ignore]` reason used on every deploy test. Mirrors the live
/// tests' `LIVE_TEST_IGNORE_REASON` pattern so both gates feel identical at
/// the call site.
pub const DEPLOY_TEST_IGNORE_REASON: &str = "deploy test; see CLAUDE.md §Live Node Tests";

/// Witness that the local Docker daemon is reachable.
///
/// Created by [`require_docker`]; carrying it through the test body proves
/// the daemon probe ran instead of relying on shell `docker` invocations to
/// fail mid-test.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DockerProbe {
    /// `true` when `docker info` returned successfully during the probe.
    pub daemon_reachable: bool,
}

/// Probes for a reachable Docker daemon.
///
/// Returns `Ok(Some(_))` when `docker info` succeeds within a short
/// deadline, `Ok(None)` when the binary is missing or the daemon is not
/// reachable (deploy tests skip silently in that case), and `Err` only for
/// genuine I/O errors that prevent the probe from running.
pub async fn require_docker() -> Result<Option<DockerProbe>> {
    let outcome = Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    match outcome {
        Ok(output) if output.status.success() => Ok(Some(DockerProbe {
            daemon_reachable: true,
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(eyre!("invoking docker info failed: {error}")),
    }
}
