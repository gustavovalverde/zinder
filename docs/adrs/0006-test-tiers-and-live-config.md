# ADR-0006: Test Tiers and Unified Live-Test Config

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Test infrastructure, validation tier mapping, live-test configuration schema |
| Related | [Service operations §Validation Tiers](../architecture/service-operations.md#validation-tiers), [Public interfaces](../architecture/public-interfaces.md) |

## Context

Zinder has tests with very different runtime mechanisms: pure-logic unit tests, fixture-driven integration tests, time-budgeted perf tests, and tests that touch a real upstream node. Each mechanism has a different tolerance for flake, a different runtime budget, and a different cadence to belong in CI. Encoding the tier in the file name (`z3_regtest_*`, `z3_testnet_*`) or in a function-name prefix produces filenames that lie when a contributor reorganizes tests, and makes "which network am I running" a parallel parameter to "which tier am I running."

Live tests also need to talk to an upstream node. Inventing a parallel test-only env-var namespace splits the configuration schema in two: production binaries read `ZINDER_NODE__*`, live tests read something else, and credential helpers get duplicated across crates.

## Decision

### One vocabulary: four tiers, one parameter

Tiers describe the **runtime mechanism** a test depends on, not which environment it runs in. A live test is a live test whether the network is regtest, testnet, or mainnet; the network is a parameter, not a tier.

| Tier | Mechanism | Module path |
| ---- | --------- | ----------- |
| T0 unit | in-process pure logic | `#[cfg(test)] mod tests` in `src/` |
| T1 integration | fixture-driven, no external state | `tests/integration/` |
| T2 perf | time-budgeted, no external state | `tests/perf/` |
| T3 live | real upstream node | `tests/live/` |

`ZINDER_NETWORK` is the only knob that distinguishes regtest, testnet, and mainnet runs of T3. Cadence (PR, nightly, weekly, release) is a CI matrix axis on T3, not a separate tier.

### Unified config schema

Live tests and production binaries read the same env-var schema.

| Variable | Purpose | Production | Tests |
| --- | --- | --- | --- |
| `ZINDER_NETWORK` | Which chain Zinder operates as | required | required (T3 dispatch) |
| `ZINDER_NODE__JSON_RPC_ADDR` | Upstream node endpoint | required | required (T3) |
| `ZINDER_NODE__AUTH__METHOD` | `basic` or `cookie` | required | required (T3) |
| `ZINDER_NODE__AUTH__USERNAME` | Node username (Basic) | conditional | conditional |
| `ZINDER_NODE__AUTH__PASSWORD` | Node password (Basic) | conditional | conditional |
| `ZINDER_NODE__REQUEST_TIMEOUT_SECS` | Per-RPC timeout | optional (default 30) | optional |
| `ZINDER_TEST_LIVE` | Acknowledge T3 invocation | unused | required `=1` |

Production reads of `std::env::vars()` skip every key starting with `ZINDER_TEST_`; the strip happens in `zinder_runtime::zinder_environment_source`. There is no parallel test-only namespace mirroring node config.

`ZINDER_TEST_LIVE` is the only test-side variable that overlaps the schema. Its job is to defend against accidental T3 invocation when a developer's shell carries production config from prior debugging. The defense: `--ignored` (or `--run-ignored=all`) **and** `ZINDER_TEST_LIVE=1` must both be set. Either alone is insufficient.

The `ZINDER_STORE_CRASH_*` crash-recovery harness uses the `ZINDER_TEST_` namespace.

### Shared `NodeTarget` type

A shared resolved type lives in `zinder-source`:

```rust
// crates/zinder-source/src/node_target.rs

#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct NodeTarget {
    pub network: Network,
    pub json_rpc_addr: String,
    pub node_auth: NodeAuth,
    pub request_timeout: Duration,
    pub max_response_bytes: NonZeroU64,
}

impl NodeTarget {
    pub fn from_environment() -> Result<Self, NodeConfigError>;
}
```

Each binary's `resolve_*_config` maps its raw deserialized schema into a `NodeTarget` plus its own binary-specific extras. Tests reuse `NodeTarget::from_environment` directly. The config-rs loader, the `--print-config` redaction, and the env-var schema are one definition.

`zinder-source` is the natural home: it owns `NodeAuth`, `ZebraJsonRpcSource`, the `node` capability vocabulary, and `SourceError`.

### `zinder-testkit::live` surface

```rust
// crates/zinder-testkit/src/live.rs

pub const LIVE_TEST_IGNORE_REASON: &str =
    "live test; see CLAUDE.md §Live Node Tests";

#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LiveTestEnv {
    pub target: NodeTarget,
}

impl LiveTestEnv {
    pub fn network(&self) -> Network;
}

/// Gate any test that touches a real upstream node. Rejects mainnet by default.
pub fn require_live() -> eyre::Result<LiveTestEnv>;

/// Gate a live test to a specific network allowlist.
pub fn require_live_for(allowed: &[Network]) -> eyre::Result<LiveTestEnv>;

/// Convenience for tests that genuinely target mainnet.
pub fn require_live_mainnet() -> eyre::Result<LiveTestEnv>;

/// One-time test bootstrap: installs `color-eyre` panic hook and `tracing-subscriber`.
pub fn init() -> impl Drop;
```

`LiveTestEnv` is a pure resolved-config struct plus a check-witness. It does not provide factory methods like `zebra_source()`. Tests that need a `ZebraJsonRpcSource` construct it from the public `NodeTarget` fields.

`require_live()` rejects `Network::ZcashMainnet` even when `ZINDER_NETWORK=Mainnet` is set. Mainnet is opt-in by import, not just by env.

`init()` uses `std::sync::Once` so installation is safe under both `cargo test` and `cargo nextest run`.

### Test gating

Every T3 test carries both gates:

1. `#[ignore = LIVE_TEST_IGNORE_REASON]` — the libtest fallback.
2. A first-line `zinder_testkit::live::require_live()` call that returns a `LiveTestEnv` after verifying `ZINDER_TEST_LIVE=1` and resolving the node config.

```rust
use eyre::{Result, WrapErr};
use zinder_testkit::live::{init, require_live, LIVE_TEST_IGNORE_REASON};

#[tokio::test]
#[ignore = LIVE_TEST_IGNORE_REASON]
async fn backfills_initial_range() -> Result<()> {
    let _guard = init();
    let env = require_live()?;
    let source = ZebraJsonRpcSource::with_options(
        env.network(),
        &env.target.json_rpc_addr,
        env.target.node_auth.clone(),
        ZebraJsonRpcSourceOptions {
            request_timeout: env.target.request_timeout,
            max_response_bytes: env.target.max_response_bytes,
        },
    )?;
    Ok(())
}
```

Function names are plain `snake_case_describing_behavior`. Network is a runtime fact discovered through `env.network()`; do not include `live`, `regtest`, `testnet`, `mainnet`, or `z3` in function names.

### Module-path tier organization

Every crate with a `tests/` directory uses one binary per crate via `tests/acceptance.rs` plus tier submodules:

```text
<crate>/tests/
  acceptance.rs          # mod common; mod integration; mod live; mod perf;
  common/
    mod.rs               # shared helpers
  integration/
    mod.rs
    <sut>.rs             # fixture-driven, no external state
  live/                  # only in crates that talk to an upstream node
    mod.rs
    <sut>.rs             # network-agnostic; reads env.network() at runtime
  perf/                  # only in crates with perf budgets
    mod.rs
    <sut>.rs             # time-budgeted, no external state
```

Tier selection is by module path through nextest filter expressions. Adding a file under `live/` automatically enrolls it in T3.

In-crate `#[cfg(test)] mod tests` blocks remain the home for T0 unit tests.

### Tier-to-mechanism mapping

| Tier | Module path | Profile | Filter | CI cadence |
| --- | --- | --- | --- | --- |
| T0 unit | `#[cfg(test)] mod tests` in `src/` | `default` / `ci` | (default-filter) | every PR |
| T1 integration | `tests/integration/` | `default` / `ci` | (default-filter) | every PR |
| T2 perf | `tests/perf/` | `ci-perf` | `test(/^perf::/)` | every PR (separate job) |
| T3 live | `tests/live/` | `ci-live` | `test(/^live::/)` | nightly (regtest), weekly (testnet), `workflow_dispatch` (mainnet) |

### Test runner: nextest

`cargo nextest run` is the canonical workspace test runner. `cargo test` continues to work as a libtest fallback (and is what `cargo mutants` shells), but is not the validation gate.

Four nextest profiles in `.config/nextest.toml`:

- **`default`**: local dev. Excludes `live::` and `perf::` via `default-filter`. `fail-fast = false`.
- **`ci`**: PR CI. Inherits from `default`. Adds JUnit output and one retry.
- **`ci-perf`**: separate PR job. Filters to `test(/^perf::/)`. Raised slow-timeout for budget assertions.
- **`ci-live`**: nightly/weekly/release. Filters to `test(/^live::/)`. Two retries for network flake. `node-mutating` test group with `max-threads = 1` for state-mutating live tests.

The `default-filter` mechanism is the structural boundary between tiers. `#[ignore]` is the libtest-fallback safety net for `cargo test` users without nextest. Both are required.

### `node-mutating` test group

Live tests that mutate upstream-node state (multi-step backfill that observes chain growth, mempool flow tests that depend on ordering) belong in a dedicated test group:

```toml
[test-groups]
node-mutating = { max-threads = 1 }
```

Read-only live tests (capability probes, tip fetches, broadcast-rejection classification) run in parallel under nextest's default group.

A test joins the group with a per-test override:

```toml
[[profile.ci-live.overrides]]
filter = "test(live::deep_chain) or test(live::tip_follow)"
test-group = "node-mutating"
```

### Test helper error type

Test code in `zinder-testkit::live` and tier submodules uses `eyre::Result<T>` with `eyre::WrapErr::wrap_err` for context chains. This is a deliberate exception to the production crate convention of `thiserror`-typed errors per boundary. Test code is not a public API; the goal is diagnostic clarity at failure time.

The workspace-level lint floor (`unwrap`, `expect`, `dbg`, `print` denied in tests per `clippy.toml`) is unchanged. `eyre` does not relax it.

### Open mainnet infrastructure questions

Until each is answered, T3 mainnet is `workflow_dispatch`-only on the same GitHub-hosted runner shape as T3 regtest/testnet, and tests guard their own scope through `require_live_for(&[Network::ZcashMainnet])` plus checkpoint-bounded ranges:

1. **Mainnet node hosting.** ZFND-operated dedicated Zebra node, or third-party node with credentials shared via GitHub Actions secrets.
2. **Mainnet runner topology.** GitHub-hosted runners cap at 6 hours. A genesis backfill on mainnet exceeds that.
3. **T3 mainnet trigger model.** Release-tag only, scheduled monthly, both, or `workflow_dispatch`-only.
4. **Cost ownership.** Persistent runner plus multi-TB disk has a recurring monthly cost.
5. **On-call destination.** Slack channel, email list, or auto-opened GitHub issue.
6. **T3 mainnet scope.** Mainnet starts with capability probe, single read-RPC roundtrip, checkpoint-bounded backfill.

## Consequences

Positive:

- One config schema across production binaries and live tests.
- Tier is a structural boundary, not a name match. The directory listing is the tier inventory.
- The same test body runs against any network. One `backfills_initial_range` reads `env.network()` at runtime; CI runs it three times by varying `ZINDER_NETWORK` per matrix cell.
- One opt-in gate (`ZINDER_TEST_LIVE=1` plus `--ignored`) is sufficient to run any T3 cell.
- Mainnet is opt-in by import. A test that did not say `require_live_for(&[Network::ZcashMainnet])` cannot run on mainnet.
- One shared `NodeTarget` type across binaries and tests.
- Per-test slow-timeout overrides in `nextest.toml`. Test bodies stop carrying timeout constants for runtime budget.
- The `node-mutating` test group serializes only state-mutating tests; read-only live tests run in parallel.
- The `#[ignore]` defense is preserved across runners.

Negative:

- `zinder-testkit` has a path dep on `zinder-runtime` so `LiveTestEnv` can reuse `zinder_environment_source`. The dependency is one-way.
- `cargo test --test <name>` invocations someone may have memorized are gone; per-file binaries become one-binary-per-crate. Nextest filter expressions are a strict superset.
- `eyre` is a dev-dep across the workspace.
- The mainnet workflow is `workflow_dispatch`-only until the six open questions resolve.

Tradeoffs:

- Reusing production env vars in tests instead of inventing a test-only namespace. The defense against accidental T3 invocation is `ZINDER_TEST_LIVE=1` plus `#[ignore]` plus mainnet-rejected-by-default.
- Module-path tiers instead of file-prefix separate binaries. Submodules compile to one binary per crate (faster link, lower test-startup overhead, easier shared `tests/common/`).
- Runtime parameterization by network instead of compile-time macro expansion.
- `eyre::Result` for tests instead of `Box<dyn Error>` or `thiserror`. Wins on diagnostic ergonomics.
- Mainnet rejected by default in `require_live()`. Tests targeting mainnet opt in by name.
