//! Live-node test gating helpers.
//!
//! Live tests live under each crate's `tests/live/` submodule per [ADR-0012].
//! They reuse the production `ZINDER_NETWORK` and `ZINDER_NODE__*` env-var
//! schema instead of inventing a parallel namespace, plus a single
//! `ZINDER_TEST_LIVE=1` opt-in gate.
//!
//! [ADR-0012]: ../../../../docs/adrs/0012-test-tiers-and-live-config.md
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
//! genuinely target mainnet must opt in:
//!
//! ```ignore
//! let env = require_live_for(&[Network::ZcashMainnet])?;
//! // or:
//! let env = require_live_mainnet()?;
//! ```

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

/// Gate any test that touches a real upstream node. Verifies `ZINDER_TEST_LIVE=1`,
/// resolves the source endpoint from the unified env-var schema, and
/// rejects [`Network::ZcashMainnet`] by default.
///
/// Tests that target a subset of networks should call [`require_live_for`].
/// Tests that genuinely target mainnet should call [`require_live_mainnet`].
pub fn require_live() -> eyre::Result<LiveTestEnv> {
    let env = resolve_live_env()?;
    if matches!(env.network(), Network::ZcashMainnet) {
        return Err(eyre!(
            "this live test does not target mainnet by default; \
             use require_live_for(&[Network::ZcashMainnet]) or require_live_mainnet() \
             to opt in explicitly"
        ));
    }
    Ok(env)
}

/// Gate a live test to a specific network allowlist. Returns an error if the
/// resolved network is not in `allowed`.
pub fn require_live_for(allowed: &[Network]) -> eyre::Result<LiveTestEnv> {
    let env = resolve_live_env()?;
    if allowed.contains(&env.network()) {
        Ok(env)
    } else {
        Err(eyre!(
            "live test allowed only on {allowed:?}; ZINDER_NETWORK resolved to {:?}",
            env.network()
        ))
    }
}

/// Convenience for tests that genuinely target mainnet. Equivalent to
/// `require_live_for(&[Network::ZcashMainnet])` but reads more directly at the
/// call site.
pub fn require_live_mainnet() -> eyre::Result<LiveTestEnv> {
    require_live_for(&[Network::ZcashMainnet])
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
