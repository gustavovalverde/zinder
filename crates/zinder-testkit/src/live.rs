//! Live-node test gating helpers.
//!
//! Live tests live under each crate's `tests/live/` submodule. They use the
//! same environment schema as production binaries so live-test setup and
//! service setup cannot drift.
//! They reuse the production `ZINDER_NETWORK` and `ZINDER_NODE__*` env-var
//! schema instead of inventing a parallel namespace, plus a single
//! `ZINDER_TEST_LIVE=1` opt-in gate.
//!
//! # Usage
//!
//! ```ignore
//! use eyre::Result;
//! use zinder_testkit::live::{init, require_live, LIVE_TEST_IGNORE_REASON};
//!
//! #[tokio::test]
//! #[ignore = LIVE_TEST_IGNORE_REASON]
//! async fn backfills_initial_range() -> Result<()> {
//!     let _guard = init();
//!     let env = require_live()?;
//!     // env.target.json_rpc_addr, env.target.node_auth, env.target.network ...
//!     Ok(())
//! }
//! ```
//!
//! `require_live()` rejects [`Network::ZcashMainnet`] by default. Tests that
//! target a specific network allowlist (mainnet, testnet, regtest, or a
//! subset) opt in via [`require_live_for`] and use the `let Some(env) = ...?
//! else { return Ok(()); }` pattern so the operator can run the full live
//! suite against any single network without those tests failing:
//!
//! ```ignore
//! let Some(env) = require_live_for(&[Network::ZcashMainnet])? else {
//!     return Ok(());
//! };
//! // or:
//! let Some(env) = require_live_mainnet()? else { return Ok(()); };
//! ```
//!
//! Tests that depend on optional external services (a reference
//! `lightwalletd-go` for parity, a Zallet sidecar) read their endpoints with
//! [`optional_env`], applying the same skip-when-absent pattern so a full
//! `cargo nextest run --profile=ci-live` always lights up exactly the subset
//! of tests the operator's environment can serve.

use std::sync::Once;

use eyre::{WrapErr, eyre};
use zinder_core::Network;
use zinder_source::NodeTarget;

/// Canonical `#[ignore]` reason used on every live test.
///
/// Rust attributes cannot reference this constant directly (`#[ignore = ...]`
/// requires a literal string). Each live test duplicates the literal text
/// `"live test; see CLAUDE.md §Live Node Tests"`; this constant exists
/// as the documentation anchor so contributors can grep for the canonical
/// reason. The longer operator-facing guidance lives in [`require_live`]'s
/// error path.
pub const LIVE_TEST_IGNORE_REASON: &str = "live test; see CLAUDE.md §Live Node Tests";

/// Resolved live-test inputs. Carries the [`NodeTarget`] plus the witness
/// that the live gate was checked.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LiveTestEnv {
    /// Resolved node endpoint and credentials.
    pub target: NodeTarget,
}

impl LiveTestEnv {
    /// Returns the resolved network for runtime test dispatch.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.target.network
    }
}

/// Gate any live test that targets a non-mainnet upstream node.
///
/// Returns `Ok(Some(env))` when `ZINDER_TEST_LIVE=1` is set and the resolved
/// network is regtest or testnet. Returns `Ok(None)` when the resolved
/// network is mainnet (the test skips silently because it does not opt into
/// mainnet semantics); tests that genuinely target mainnet should call
/// [`require_live_mainnet`] or [`require_live_for`] instead. Returns `Err`
/// only when the live gate or `ZINDER_NODE__*` env-var schema is misconfigured.
pub fn require_live() -> eyre::Result<Option<LiveTestEnv>> {
    let env = resolve_live_env()?;
    if matches!(env.network(), Network::ZcashMainnet) {
        Ok(None)
    } else {
        Ok(Some(env))
    }
}

/// Gate a live test to a specific network allowlist.
///
/// Returns `Ok(Some(env))` when the resolved network is in `allowed`. Returns
/// `Ok(None)` when `ZINDER_TEST_LIVE=1` is set but the resolved network is
/// not in `allowed`, so the test can early-return successfully (the operator
/// is running the full suite against a different network). Returns `Err` only
/// for genuine configuration problems (live gate not enabled, missing
/// required `ZINDER_NODE__*` vars).
pub fn require_live_for(allowed: &[Network]) -> eyre::Result<Option<LiveTestEnv>> {
    let env = resolve_live_env()?;
    if allowed.contains(&env.network()) {
        Ok(Some(env))
    } else {
        Ok(None)
    }
}

/// Convenience for tests that target mainnet only.
///
/// Equivalent to `require_live_for(&[Network::ZcashMainnet])` but reads more
/// directly at the call site. Returns `Ok(None)` when the operator's
/// environment targets a non-mainnet network so the test skips silently.
pub fn require_live_mainnet() -> eyre::Result<Option<LiveTestEnv>> {
    require_live_for(&[Network::ZcashMainnet])
}

/// Reads a required environment variable, returning a wrapped error when it is unset.
///
/// Use for live-test inputs the operator must supply explicitly when the test
/// cannot meaningfully proceed without them and the absence indicates
/// operator misconfiguration rather than an unprovisioned subsystem.
pub fn require_env(name: &str) -> eyre::Result<String> {
    std::env::var(name).wrap_err_with(|| format!("missing required env var {name}"))
}

/// Reads an optional environment variable.
///
/// Returns `Ok(None)` when the variable is unset so the caller can skip its
/// enclosing test silently; returns `Err` only when the value is present but
/// unreadable (non-UTF-8).
///
/// Use for endpoints supplied by optional external sidecars (a reference
/// `lightwalletd-go` for parity, a Zallet binary) so a full
/// `cargo nextest run --profile=ci-live` always lights up exactly the subset
/// of tests the operator's environment can serve.
pub fn optional_env(name: &str) -> eyre::Result<Option<String>> {
    match std::env::var(name) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(eyre!("environment variable {name} is not valid UTF-8"))
        }
    }
}

/// One-time test bootstrap.
///
/// Installs the `color-eyre` panic hook and a `tracing-subscriber` writer.
/// Safe to call from many tests in the same process because it uses [`Once`].
/// Returns a drop guard the test body must hold so call sites read uniformly
/// across the workspace.
#[must_use = "hold the returned guard for the duration of the test"]
pub fn init() -> impl Drop {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = color_eyre::install();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_test_writer()
            .try_init();
    });
    InitGuard
}

struct InitGuard;

impl Drop for InitGuard {
    fn drop(&mut self) {}
}

fn resolve_live_env() -> eyre::Result<LiveTestEnv> {
    if std::env::var("ZINDER_TEST_LIVE").as_deref() != Ok("1") {
        return Err(eyre!(
            "set ZINDER_TEST_LIVE=1 plus ZINDER_NETWORK and ZINDER_NODE__* env vars to run live tests"
        ));
    }
    let target = NodeTarget::from_environment()
        .map_err(|error| eyre!("{error}"))
        .wrap_err("resolving node target from environment")?;
    Ok(LiveTestEnv { target })
}
