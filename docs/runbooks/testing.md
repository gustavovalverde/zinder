# Testing Runbook

Operational procedures for validating Zinder against its workspace, real Zebra
nodes, and external wallet applications. The structural rules behind the test
tiers live in [ADR-0006](../adrs/0006-test-tiers-and-live-config.md) and
[ADR-0007](../adrs/0007-multi-process-storage-access.md); the consumer release
gate lives in [ADR-0012](../adrs/0012-consumer-release-certification.md). This
document is the step-by-step guide for actually running them.

## Test tier matrix

| Tier | Location | Profile | Trigger | Catches |
| ---- | -------- | ------- | ------- | ------- |
| T0 unit | `#[cfg(test)] mod tests in src/` | `default-filter` of `default`/`ci` | Every commit | Logic regressions in the unit under test |
| T1 integration | `tests/integration/` | `default-filter` of `default`/`ci` | Every commit | Cross-module wiring, gRPC adapter shape, store/proto round-trips |
| T2 perf | `tests/perf/` | `ci-perf` | Every commit | Latency budget regressions per the published budgets |
| Release parity | `crates/zinder-client/tests/parity/` | `ci-parity` | Release / tag pipeline | Consumer-shaped request and error-shape regressions for Zashi/Zodl, Zallet, public lightwalletd operators, and explorers |
| T3 live | `tests/live/` | `ci-live` | Manual / scheduled CI | Real upstream-node behavior (Zebra JSON-RPC, indexer gRPC) |
| T3 Zallet live | `crates/zinder-client/tests/live/zallet.rs` | `ci-zallet-live` | Release / integration certification | Real Zallet binary using Zinder's native contract |
| External | n/a | n/a | Manual | Exploratory wallet runs (Zodl/Android SDK, public lightwalletd clients) |

`default-filter = "not test(/^live::/) and not test(/^perf::/) and not test(/^parity::/)"`
is the structural boundary. The regular `ci-live` profile excludes
`live::zallet::`; run `ci-zallet-live` explicitly when a Zallet build that
targets Zinder is available. Every live test additionally carries
`#[ignore = LIVE_TEST_IGNORE_REASON]` and a runtime
`zinder_testkit::live::require_live()` check, so a stray `cargo test` cannot
talk to a node by accident.

`ci-parity` is fixture-backed and does not require `ZINDER_TEST_LIVE`. Treat it
as a release-certification gate for consumer-shaped contracts, not as a
replacement for live SDK, Zallet, or network validation.

## Default validation gate (T0 + T1 + T2 + format/lint/docs)

Run before considering any change complete. This is the canonical gate; CI
mirrors it.

```bash
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
cargo nextest run --profile=ci-perf
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo deny check
cargo machete
git diff --check
```

Expected outcome: every command exits zero. The full suite runs in ~5 minutes
on a developer laptop. If `cargo machete` reports unused dependencies on a
crate you did not touch, treat it as a pre-existing finding, not a blocker.

## Release parity gate (consumer-shaped fixtures)

Run before tagging a release, and after any change that touches a consumer-facing
wire contract or compatibility adapter:

```bash
cargo nextest run --profile=ci-parity
```

Expected outcome: every `parity::` module exits zero and the report is archived
with the release evidence. This profile is organized by consumer:

- `parity/zashi.rs` covers the lightwalletd-compatible shapes Zashi/Zodl and the
  Android SDK hit today.
- `parity/zallet.rs` covers the Zinder-native `WalletQuery` shape consumed by
  Zallet.
- `parity/lightwalletd_operators.rs` covers public lightwalletd operator
  expectations.
- `parity/explorers.rs` covers explorer-facing transparent-address read shapes.

Do not use `ci-parity` as proof that a real consumer SDK or binary works. It
proves the fixture-backed request and error shapes that must stay stable before
the slower external runs start.

## T3: Live regtest sweep

The most valuable automated check that exercises a real Zebra node.

### Prerequisites

1. **z3 regtest sidecar** running. The default ZF z3 setup gives you:
   - `z3_regtest_sidecar_zebra` (Zebra) listening JSON-RPC on host
     `127.0.0.1:39232` and indexer gRPC on `127.0.0.1:39155`.
   - Health endpoint at `127.0.0.1:38080`.
   - Mining target seed-derived address `tmDpFafuBHKGUYmuwLsrxWJrwcnSyzEEtYx`
     (configured via `ZEBRA_MINING__MINER_ADDRESS`).
2. **Container env contains** `ZEBRA_RPC__INDEXER_LISTEN_ADDR=0.0.0.0:8155`
   (mapped to host `39155`). Without it the indexer-gRPC tests skip.
3. **Auth method** matches what z3 publishes. The default sidecar uses basic
   auth `zebra:zebra` (`ZEBRA_RPC__ENABLE_COOKIE_AUTH=false`).

### Pre-flight checks

```bash
# Zebra JSON-RPC reachable
curl -s -u zebra:zebra -H "Content-Type: application/json" \
  -d '{"jsonrpc":"1.0","id":"1","method":"getblockchaininfo","params":[]}' \
  http://127.0.0.1:39232 | jq '.result | {chain, blocks, bestblockhash}'

# Indexer gRPC reachable (and feature compiled in)
grpcurl -plaintext 127.0.0.1:39155 list
# Expect: zebra.indexer.rpc.Indexer
```

If either probe fails, fix the sidecar before running tests. Don't use a
"--insecure" or "--ignore-failures" flag to paper over a missing service.

### Cleanup between runs

A persisted store from a previous run that bumped `STORE_SCHEMA_VERSION`
fails to reopen with `SchemaMismatch`. Clear the scratch directories before
reusing them across schema-changing commits:

```bash
rm -rf .tmp/regtest.zinder-store .tmp/regtest.compat-secondary .tmp/regtest.query-secondary
```

The `.tmp/` directory is `.gitignore`'d for exactly this purpose.

### Run

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__INDEXER_GRPC_ADDR=http://127.0.0.1:39155 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  cargo nextest run --profile=ci-live --run-ignored=all
```

Expected outcome on a healthy regtest with the indexer gRPC port set: all
regtest-targeted tests pass in ~30 s. A full regtest `ci-live` invocation also
contains hosted-network checks that deliberately refuse to run on regtest:
the mainnet-only tests
(`fetch_chain_checkpoint_returns_advancing_tree_sizes_on_mainnet`,
`tip_id_advances_above_one_million`,
`backfills_last_1000_blocks_from_checkpoint`) and the testnet-or-mainnet tests
(`cli_backfills_bounded_wallet_serving_floor_from_config`,
`checkpoint_bounded_read_endpoint_latency_baseline`).

Without `ZINDER_NODE__INDEXER_GRPC_ADDR`, three additional tests skip:
`zebra_indexer_mempool_*` and
`mempool_orchestrator_runs_against_real_zebra_indexer_with_in_memory_state`.

### Targeted reruns

```bash
# Single test
cargo nextest run --profile=ci-live -E 'test(read_endpoint_latency_baseline)' --run-ignored=all

