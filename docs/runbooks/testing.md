# Testing Runbook

Operational procedures for validating Zinder against its workspace, real Zebra
nodes, and external wallet applications. The structural rules behind the test
tiers live in [ADR-0006](../adrs/0006-test-tiers-and-live-config.md) and
[ADR-0007](../adrs/0007-multi-process-storage-access.md); this document is the
step-by-step guide for actually running them.

## Test tier matrix

| Tier | Location | Profile | Trigger | Catches |
| ---- | -------- | ------- | ------- | ------- |
| T0 unit | `#[cfg(test)] mod tests in src/` | `default-filter` of `default`/`ci` | Every commit | Logic regressions in the unit under test |
| T1 integration | `tests/integration/` | `default-filter` of `default`/`ci` | Every commit | Cross-module wiring, gRPC adapter shape, store/proto round-trips |
| T2 perf | `tests/perf/` | `ci-perf` | Every commit | Latency budget regressions per the published budgets |
| T3 live | `tests/live/` | `ci-live` | Manual / scheduled CI | Real upstream-node behavior (Zebra JSON-RPC, indexer gRPC) |
| External | n/a | n/a | Manual | Real-wallet integration (Zallet, Zashi/Android SDK, public lightwalletd clients) |

`default-filter = "not test(/^live::/) and not test(/^perf::/)"` is the
structural boundary. Every live test additionally carries
`#[ignore = LIVE_TEST_IGNORE_REASON]` and a runtime
`zinder_testkit::live::require_live()` check, so a stray `cargo test` cannot
talk to a node by accident.

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

Expected outcome on a healthy regtest: **25 passed, 3 failed, 397 skipped**
in ~30 s. The three remaining failures are the mainnet-only tests
(`fetch_chain_checkpoint_returns_advancing_tree_sizes_on_mainnet`,
`tip_id_advances_above_one_million`,
`backfills_last_1000_blocks_from_checkpoint`); they refuse to run on regtest
by design.

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

## External integration: Zallet via lightwalletd compat shim

Zallet's current build links `lightwalletd-tonic-tls-webpki-roots` and consumes
the lightwalletd protocol. The breaking changes in this batch reach Zallet
through `services/zinder-compat-lightwalletd`, not the native API.

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

### End-to-end with a real Zallet binary

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
# Terminal 3: Zallet pointing at the compat shim
cd /Users/gustavovalverde/dev/zfnd/wallet
# Configure the lightwalletd endpoint in Zallet's config (see Zallet's docs
# for the canonical config field; the value is http://127.0.0.1:9067).
cargo run --bin zallet -- <wallet-command>
```

What this catches that the deterministic test does not:

- `GetTransaction` NotFound mapping (Zallet sees plain `NOT_FOUND`, not the
  legacy `ArtifactUnavailable`-with-resource_info detail).
- `GetBlock { height=0, hash }` flow through the new `BlockSelector`
  resolver.
- `GetTaddressTxids` / `GetTaddressTransactions` via the transparent-history
  index.
- Real-world streaming and connection-reuse patterns the in-process test
  cannot reproduce.

If you transition Zallet to `zinder-client::RemoteChainIndex` (per
[service-operations §Zallet with Zinder](../architecture/service-operations.md#zallet-with-zinder)),
re-run this same setup pointing Zallet at the `zinder-query` endpoint instead
of the compat shim.

## External integration: Zashi / Android SDK

Same compat-shim path as Zallet. The Android SDK speaks lightwalletd; point it
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

Reference comparison points: `zec.rocks` for public lightwalletd behavior,
the existing `wallet_sdk_scan.rs` for the in-process baseline.

## External integration: native `WalletQuery` API via grpcurl

For new wire shapes that no Rust client consumes yet (`BlockSelector`,
`BlockHeaderInfo`, the `TxStatus` oneof under
`wallet.read.transaction_by_id_v1`), validate by hand with `grpcurl`. The
scripted version of every probe below is at
[`scripts/native-grpc-smoke.sh`](../../scripts/native-grpc-smoke.sh) and
takes one optional positional argument (the `host:port` of a running
`zinder-query`):

```bash
scripts/native-grpc-smoke.sh 127.0.0.1:9069
```

The script verifies the capability descriptor matches `ZINDER_CAPABILITIES`
exactly, exercises `LatestBlock`, both `BlockIdBySelector` arms (height +
hash round-trip), `BlockHeaderBySelector`, and asserts the `Transaction`
NotFound mapping. Exit code zero is the contract; any drift fails CI.

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

## Pre-flight checklist (before declaring a change shipped)

- [ ] Default validation gate green (`cargo fmt`/`cargo clippy`/`cargo nextest run --profile=ci`/`cargo nextest run --profile=ci-perf`/`cargo doc`/`cargo deny`).
- [ ] Live regtest sweep green (`ci-live` profile against z3, with indexer
      gRPC env). Three mainnet-only failures are expected and acceptable.
- [ ] If the change touched the lightwalletd wire surface: a manual end-to-end
      run with Zallet or the Android SDK against `zinder-compat-lightwalletd`.
- [ ] If the change touched the native `WalletQuery` wire surface: grpcurl
      probes confirm the new shape and the capability descriptor reflects
      every added/removed/renamed cap.
- [ ] If the change altered storage byte layout: `cargo mutants` against
      `chain_store.rs` and `chain_store/validation.rs` is a healthy plus
      coverage run via `cargo llvm-cov`.
- [ ] If the change is mainnet-relevant: filed against the open
      [ADR-0006](../adrs/0006-test-tiers-and-live-config.md) mainnet
      infrastructure work, not retrofitted into the default matrix.

## Cross-references

- [ADR-0006: Test tiers and live config](../adrs/0006-test-tiers-and-live-config.md) — the structural rules.
- [ADR-0007: Multi-process storage access](../adrs/0007-multi-process-storage-access.md) — secondary catchup and writer-status semantics that some live tests verify.
- [ADR-0009: IngestControl transport security](../adrs/0009-ingest-control-transport-security.md) — the bearer-token contract referenced in the external-integration conventions.
- [Service operations](../architecture/service-operations.md) — the operator-facing deployment story; the external-integration recipes here use its single-host, single-store conventions. For multi-host or multi-process deployments, follow that doc's recipes instead.
- [Wallet data plane](../architecture/wallet-data-plane.md) — the wire surface the external integration tests exercise.
- [Lessons from Zaino](../reference/lessons-from-zaino.md) — comparison points for "is this behavior right" judgment calls during external-integration testing.
- [`scripts/native-grpc-smoke.sh`](../../scripts/native-grpc-smoke.sh) — the scripted version of the manual `grpcurl` recipes below, callable from CI or a dev shell.