# Whole suite of one file
cargo nextest run --profile=ci-live -E 'test(/^live::mempool_broadcast_cycle::/)' --run-ignored=all

# Re-run a specific failed test in isolation, with logs visible
cargo nextest run --profile=ci-live -E 'test(<failed_test_name>)' \
  --run-ignored=all --no-capture
```

Nextest does not have a built-in `--rerun-failures` flag; the `ci-live`
profile already retries each test up to twice on transient failure. For
deterministic reruns, copy the failed test name from the summary and pass it
through `-E 'test(<name>)'` as above.

The `node-mutating` test group in `.config/nextest.toml` serializes any test
that mutates regtest state (broadcast cycles, indexer-restart, deep-chain
backfills). Run them in isolation if you want to debug — parallel execution
will fight for the same node.

## T3: Live testnet (Zebra cookie auth)

Same surface as regtest but against real testnet. Useful for capability probes
and tip-fetch validation against a node with non-trivial chain weight.

```bash
cookie=$(docker exec <zebra_container> cat /var/run/auth/.cookie)
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-testnet \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:18232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=${cookie%%:*} \
  ZINDER_NODE__AUTH__PASSWORD=${cookie#*:} \
  cargo nextest run --profile=ci-live --run-ignored=all
```

`require_live()` accepts testnet by default; nothing further to opt in. Tests
that target only mainnet (see below) still skip.

## T3: Live mainnet (operator-hosted Zebra)

Local mainnet runs are supported against an operator-hosted Zebra. The
[ADR-0006 §Open mainnet infrastructure questions](../adrs/0006-test-tiers-and-live-config.md#open-mainnet-infrastructure-questions)
that remain open scope the **CI** matrix shape (cadence, hosting, cost), not
whether mainnet T3 is runnable. Locally:

1. Mainnet tests opt in via `require_live_for(&[Network::ZcashMainnet])` or
   `require_live_mainnet()`. They refuse to run on any other network.
2. `ZINDER_NETWORK=zcash-mainnet` plus the standard `ZINDER_NODE__*` schema
   pointed at a synced mainnet Zebra.
3. Same `cargo nextest` invocation as above:

```bash
cookie=$(docker exec <zebra_mainnet_container> cat /var/run/auth/.cookie)
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-mainnet \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:8232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=${cookie%%:*} \
  ZINDER_NODE__AUTH__PASSWORD=${cookie#*:} \
  cargo nextest run --profile=ci-live --run-ignored=all
```

Currently-mainnet-only tests are:
`fetch_chain_checkpoint_returns_advancing_tree_sizes_on_mainnet`,
`tip_id_advances_above_one_million`,
`backfills_last_1000_blocks_from_checkpoint`, plus the federated balance
read-only confirmations under `services/zinder-derive/tests/live/`.

## Performance calibration (T2 + live latency)

Two layers, both useful.

### T2 perf budgets

Synthetic, fast, deterministic; runs every commit:

```bash
cargo nextest run --profile=ci-perf
```

The `services/zinder-query/tests/perf/perf_smoke.rs` file holds the budgets;
each test asserts an upper bound (`PERF_SMOKE_LATEST_BUDGET = 250 ms`,
`PERF_SMOKE_RANGE_BUDGET = 1.5 s` for 1000 blocks). A budget violation is a
test failure.

### Live latency baseline

Real Zebra, real RocksDB; reports microsecond budgets per call:

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  cargo nextest run --profile=ci-live -E 'test(read_endpoint_latency_baseline)' \
    --run-ignored=all --no-capture
```

Look for the `live_latency_baseline` line in the output:

```
live_latency_baseline  network=zcash-regtest  tip=2361
  latest_block          = 160 µs
  compact_block_at      = 168 µs
  compact_block_range_50 = 1175 µs
  tree_state_at         = 249 µs
```

These numbers are not asserted by the test; treat the printed values as
operator-facing telemetry. Compare across changes to spot regressions in
the read path. The baseline does not exercise `transaction()` — that path
goes through `mempool_broadcast_cycle` (broadcast then look up the mined
txid) and `deep_chain` (high-volume backfill).

### Heavy probes (trust-sensitive code only)

Run on demand for changes to storage commit paths, parser code, or anything
under `chain_store::validation`:

```bash
cargo llvm-cov --workspace --all-features --no-report
cargo mutants --workspace --all-features \
  --file crates/zinder-store/src/chain_store.rs \
  --file crates/zinder-store/src/chain_store/validation.rs \
  --file crates/zinder-source/src/source_block.rs \
  --re 'chain_event_history|finalized_only_commit_without_artifacts|validate_reorg_window_change|from_raw_block_bytes'
```

These are slow (mutants often >30 min). Scheduled CI runs them weekly; locally,
gate on whether you actually changed trust-sensitive code.

## T3: Real Zallet binary gate

Zallet compatibility is a native-client claim, not a lightwalletd-compat claim.
The current public Zallet sidecar (`v0.1.0-alpha.3`) uses its embedded Zaino
indexer and points `[indexer].validator_address` at Zebra JSON-RPC. Do not count
that path as Zinder compatibility: it proves Zallet + Zebra/Zaino, not Zallet +
Zinder.

The automated Zallet gate lives in
`crates/zinder-client/tests/live/zallet.rs` and runs under the
`ci-zallet-live` profile. It is intentionally separate from the regular
`ci-live` sweep because it needs a Zallet build and config that target Zinder's
native contract, for example a Zallet branch wired to `RemoteChainIndex` over
`zinder-query`.

The gate fails closed when enabled:

- `ZINDER_TEST_ZALLET=1` must be set.
- `ZINDER_TEST_ZALLET_CONFIG` must point at the exact config used by the Zallet
  process.
- `ZINDER_TEST_ZALLET_CONFIG_MUST_CONTAIN` must be an active, uncommented line
  fragment proving that the config targets Zinder's native contract.
- Active `validator_address`, `validator_cookie_path`, `validator_user`, or
  `validator_password` entries are rejected because they are the embedded-Zaino
  validator path.
- `ZINDER_TEST_ZALLET_ARGS` is the real `zallet` command to execute, split on
  whitespace.
- `ZINDER_TEST_ZALLET_OUTPUT_MUST_CONTAIN` must appear in stdout or stderr from
  that command.

Example shape once a Zinder-native Zallet build exists:

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__INDEXER_GRPC_ADDR=http://127.0.0.1:39155 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  ZINDER_TEST_ZALLET=1 \
  ZINDER_TEST_ZALLET_BIN=/path/to/zallet \
  ZINDER_TEST_ZALLET_CONFIG=/path/to/zallet.toml \
  ZINDER_TEST_ZALLET_CONFIG_MUST_CONTAIN='zinder_query_addr = "http://127.0.0.1:9101"' \
  ZINDER_TEST_ZALLET_ARGS='--datadir /tmp/zallet --config /path/to/zallet.toml rpc getblockchaininfo' \
  ZINDER_TEST_ZALLET_OUTPUT_MUST_CONTAIN='regtest' \
  cargo nextest run --profile=ci-zallet-live --run-ignored=all
```

This profile does not start `zinder-query` or Zallet for you. The test is the
certification check over a running integration setup, so the failure output
points at the wrong config, missing binary, failed command, or unexpected
consumer output directly.

## External integration: lightwalletd-compatible wallets

### Conventions for the recipes below

- All three Zinder binaries (`zinder-ingest`, `zinder-query`,
  `zinder-compat-lightwalletd`) accept their config through a TOML file
  (`--config <PATH>`), the `ZINDER_*` env-var schema, and CLI flags, in
  that precedence order. **Sensitive fields (passwords, tokens) must come
  from the TOML file, not from env vars.** The runtime rejects
  `ZINDER_NODE__AUTH__PASSWORD` (and similar) with `environment variable
  targets sensitive field`; the live nextest runs only succeed because
  `ZINDER_TEST_LIVE=1` swaps in the test config source. Recipes for the
  binaries below therefore write a config TOML; recipes for the test
  runner keep using env vars.
- Pass `--ops-listen-addr 127.0.0.1:<port>` to expose `/healthz`,
  `/readyz`, and Prometheus `/metrics` on each runtime. Useful when
  debugging stuck live tests or watching ingest catch-up; the metrics
  endpoint is the same one CI scrapes.
- When the writer is configured with a shared-secret bearer token (per
  [ADR-0009 §Optional bearer token](../adrs/0009-ingest-control-transport-security.md#optional-shared-secret-bearer-token-loaded-from-a-file)),
  every reader needs `--ingest-control-token-path <PATH>` pointing at the
  same plain-text file. The file holds the token verbatim; the runtime
  loads it into `secrecy::SecretString` so it never appears in logs or
  `--print-config`. Without it, readers fall through to plaintext h2c
  (the localhost default).
- Each binary has its own config schema; `zinder-ingest` accepts
  `node.source = "zebra-json-rpc"` while `zinder-query` and
  `zinder-compat-lightwalletd` reject it as an unknown field. Write per-process
  configs in `.tmp/`:

  ```bash
  mkdir -p .tmp

  # zinder-ingest (writer)
  cat >.tmp/regtest.ingest.toml <<'EOF'
  [network]
  name = "zcash-regtest"

  [node]
  source = "zebra-json-rpc"
  json_rpc_addr = "http://127.0.0.1:39232"

  [node.auth]
  method = "basic"
  username = "zebra"
  password = "zebra"

  [storage]
  path = ".tmp/regtest.zinder-store"
  EOF

  # zinder-query and zinder-compat-lightwalletd share this reader config.
  # Same node + storage block, no `node.source`.
  cat >.tmp/regtest.reader.toml <<'EOF'
  [network]
  name = "zcash-regtest"

  [node]
  json_rpc_addr = "http://127.0.0.1:39232"

  [node.auth]
  method = "basic"
  username = "zebra"
  password = "zebra"

  [storage]
  path = ".tmp/regtest.zinder-store"
  EOF
  ```

  Per-process options (`--listen-addr`, `--secondary-path`,
  `--ops-listen-addr`) are passed as CLI flags on top of the configs.

### One-shot deterministic test

Already covered by `services/zinder-compat-lightwalletd/tests/integration/wallet_sdk_scan.rs`
in the default validation gate. It:

1. Stands up `LightwalletdGrpcAdapter` over a populated `PrimaryChainStore`.
2. Connects through the generated `CompactTxStreamerClient` (the same
   transport `librustzcash` consumers use).
3. Calls `GetBlockRange`, decodes every compact block, asserts `vtx`/`txid`
   alignment with stored transaction artifacts.
4. Asserts no key material appears in any client→server payload.

This test is the deterministic version of the wallet-SDK contract; running it
under `cargo nextest run --profile=ci` is part of every commit.

### End-to-end with a real lightwalletd-compatible client

Three terminals.

```bash
# Terminal 1: zinder-ingest tip-follow against z3 regtest
cargo run --release --bin zinder-ingest -- \
  --config .tmp/regtest.ingest.toml \
  tip-follow
```

```bash
# Terminal 2: lightwalletd compat shim over the same store
cargo run --release --bin zinder-compat-lightwalletd -- \
  --config .tmp/regtest.reader.toml \
  --secondary-path .tmp/regtest.compat-secondary \
  --listen-addr 127.0.0.1:9067
```

```bash
# Terminal 3: a lightwalletd-compatible wallet or SDK pointing at the compat shim
# Configure the endpoint to http://127.0.0.1:9067.
<wallet-or-sdk-command>
```

What this catches that the deterministic test does not:

- `GetTransaction` NotFound mapping (the client sees plain `NOT_FOUND`, not the
  legacy `ArtifactUnavailable`-with-resource_info detail).
- `GetBlock { height=0, hash }` flow through the new `BlockSelector`
  resolver.
- `GetTaddressTxids` / `GetTaddressTransactions` via the transparent-history
  index.
- Real-world streaming and connection-reuse patterns the in-process test cannot
  reproduce.

## External integration: Zashi / Android SDK

Same compat-shim path. The Android SDK speaks lightwalletd; point it
at the running `zinder-compat-lightwalletd:9067`.

What to validate by hand:

- `GetLightdInfo.taddr_support` is `true` only on stores produced by the
  wallet-serving backfill profile (per
  [wallet-data-plane §Transparent Address UTXOs](../architecture/wallet-data-plane.md#transparent-address-utxos)).
- `GetMempoolStream` closes cleanly on tip-change. Force a tip change with
  `docker exec z3_regtest_sidecar_zebra zebrad ... generate 1` and observe
  the stream end on the SDK side.
- `SendTransaction` round-trips through `BroadcastTransaction` and the
  resulting `SendResponse.error_code` matches the documented scheme
  (`0`, `-22`, `-26`, `-27`, `-1`).

For production Zashi/Zodl claims, keep the evidence stricter:

- Create-new-wallet bootstrap reaches the compat endpoint's advertised tip
  without protocol-shape errors.
- Restore/import and resync flows either request tree state and subtree roots at
  or above the wallet-serving store floor, or fail only with the documented
  strict `NOT_FOUND` unsupported-floor case. Unknown tree-state or subtree-root
  gaps are release blockers, not acceptable follow-up notes.
- `GetAddressUtxosStream` is backed by stored transparent UTXO artifacts.
  Synthetic empty responses, upstream-node fallbacks, and compact-block scans do
  not satisfy the wallet-serving contract.
- Pending-transaction UX requires both mempool surfaces: `GetMempoolTx` returns
  the compact transaction while pending, and `GetMempoolStream` emits the raw
  transaction before closing on the mining tip change.
- Send tests record writer-tip lag at submission time. A success when the writer
  is outside the wallet expiry window is not production evidence.

Reference comparison points: `zec.rocks` for public lightwalletd behavior,
the existing `wallet_sdk_scan.rs` for the in-process baseline.

## External integration: native `WalletQuery` API via grpcurl

For new wire shapes that no Rust client consumes yet (`BlockSelector`,
`BlockHeaderInfo`, the `TxStatus` oneof under
`wallet.read.transaction_by_id_v1`), validate by hand with `grpcurl`. The
scripted version of every probe below is at
[`scripts/native-grpc-smoke.sh`](../../scripts/native-grpc-smoke.sh) and
takes one optional positional argument (the `host:port` of a running
`zinder-query`). By default, it probes the latest visible block so it works
against checkpoint-bootstrapped stores. Set `ZINDER_QUERY_HEIGHT` when you need
to pin the probe to a specific artifact height:

```bash
scripts/native-grpc-smoke.sh 127.0.0.1:9069
```

The script verifies the standalone `WalletQuery` capability baseline, exercises
`LatestBlock`, both `BlockIdBySelector` arms (height + hash round-trip),
`BlockHeaderBySelector`, and asserts the `Transaction` NotFound mapping. Exit
code zero is the contract; any drift fails CI. The
`derive.explorer.transparent_balance_v1` capability is conditional on a fresh
derive proxy. Set `ZINDER_NATIVE_GRPC_EXPECT_DERIVE_BALANCE=1` when the query
process is started with `zinder-derive` and the smoke should require that
capability too.

### Bring up `zinder-query`

```bash
# Same store as ingest
cargo run --release --bin zinder-query -- \
  --config .tmp/regtest.reader.toml \
  --secondary-path .tmp/regtest.query-secondary \
  --listen-addr 127.0.0.1:9069
```

When `zinder-ingest` runs with `IngestControl` enabled (auth token, etc.),
also pass `--ingest-control-addr` and `--ingest-control-token-path` so
`ChainEvents` and `MempoolSnapshot`/`MempoolEvents` proxy correctly through
the writer (per the conventions above).

### Capability descriptor (sanity check)

```bash
grpcurl -plaintext \
  -import-path crates/zinder-proto/proto \
  -proto crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto \
  127.0.0.1:9069 zinder.v1.wallet.WalletQuery/ServerInfo | jq '.capabilities.capabilities'
```

Expected entries (this list is the public contract; treat any drift as a
breaking change requiring a capability rename). The
`zinder-proto::integration::capability_docs::testing_runbook_capability_list_mirrors_zinder_capabilities`
test asserts the list below equals
[`ZINDER_CAPABILITIES`](../../crates/zinder-proto/src/capabilities.rs):

<!-- capability-list:testing-runbook:start -->
```
wallet.read.latest_block_v1
wallet.read.block_id_by_selector_v1
wallet.read.block_header_by_selector_v1
wallet.read.compact_block_at_v1
wallet.read.compact_block_range_v1
wallet.read.tree_state_at_v1
wallet.read.latest_tree_state_v1
wallet.read.subtree_roots_in_range_v1
wallet.read.transaction_by_id_v1
wallet.read.server_info_v1
wallet.broadcast.transaction_v1
wallet.events.chain_v1
wallet.snapshot.mempool_v1
wallet.events.mempool_v1
wallet.mempool.transparent_outputs_by_address_v1
wallet.mempool.transparent_spend_by_outpoint_v1
wallet.mempool.transparent_prevouts_v1
wallet.read.transparent_prevouts_v1
wallet.address.transparent_utxos_v1
wallet.address.transparent_history_v1
derive.explorer.ready_v1
derive.explorer.transparent_balance_v1
```
<!-- capability-list:testing-runbook:end -->

Standalone `zinder-query` processes advertise the list above minus
`derive.explorer.transparent_balance_v1` until the configured derive proxy is
reachable and fresh.

### `BlockSelector` smoke

```bash
# Hash → block id
grpcurl -plaintext -import-path crates/zinder-proto/proto \
  -proto crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto \
  -d '{"selector":{"hash":"<base64 hash>"}}' \
  127.0.0.1:9069 zinder.v1.wallet.WalletQuery/BlockIdBySelector

# Hash → block header (Zinder-native shape, not zaino's)
grpcurl -plaintext -import-path crates/zinder-proto/proto \
  -proto crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto \
  -d '{"selector":{"hash":"<base64 hash>"}}' \
  127.0.0.1:9069 zinder.v1.wallet.WalletQuery/BlockHeaderBySelector
```

### `Transaction` (TxStatus oneof)

```bash
grpcurl -plaintext -import-path crates/zinder-proto/proto \
  -proto crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto \
  -d '{"transaction_id":"<base64 txid>"}' \
  127.0.0.1:9069 zinder.v1.wallet.WalletQuery/Transaction
```

Expect:

- `mined`/`in_mempool`/`conflicting` oneof for known transactions.
- gRPC `NOT_FOUND` (with a plain "not visible" message) for unknown
  transactions. The legacy `ArtifactUnavailable`-with-resource_info shape is
  gone.

## Failure interpretation reference

A live test failure is one of these classes. Read the error string before
debugging code.

| Error fragment | Class | Action |
| -------------- | ----- | ------ |
| `set ZINDER_TEST_LIVE=1 plus ZINDER_NETWORK and ZINDER_NODE__* env vars` | Opt-in missing | Add the env vars; not a real failure |
| `live test allowed only on [ZcashMainnet]` | Mainnet-only gate | Expected on regtest/testnet; refuse to bypass |
| `live test allowed only on [ZcashTestnet, ZcashMainnet]` | Hosted-network-only gate | Expected on regtest; rerun against testnet or mainnet |
| `requires ZINDER_NODE__INDEXER_GRPC_ADDR` | Indexer-feature missing | Restart Zebra with `ZEBRA_RPC__INDEXER_LISTEN_ADDR` and add the env var |
| `connection refused` / `transport error` | Sidecar not running | Restart z3 / verify ports |
| `chain_epoch.network does not match` | Wrong network | Check `ZINDER_NETWORK` matches the running Zebra |
| `schema mismatch` | Persisted store from a different schema version | Use a fresh `.tmp/<network>.zinder-store/` directory |
| `compact_block_range_too_large` / `invalid_block_range` | Real assertion | Treat as a real failure — investigate |
| `block_not_in_best_chain` from a hash lookup | Reorged out OR never indexed | Check the chain epoch you queried; not necessarily a bug |

If a test that previously passed starts failing, run with
`RUST_LOG=debug,zinder=trace` and the `--no-capture` flag for live tests:

```bash
ZINDER_TEST_LIVE=1 ... \
RUST_LOG=debug,zinder=trace \
cargo nextest run --profile=ci-live -E 'test(<failing_test>)' \
  --run-ignored=all --no-capture 2>&1 | tee .tmp/<failing_test>.log
```

The `.tmp/` directory is `.gitignore`'d for exactly this purpose.

## Ecosystem validation queue

Use this queue when closing the remaining ecosystem gaps after the default and
live-node gates are already green. Each item points at the owning detail instead
of repeating the full procedure here. Keep the evidence artifacts in `.tmp/`
unless the result changes a durable claim, in which case update the referenced
architecture or reference page.

| Gap | Friction | Run shape | Pass condition | Evidence to keep | Owning detail |
| --- | -------- | --------- | -------------- | ---------------- | ------------- |
| Full wallet-serving testnet backfill | Medium | Against a synced testnet Zebra, run `zinder-ingest backfill --wallet-serving` to a safe height, then `tip-follow`, `zinder-query`, and `zinder-compat-lightwalletd` over the same store. | Store covers the wallet-serving floor, readers open from secondaries, `/readyz` reports ready or bounded syncing, `GetLightdInfo` reports a non-zero height and `taddrSupport: true`. | Ingest and reader logs, `/readyz` JSON, `GetLightdInfo` output, first and last ingested heights. | [Android findings §Reproduction](../reference/android-wallet-integration-findings.md#reproduction), [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims) |
| Android SDK / Zashi bootstrap and restore | Medium | Point the SDK demo app or Zashi/Zodl at the testnet compat endpoint. Exercise create-new-wallet first, then restore/import or resync against the same serving store. | Create-new-wallet and restored/resync wallets reach the chain tip when requested tree-state and subtree-root heights are at or above the store floor. Below-floor requests fail only as the documented strict `NOT_FOUND` unsupported-floor case; unknown tree-state or subtree-root gaps are blockers. | `adb logcat`, SDK/Zashi endpoint config diff, wallet-visible height, store floor, requested tree-state/subtree-root heights, `GetAddressUtxosStream` sample. | [External integration: Zashi / Android SDK](#external-integration-zashi--android-sdk), [Android findings §Open questions](../reference/android-wallet-integration-findings.md#open-questions) |
| Send plus mempool end-to-end | Medium | With `tip-follow` and the mempool surface running, open the mempool stream, then submit a self-send from a real wallet through `zinder-compat-lightwalletd`. | `SendTransaction` returns the expected success mapping. `GetMempoolStream` emits `RawTransaction`, stays open while the tx is pending, and closes on mining. `GetMempoolTx` returns the compact tx while pending and empties after mining. Wallet scan-back observes the mined tx, and writer-tip lag stays inside the wallet expiry window. | Wallet logs, compat logs, mempool stream excerpt, `GetMempoolTx` before/after output, txid, mined height, `/readyz` lag sample at submit time. | [Android findings §Why mempool compatibility belongs in the wallet contract](../reference/android-wallet-integration-findings.md#why-mempool-compatibility-belongs-in-the-wallet-contract), [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims) |
| Real Zallet native-contract gate | Medium | Start a Zinder-native `zinder-query` integration target, then run `cargo nextest run --profile=ci-zallet-live --run-ignored=all` with `ZINDER_TEST_ZALLET*` pointing at a real Zallet binary and config. | The gate proves the Zallet config targets Zinder's native contract, rejects embedded-Zaino validator config, and observes the required Zallet command output. | Zallet config path, command output, nextest result, `zinder-query` logs. | [T3: Real Zallet binary gate](#t3-real-zallet-binary-gate), [Service operations §Zallet with Zinder](../architecture/service-operations.md#zallet-with-zinder) |
| Mainnet bounded live sweep | Low when local mainnet is synced | Run the mainnet-only `ci-live` tests against the local operator-hosted Zebra before attempting longer sessions. Prefer targeted filters first, then the full live profile if the node is healthy. | Mainnet-only tests pass on `ZINDER_NETWORK=zcash-mainnet`; non-mainnet tests either pass or skip for the documented reason. No test mutates mainnet state. | Nextest output, Zebra tip height, tested block range, node auth source used. | [T3: Live mainnet](#t3-live-mainnet-operator-hosted-zebra), [ADR-0006 §Open mainnet infrastructure questions](../adrs/0006-test-tiers-and-live-config.md#open-mainnet-infrastructure-questions) |
| lightwalletd / Zaino comparative parity | Medium | Run the same lightwalletd-compatible probes or wallet flow against Zinder and a known lightwalletd or Zaino endpoint. Include `GetBlockRange`, transaction lookup, transparent history, UTXO, mempool, and send shapes that the release claim names. | Zinder returns compatible response and error shapes where the lightwalletd contract applies. Any intentional difference is documented against the native contract or compatibility adapter. Hot-path timings stay inside the published budgets. | Per-method request/response summaries, error codes, timing samples, endpoint versions. | [Lessons from Zaino §Pattern 6](../reference/lessons-from-zaino.md#pattern-6-performance-as-a-sequential-implementation), [Wallet data plane §Performance and Pagination](../architecture/wallet-data-plane.md#performance-and-pagination) |
| Public deployment shape | Medium for internal CA, high for public cert | Put TLS in front of `zinder-compat-lightwalletd` with Caddy, nginx, or traefik, forwarding h2c to the local compat process. Use a private CA only for internal pilots; use a publicly trusted cert for public claims. | A real SDK or Zashi/Zodl endpoint validation succeeds through TLS, `GetLightdInfo` works through the proxy, public traffic cannot reach plaintext gRPC, `IngestControl`, or ops endpoints, proxy rate limiting is present for public exposure, and `--print-config` plus logs redact secrets. | Proxy config, cert source, endpoint validation logs, `grpcurl` result through TLS, bind-address audit, redacted `--print-config` output. | [Serving public lightwalletd clients §Operator recipe](../reference/serving-public-lightwalletd-clients.md#operator-recipe), [Service operations §Deployment guidance](../architecture/service-operations.md#deployment-guidance) |
| Observability and readiness warning semantics | Low | Run `scripts/observability-smoke.sh run` against regtest or a bounded public-network smoke. Inspect `/readyz`, Prometheus, and Grafana readiness panels after traffic generation. | Traffic-blocking readiness causes are zero. `cursor_at_risk` and `mempool_cursor_at_risk` are classified as warnings: `/readyz` still returns HTTP 200 with `"status": "ready"`, and metrics/dashboards expose the warning separately from load-balancer failure. | `.tmp/observability/reports/latest-readiness.json`, latest readiness Markdown, Prometheus query output, Grafana screenshot or panel JSON, service logs. | [Service operations §Health and Readiness](../architecture/service-operations.md#health-and-readiness), [Observability smoke](../../observability/README.md) |
| Backup, restore, and rolling restart | Low for smoke, medium for a long-running store | Run `scripts/observability-smoke.sh run` with the default backup restore enabled, or manually run `zinder-ingest backup --to <path>` and serve the checkpoint through a restored `zinder-query`. Restart query/compat, then restart ingest and verify readers catch up. | Restored `zinder-query` serves `WalletQuery/LatestBlock` at the checkpointed height, secondaries reopen from the restored or live store, restart/shutdown is clean, and a second writer cannot silently take over an already-open primary store. | Backup path, restore `LatestBlock` output, restart logs, `/readyz` after each restart, second-primary failure output. | [Service operations §Production Configuration](../architecture/service-operations.md#production-configuration), [Service operations §Recovery](../architecture/service-operations.md#recovery), [Observability smoke](../../observability/README.md) |
| Long soak | High | Run `tip-follow`, `zinder-query`, `zinder-compat-lightwalletd`, and any derive or mempool surface needed by the claim for several hours or longer while scraping metrics. Exercise read streams during the run. | No unexplained readiness flaps, writer-tip and reader lag stay bounded, cursor-retention and RocksDB alerts stay quiet, memory growth is explainable, and restart/shutdown is clean. | Metrics scrape, readiness samples, process logs, memory/RSS samples, final restart result. | [Service operations §Validation Tiers](../architecture/service-operations.md#validation-tiers), [Wallet data plane §Performance and Pagination](../architecture/wallet-data-plane.md#performance-and-pagination) |
| Release evidence manifest | Low | Before declaring a release candidate production-ready, collect the commands, versions, network, node tip, consumer versions, and artifact paths from the rows above into one file under `.tmp/production-readiness/<run-id>/manifest.md` or `.json`. | A reviewer can replay the certification story without chat history: every claimed consumer, network, command, binary version, and evidence artifact has a path and pass/fail result. | Manifest file, command transcript, commit SHA, binary versions, Zebra version and tip height, SDK/Zodl/Zallet versions. | [ADR-0012](../adrs/0012-consumer-release-certification.md), this runbook |

## Production release certification checklist

Use this checklist only when the goal is a production-readiness or release claim.
For ordinary code changes, use the shorter pre-flight checklist below.

- [ ] Default validation gate is green.
- [ ] `cargo nextest run --profile=ci-parity` is green and its report is archived
      with the release evidence.
- [ ] T3 regtest is green with Zebra indexer gRPC enabled.
- [ ] Testnet live sweep is green for every wallet-serving claim in the release.
- [ ] Mainnet read-only targeted tests are green when the release makes a mainnet
      readiness claim.
- [ ] Full wallet-serving backfill and `tip-follow` evidence exists for the target
      network, including store floor, tip height, `/readyz`, and reader secondary
      state.
- [ ] Zashi/Zodl or Android SDK bootstrap, restore/resync, transparent UTXO, send,
      and mempool evidence is green when claiming lightwalletd-compatible wallet
      support.
- [ ] Real Zallet binary gate is green when claiming Zallet support; the config
      must target Zinder's native contract and must reject embedded-Zaino
      validator config.
- [ ] Public deployment audit is green: TLS on the public wallet endpoint, no
      public plaintext gRPC, no public ops endpoints, no public `IngestControl`,
      rate limiting present, filesystem permissions restricted, and secrets
      redacted in logs and `--print-config`.
- [ ] Observability smoke or calibration report exists, with zero traffic-blocking
      readiness causes and warning causes classified separately.
- [ ] Backup restore and restart evidence exists for the store used in the claim.
- [ ] Long soak evidence exists when the release touches ingestion, storage,
      mempool, compatibility serving, or public deployment shape.
- [ ] A release evidence manifest points to every command, version, network,
      artifact, and pass/fail result used for the claim.

## Pre-flight checklist (before declaring a change shipped)

- [ ] Default validation gate green (`cargo fmt`/`cargo clippy`/`cargo nextest run --profile=ci`/`cargo nextest run --profile=ci-perf`/`cargo doc`/`cargo deny`).
- [ ] If the change touched a consumer-facing wire contract or compatibility
      adapter: `cargo nextest run --profile=ci-parity`.
- [ ] Live regtest sweep green (`ci-live` profile against z3, with indexer
      gRPC env). Hosted-network-only gates are expected to refuse regtest and
      should be rerun against testnet or mainnet.
- [ ] If the change claims Zallet compatibility: real Zallet binary gate green
      (`ci-zallet-live` with `ZINDER_TEST_ZALLET*` env against the Zinder-native
      contract).
- [ ] If the change touched the lightwalletd wire surface: a manual end-to-end
      run with a lightwalletd-compatible wallet or the Android SDK against
      `zinder-compat-lightwalletd`.
- [ ] If the change touched the native `WalletQuery` wire surface: grpcurl
      probes confirm the new shape and the capability descriptor reflects
      every added/removed/renamed cap.
- [ ] If the change altered storage byte layout: `cargo mutants` against
      `chain_store.rs` and `chain_store/validation.rs` is a healthy plus
      coverage run via `cargo llvm-cov`.
- [ ] If the change altered readiness, metrics, backup, restart, or deployment
      configuration: run the relevant observability or deployment row from the
      ecosystem validation queue.
- [ ] If the change is mainnet-relevant: filed against the open
      [ADR-0006](../adrs/0006-test-tiers-and-live-config.md) mainnet
      infrastructure work, not retrofitted into the default matrix.

## Latest local validation log

Pointer to the most recent end-to-end validation run that exercises consumers
beyond the automated gates. Each run produces a manifest under the gitignored
`.tmp/production-readiness/<run-id>/` per the *Release evidence manifest* row in
[Ecosystem validation queue](#ecosystem-validation-queue). Overwrite this
section when a newer run lands; the runbook is procedural, so only the latest
run lives here.

### Run `20260511T2242Z-zashi-zodl`

- Date: `2026-05-11`
- Commit: `e5e2ced` on `main`
- Network: `zcash-testnet`
- Operator-local manifest (gitignored): `.tmp/production-readiness/20260511T2242Z-zashi-zodl/manifest.md`

What passed:

- Every automated gate was green at this commit: the default validation gate,
  `ci-parity`, regtest and testnet `ci-live`, mainnet read-only checks, the
  mainnet 1,000-block calibration, the balance federation checks on testnet,
  mainnet, and regtest, and the observability smoke including backup restore.
- Two end-to-end sends from Zashi/Zodl on a Pixel 10 Pro through
  `zinder-compat-lightwalletd`:
  - Send 1, txid `edd8f9f1ffc0bb65d4e2f2b22607383af1c213405748baba4c64e145a961e0be`,
    mined at height `4006672`.
  - Send 2, txid `c8c8fd9830a311583e59c29348ec15ef49ce173350779774bf36985ab34ac9be`,
    observed pending in `GetMempoolTx` at poll `173`, then mined at height
    `4006680`.
- Pending mempool surfaces stayed consistent across the Send 2 window:
  `GetMempoolTx` was non-empty while pending and empty post-mine, and the
  native Zinder mempool snapshot `entryCount` transitioned `1` to `0`.
- Post-mine cleanup was clean: Zebra mempool size `0`, both `zinder-ingest`
  and `zinder-compat-lightwalletd` readiness `ready`.
- `GetLightdInfo` on the compat endpoint: `vendor = "Zinder"`,
  `chainName = "test"`, `blockHeight = 4006676`, `taddrSupport = true`.

### Follow-up run `20260512T0643Z-mempool-stream`

- Date: `2026-05-12`
- Network: `zcash-regtest`
- Operator-local manifest (gitignored): `.tmp/production-readiness/20260512T0643Z-mempool-stream/manifest.md`

Closes the *Non-empty `GetMempoolStream` byte capture* residual from the run
above. Phone-backed testnet captures remain structurally unwinnable for the
lightwalletd stream: testnet block time averages ~75 s, the lightwalletd
`GetMempoolStream` projection in
[`services/zinder-compat-lightwalletd/src/grpc.rs`](../../services/zinder-compat-lightwalletd/src/grpc.rs)
anchors at the snapshot sequence at stream-open time and closes on the first
observed best-chain tip change, and Zashi's Orchard tx-build runs ~60–120 s
end-to-end. The stream closes before the user's tx reaches the mempool. The
compat shim is network-agnostic, so a regtest capture is sufficient for the
wire-contract gate.

What passed on regtest:

- Stream emitted one `RawTransaction`: 207-byte V5 transparent tx, height
  `7403`. Stream closed cleanly (`exit 0`) when the test mined block `7404`.
- Block `7404` contains the broadcast tx (1-in/1-out V5) alongside the
  coinbase.
- The capture used a temporary, env-gated sleep in
  `broadcasting_signed_transparent_v5_surfaces_through_polling_mempool_source`
  to hold the broadcast in mempool past the running ingest's default 5 s
  mempool poll interval. That hook was reverted; the long-term mechanism for
  asserting the lightwalletd `GetMempoolStream` contract is a dedicated live
  test (filed separately).

### Follow-up run `20260512T0709Z-zaino-parity`

- Date: `2026-05-12`
- Network: `zcash-regtest`
- Operator-local manifest (gitignored): `.tmp/production-readiness/20260512T0709Z-zaino-parity/manifest.md`

Closes the *Comparative parity against an external lightwalletd or Zaino
endpoint* residual. Probed Zinder's compat shim (`127.0.0.1:9067`, plaintext)
side-by-side with `z3_regtest_sidecar_zaino` (`127.0.0.1:38137`, TLS,
self-signed) using identical `grpcurl` payloads against the same z3 regtest
Zebra. Both endpoints saw the same chain state (tip `7404`).

Surfaces that match: `GetLatestBlock`, `GetLatestTreeState`, `GetTreeState`,
`GetMempoolTx`, `GetMempoolStream`, `GetSubtreeRoots` (sapling, orchard).

Major interop divergences (manifest has the full matrix):

- **Zaino `GetBlock` returns stub blocks** on regtest at every probed height,
  with no `header` and no `vtx`. Any lightwalletd-compatible wallet that scans
  via `GetBlock`/`GetBlockRange` is unusable against this Zaino instance.
  Zinder returns the full compact block.
- **`TxFilter.hash` byte-order convention diverges.** Zinder accepts display
  (big-endian) order matching the upstream Go lightwalletd reference and the
  byte order Zashi/Zodl/librustzcash use; Zaino accepts wire (little-endian)
  order, the same convention used by `CompactTx.txid` (where the spec
  mandates protocol order). A client built for one indexer fails on the
  other.
- **Zaino leaks Rust type names into `Internal` gRPC errors.** `GetTransaction`
  (unknown) returns `... core::convert::Infallible error: RPC Error (code: -5)
  ...` and `SendTransaction` (malformed) returns
  `... zaino_fetch::jsonrpsee::response::SendTransactionError error: RPC Error
  (code: -22) ...`. Zinder maps these to clean `NotFound` and the documented
  `SendResponse{ errorCode, errorMessage }` shape respectively.

Zinder bug surfaced by this run (closed by [ADR-0015](../adrs/0015-network-parameter-discovery.md)):

- `GetLightdInfo` on regtest reported the wrong **active upgrade**:
  `consensusBranchId = e9ff75a6` (Canopy), `upgradeName = Canopy`,
  `upgradeHeight = 1`, while Zaino on the same node correctly reported
  `c8e71055` (NU6) at `2`. Root cause: the static `OnceLock<ZebraNetwork>`
  singleton was seeded from `RegtestParameters::default()` (zebra-chain
  library defaults), which leaves NU5/NU6/NU6\_1 unset; `NetworkUpgrade::current`
  fell back to Canopy at height 1. The same wrong branch ID flowed into
  `MinedDetails.consensus_branch_id` on the wallet read path. Fix: Zinder
  now discovers the schedule from the running node at startup and carries it
  as `Arc<NetworkUpgradeSchedule>` through every consumer; see
  [ADR-0015](../adrs/0015-network-parameter-discovery.md). Regression
  guarded by the live test
  `live::zebra_json_rpc::fetch_network_upgrade_schedule_matches_running_node_getblockchaininfo`
  in `crates/zinder-source/tests/live/zebra_json_rpc.rs`.

Volume difference on `GetTaddressTxids`/`GetTaddressTransactions` is the
documented wallet-serving floor (53 records from `7352`–`7404` vs Zaino's
7,354 from `127`–`7404`) and is intentional per [External integration: Zashi
/ Android SDK](#external-integration-zashi--android-sdk).

### Follow-up run `20260512T0715Z-tls-validation`

- Date: `2026-05-12`
- Network: `zcash-regtest`
- Operator-local manifest (gitignored): `.tmp/production-readiness/20260512T0715Z-tls-validation/manifest.md`

Closes the *Public TLS endpoint validation* residual using the topology
documented in [Serving public lightwalletd clients §Operator recipe](../reference/serving-public-lightwalletd-clients.md#operator-recipe):
Caddy terminates HTTPS on `:9443`, forwards `h2c` to
`zinder-compat-lightwalletd` at `127.0.0.1:9067`. Caddy provisions an
internal-CA cert on-demand for `localhost`. The same Caddyfile drops in for a
public deployment by swapping `tls internal` for an email address (Let's
Encrypt) and replacing `localhost` with the real hostname.

Eight RPCs probed plaintext vs TLS-fronted (`GetLightdInfo`, `GetLatestBlock`,
`GetMempoolTx`, `GetBlock(h=7404)`, `GetTreeState(h=7404)`, `GetTransaction`
with known display-order txid, `GetTransaction` with unknown txid,
`SendTransaction` with malformed bytes). Every response byte-identical across
the proxy. `Via: 2.0 Caddy` header on responses; ALPN negotiates `h2`;
HTTP/3 advertised via `Alt-Svc`. `IngestControl` (`127.0.0.1:9100`) was not
fronted by Caddy.

The Caddyfile and probe outputs are operator-local at
`.tmp/observability/Caddyfile-tls-validation` and
`.tmp/observability/evidence/20260512T0715Z-tls-validation/`.

### Follow-up run `20260512T0847Z-network-schedule-fix`

- Date: `2026-05-12`
- Networks: `zcash-regtest`, `zcash-testnet`, `zcash-mainnet`
- Operator-local manifest (gitignored): `.tmp/production-readiness/20260512T0847Z-network-schedule-fix/manifest.md`

Closes the regtest `GetLightdInfo` active-upgrade bug described above and
validates the end-to-end fix on all three supported networks. Architecture
locked into [ADR-0015](../adrs/0015-network-parameter-discovery.md):
per-network consensus parameters are discovered from the running node at
startup and shared as `Arc<NetworkUpgradeSchedule>`; the static
`OnceLock<ZebraNetwork>` and the free `consensus_branch_id_at(network,
height)` are gone.

Live regression test result
(`live::zebra_json_rpc::fetch_network_upgrade_schedule_matches_running_node_getblockchaininfo`):

| Network | Tip | Result |
|---------|-----|--------|
| regtest | 7,404 | PASS |
| testnet | 4,007,339 | PASS |
| mainnet | 3,339,611 | PASS |

Wire-level `GetLightdInfo` parity against the running Zebra's
`getblockchaininfo` for each network, with the post-fix `target/release`
binaries running in the observability (regtest), testnet, and a one-shot
mainnet verification stack:

| Network | `consensusBranchId` (Zinder = Zebra `chaintip`) | `upgradeName` | `upgradeHeight` | `saplingActivationHeight` |
|---------|-------------------------------------------------|---------------|-----------------|----------------------------|
| regtest | `c8e71055` (NU6) | `NU6` | 2 | 1 |
| testnet | `4dec4df0` (NU6.1) | `NU6.1` | 3,536,500 | 280,000 |
| mainnet | `4dec4df0` (NU6.1) | `NU6.1` | 3,146,400 | 419,200 |

Every wire field matches the running Zebra exactly. The `upgradeName`
formatting also improved across the board: pre-fix Zinder returned the
zebra-chain library's `Debug` rendering (e.g. `Nu6_1`); post-fix Zinder
carries the node's canonical `name` field verbatim (e.g. `NU6.1`), matching
the lightwalletd-go reference behavior.

A second live regression now pins the wallet-read path as well:
`live::mined_consensus_branch_id_parity::mined_details_consensus_branch_id_matches_node_upgrade_schedule`
in
[`services/zinder-ingest/tests/live/mined_consensus_branch_id_parity.rs`](../../services/zinder-ingest/tests/live/mined_consensus_branch_id_parity.rs)
backfills a small near-tip window through `zinder-ingest`, opens
`zinder-query::WalletQuery` with the live-discovered schedule, looks up the
tip coinbase via `WalletQueryApi::transaction(...)`, and asserts that
`MinedDetails.consensus_branch_id ==
schedule.consensus_branch_id_at(mined_height)`. This closes the gap left
open by the `lightwalletd_grpc` integration tests, which only exercise the
in-process adapter against a synthetic schedule.

| Network | Tip at run-time | Result |
|---------|-----------------|--------|
| regtest | 7,404 | PASS |
| testnet | 4,007,360 | PASS |
| mainnet | 3,339,637 | PASS |

### Residual gates

After the runs above. Each cross-refs the section that owns the procedure.

- **Real Zallet binary gate.** Not runnable in this stack: no
  `ZINDER_TEST_ZALLET*` env and no `zallet` binary on `PATH`. Public Zallet
  builds use embedded Zaino, which the gate intentionally rejects. Tracked as
  upstream dependency on a Zinder-native Zallet branch. See
  [T3: Real Zallet binary gate](#t3-real-zallet-binary-gate).

## Cross-references

- [ADR-0006: Test tiers and live config](../adrs/0006-test-tiers-and-live-config.md) — the structural rules.
- [ADR-0007: Multi-process storage access](../adrs/0007-multi-process-storage-access.md) — secondary catchup and writer-status semantics that some live tests verify.
- [ADR-0009: IngestControl transport security](../adrs/0009-ingest-control-transport-security.md) — the bearer-token contract referenced in the external-integration conventions.
- [ADR-0012: Consumer release certification](../adrs/0012-consumer-release-certification.md) — the `ci-parity` profile and release evidence contract.
- [Service operations](../architecture/service-operations.md) — the operator-facing deployment story; the external-integration recipes here use its single-host, single-store conventions. For multi-host or multi-process deployments, follow that doc's recipes instead.
- [Wallet data plane](../architecture/wallet-data-plane.md) — the wire surface the external integration tests exercise.
- [Android wallet integration findings](../reference/android-wallet-integration-findings.md) — the verified SDK/Zashi reproduction path, deployment implications, and unresolved wallet-serving questions.
- [Serving public lightwalletd clients](../reference/serving-public-lightwalletd-clients.md) — the public endpoint, TLS, rate-limit, and operator gap checklist for lightwalletd-compatible wallets.
- [Lessons from Zaino](../reference/lessons-from-zaino.md) — comparison points for "is this behavior right" judgment calls during external-integration testing.
- [Observability smoke](../../observability/README.md) — the local metrics, readiness, and backup-restore evidence harness.
- [`scripts/native-grpc-smoke.sh`](../../scripts/native-grpc-smoke.sh) — the scripted version of the manual `grpcurl` recipes below, callable from CI or a dev shell.
