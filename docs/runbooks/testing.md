# Testing Runbook

Operational procedures for validating Zinder against its workspace, real Zebra
nodes, and external wallet applications. This document owns the test tiers,
runner profiles, live-node gates, and consumer-facing certification evidence.

## Test tier matrix

| Tier | Location | Profile | Trigger | Catches |
| ---- | -------- | ------- | ------- | ------- |
| T0 unit | `#[cfg(test)] mod tests in src/` | `default-filter` of `default`/`ci` | Every commit | Logic regressions in the unit under test |
| T1 integration | `tests/integration/` | `default-filter` of `default`/`ci` | Every commit | Cross-module wiring, gRPC adapter shape, store/proto round-trips |
| T2 perf | `tests/perf/` | `ci-perf` | Every commit | Latency budget regressions per the published budgets |
| Consumer parity | `crates/zinder-client/tests/parity/` | `ci-parity` | Consumer contract changes / release certification | Consumer-shaped request and error-shape regressions for lightwalletd-compatible wallets, Zallet, public lightwalletd operators, and explorers |
| T3 live | `tests/live/` | `ci-live` | Manual / scheduled CI | Real upstream-node behavior (Zebra JSON-RPC, indexer gRPC) |
| T3 Zallet live | `crates/zinder-client/tests/live/zallet.rs` | `ci-zallet-live` | Release / integration certification | Real Zallet binary using Zinder's native contract |
| External | n/a | n/a | Manual | Exploratory wallet runs (Zodl/Android SDK, public lightwalletd clients) |

`default-filter = "not test(/^live::/) and not test(/^perf::/) and not test(/^parity::/) and not test(/^deploy::/)"`
is the structural boundary. The regular `ci-live` profile excludes
`live::zallet::`; run `ci-zallet-live` explicitly when a Zallet build that
targets Zinder is available. Every live test additionally carries
`#[ignore = LIVE_TEST_IGNORE_REASON]` and a runtime
`zinder_testkit::live::require_live()` check, so a stray `cargo test` cannot
talk to a node by accident.

`ci-parity` is fixture-backed and does not require `ZINDER_TEST_LIVE`. Treat it
as a consumer-contract gate for request and error shapes, not as a
replacement for live SDK, Zallet, or network validation.

## Lightwalletd certification

The compatibility adapter is certified in layers: vendored-protocol coverage,
live parity against the pinned reference, an independent-client flow, then the
actual public deployment. The required boundaries and reference pins live in
[the certification plan](../plans/lightwalletd-compatibility-certification.md).

Keep the evidence simple: retain the exact commands, image digests, client
version, network, wallet-serving floor, and command output with the release.
Do not generate a repository-specific report or manifest merely to restate
those results.

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
scripts/runbook-lint.sh docs/runbooks/testing.md
git diff --check
```

Expected outcome: every command exits zero. The full suite runs in ~5 minutes
on a developer laptop. If `cargo machete` reports unused dependencies on a
crate you did not touch, treat it as a pre-existing finding, not a blocker.

`scripts/runbook-lint.sh` parses every fenced `bash` block in this runbook
through `bash -n` (syntax-only mode) so a typo or unclosed quote in an
operator recipe fails CI immediately rather than ambushing an on-call
engineer. It does not execute the blocks; running them still requires the
documented prerequisites. See [Runbook self-test](#runbook-self-test)
below for the full contract.

## Consumer parity gate (consumer-shaped fixtures)

Run before tagging a release, and after any change that touches a consumer-facing
wire contract or compatibility adapter:

```bash
cargo nextest run --profile=ci-parity
```

Expected outcome: every `parity::` module exits zero and the report is archived
with the release evidence. This profile is organized by consumer:

- `parity/zodl.rs` covers the lightwalletd-compatible shapes Zodl and the
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
`bulk_catchup_last_1000_blocks_from_checkpoint`) and the testnet-or-mainnet tests
(`cli_bulk_catchup_bounded_wallet_serving_floor_from_config`,
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
bulk-catchup runs). Run them in isolation if you want to debug — parallel execution
will fight for the same node.

## T3: Live testnet (Zebra cookie auth)

Same surface as regtest but against real testnet. Useful for capability probes
and tip-fetch validation against a node with non-trivial chain weight.

```bash
cookie=$(docker exec <zebra_container> cat /var/run/auth/.cookie)
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-testnet \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:18232 \
  ZINDER_NODE__INDEXER_GRPC_ADDR=http://127.0.0.1:18155 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=${cookie%%:*} \
  ZINDER_NODE__AUTH__PASSWORD=${cookie#*:} \
  cargo nextest run --profile=ci-live --run-ignored=all
```

`require_live()` accepts testnet by default; nothing further to opt in. Tests
that target only mainnet (see below) still skip. Tests that exercise
regtest-only RPCs (`generate`, `invalidateblock`, `reconsiderblock`) opt in
via `require_live_for(&[Network::ZcashRegtest])` and refuse to run here.

## T3: Live mainnet (operator-hosted Zebra)

Local mainnet runs are supported against an operator-hosted Zebra:

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
`bulk_catchup_last_1000_blocks_from_checkpoint`, plus the transparent-address
balance read-only confirmations under `services/zinder-explorer/tests/live/`.

## T3: Parity against a reference lightwalletd

Compares `zinder-compat-lightwalletd` against
`electriccoinco/lightwalletd:latest`, both pointed at the same Zebra.
Catches drift in the wire-shape contract `LightdInfo`, `BlockId`,
`CompactBlock`, and `RawTransaction` use across the two shims.

These tests live in
[`services/zinder-compat-lightwalletd/tests/live/parity_against_lightwalletd.rs`](../../services/zinder-compat-lightwalletd/tests/live/parity_against_lightwalletd.rs)
and need both shims running on host-reachable ports. They skip when either
env var is absent so the default `ci-live` invocation does not require a
parity sidecar pair:

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_TEST_PARITY_ZINDER_ADDR=http://127.0.0.1:9087 \
  ZINDER_TEST_PARITY_LIGHTWALLETD_ADDR=http://127.0.0.1:9088 \
  cargo nextest run --profile=ci-live --run-ignored=all \
    -E 'test(/^live::parity_against_lightwalletd::/)'
```

Operator-divergent fields (build metadata, version strings, donation
address) are intentionally allow-listed; the assertions focus on the wire
shape both shims must agree on. The fixture-backed `ci-parity` profile
covers the same byte-shape contract without the running-binaries
dependency and is the gate enforced on every CI run.

## T3: Reorg sweep

Forces canonical-chain reorgs on the running regtest sidecar via
`invalidateblock`/`reconsiderblock` and asserts that the writer's
`IngestControl.ChainEvents` stream emits a `ChainReorged` envelope whose
reverted range covers the invalidated heights. Catches drift between
Zebra's chain rollback and Zinder's reorg-detection logic at the seam
between `tip_follow` and `PrimaryChainStore::commit_chain_epoch`.

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  cargo nextest run --profile=ci-live -E 'test(/^live::reorg_sweep::/)' \
    --run-ignored=all
```

What it covers (file:
[`services/zinder-ingest/tests/live/reorg_sweep.rs`](../../services/zinder-ingest/tests/live/reorg_sweep.rs)):

- `single_block_reorg_surfaces_chain_reorged_envelope`: invalidates the
  current tip, mines two replacement blocks, asserts a `ChainReorged`
  envelope appears on the IngestControl stream whose `reverted.start_height`
  equals the invalidated height.
- `three_block_reorg_covers_full_reverted_range`: invalidates the block
  three heights below tip, mines five replacement blocks, asserts the
  reverted range spans exactly three heights.

These tests join the `node-mutating` group in `.config/nextest.toml`; they
serialize against the broadcast cycle and indexer-restart tests so they
can share the regtest sidecar without racing. Mainnet/testnet reorgs are
not exercised — forcing reorgs on a real network is destructive and
out of scope for this gate.

## T3: Network upgrade boundary crossing

Pins the per-height consensus-branch-id contract across a network
upgrade activation. The single-tip
[`mined_consensus_branch_id_parity`](../../services/zinder-ingest/tests/live/mined_consensus_branch_id_parity.rs)
test only samples the chain tip; this companion samples three heights
that straddle the latest reachable activation, so a regression in
[ADR-0008](../adrs/0008-network-parameter-discovery.md)'s discovery path
or in how `MinedDetails.consensus_branch_id` is populated surfaces here
even when the tip happens to be in a "stable" upgrade window.

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  cargo nextest run --profile=ci-live \
    -E 'test(/^live::network_upgrade_boundary::/)' --run-ignored=all
```

The same invocation works for testnet (`ZINDER_NETWORK=zcash-testnet`,
target activation = NU6.1 at height 3,536,500) and mainnet
(`ZINDER_NETWORK=zcash-mainnet`, target activation = NU6.1 at height
3,146,400). On testnet/mainnet the test anchors a checkpoint just below
the activation so the bulk catchup window stays at three blocks.

File:
[`services/zinder-ingest/tests/live/network_upgrade_boundary.rs`](../../services/zinder-ingest/tests/live/network_upgrade_boundary.rs).

## T3: Writer fencing and crash recovery

Multi-process storage access is the production deployment shape per
[ADR-0003](../adrs/0003-canonical-storage-access-boundary.md): one
`zinder-ingest` primary writer plus N `zinder-query` and
`zinder-compat-lightwalletd` secondary readers. This section validates
both the structural fencing (RocksDB primary lock) and the
operationally critical "writer crashed, readers survive" semantic.

### Automated coverage

Three integration tests under
[`crates/zinder-store/tests/integration/primary_secondary.rs`](../../crates/zinder-store/tests/integration/primary_secondary.rs)
run with every `cargo nextest run --profile=ci`:

- `second_primary_open_returns_primary_already_open` — proves a second
  writer cannot silently take over an already-open primary store; the
  RocksDB lock surfaces as `StoreError::PrimaryAlreadyOpen`.
- `secondary_continues_serving_after_primary_drops` — opens a primary,
  commits two epochs, drops the primary handle, then asserts a fresh
  secondary opened against the same path serves the last-committed
  epoch without the primary coming back up. Phase 3 of the same test
  asserts a restarted primary resumes from the durable state.
- `secondary_catches_up_after_primary_commits` — the live-writer
  catchup baseline; pre-existing.

### Manual reproduction: kill -9 mid-commit

Use this when changing storage commit paths or the
`PrimaryChainStore::commit_chain_epoch` body. The automated test cannot
exercise unclean shutdown mid-batch because dropping the handle in
process closes RocksDB cleanly.

```bash
# Terminal 1: start the unified ingest loop against z3 regtest.
mkdir -p .tmp
rm -rf .tmp/regtest.zinder-store
cargo run --release --bin zinder-ingest -- \
  --config .tmp/regtest.ingest.toml &
INGEST_PID=$!

# Terminal 1 (after a few blocks commit): SIGKILL the writer mid-flight.
sleep 5
kill -9 "$INGEST_PID"
wait "$INGEST_PID" 2>/dev/null || true

# Reopen and assert state is consistent. The reopen must succeed and the
# cursor must point at a durable height; if reopen fails with
# SchemaMismatch / corruption, that's a real defect.
cargo run --release --bin zinder-ingest -- \
  --config .tmp/regtest.ingest.toml print-config
cargo run --release --bin zinder-query -- \
  --config .tmp/regtest.reader.toml --listen-addr 127.0.0.1:9069 &
QUERY_PID=$!
sleep 2
grpcurl -plaintext \
  -import-path crates/zinder-proto/proto \
  -proto crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto \
  127.0.0.1:9069 zinder.v1.wallet.WalletQuery/LatestBlock
kill "$QUERY_PID" 2>/dev/null || true
```

Expected: `LatestBlock` returns a real chain epoch. RocksDB's WAL replay
restores any in-flight committed batch on reopen; partial commits are
visible only after their write batch is durably persisted, so the
reopened tip is always at-or-below the last batch the writer signaled
durable.

### Manual reproduction: second-writer fencing across processes

```bash
cargo run --release --bin zinder-ingest -- \
  --config .tmp/regtest.ingest.toml &
sleep 2
# This second invocation must fail with PrimaryAlreadyOpen (LOCK file in
# .tmp/regtest.zinder-store/LOCK). If it succeeds, the fencing contract
# is broken.
cargo run --release --bin zinder-ingest -- \
  --config .tmp/regtest.ingest.toml
```

Expected: the second `zinder-ingest` process exits non-zero with an
error message naming the LOCK path. The first process continues
uninterrupted.

## Concurrent readers and stream cancellation

Production deployments fan a single `zinder-query` server out behind
multiple `zinder-compat-lightwalletd` processes (per
[service-operations](../architecture/service-operations.md)). The gRPC
adapter must (a) serve concurrent streamed reads without serializing
them, and (b) clean up server-side resources when a client disconnects
mid-stream.

### Automated coverage

Two integration tests under
[`services/zinder-query/tests/integration/stream_cancellation.rs`](../../services/zinder-query/tests/integration/stream_cancellation.rs)
run with every `cargo nextest run --profile=ci`:

- `dropping_compact_block_range_stream_does_not_break_subsequent_requests`
  — opens a `compact_block_range` stream, reads one chunk, drops it,
  then opens a second stream and drains it. Asserts the server stays
  responsive after a mid-stream cancellation.
- `parallel_compact_block_range_readers_all_drain_to_completion` —
  spawns 16 concurrent clients each draining the full committed range
  and asserts every reader finishes within a 20-second deadline.

### Manual reproduction: N concurrent compat readers

Operator-grade reproduction needs a real bulk-caught-up store plus a real
gRPC client. Two terminals:

```bash
# Terminal 1: serve over a populated store.
cargo run --release --bin zinder-compat-lightwalletd -- \
  --config .tmp/regtest.reader.toml \
  --secondary-path .tmp/regtest.compat-secondary \
  --listen-addr 127.0.0.1:9067

# Terminal 2: fan out N parallel GetBlockRange calls.
for reader_index in $(seq 1 10); do
  grpcurl -plaintext \
    -import-path crates/zinder-proto/proto/compat/lightwalletd \
    -proto crates/zinder-proto/proto/compat/lightwalletd/service.proto \
    -d '{"start":{"height":1},"end":{"height":100}}' \
    127.0.0.1:9067 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange \
    > ".tmp/reader-$reader_index.out" &
done
wait
wc -l .tmp/reader-*.out
```

Expected: every reader receives a non-zero number of lines, no
`-1`/transport errors, and no reader hangs past the others.

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
  tree_state_checkpoint_at_or_before         = 249 µs
```

These numbers are not asserted by the test; treat the printed values as
operator-facing telemetry. Compare across changes to spot regressions in
the read path. The baseline does not exercise `transaction()` — that path
goes through `mempool_broadcast_cycle` (broadcast then look up the mined
txid) and `deep_chain` (high-volume bulk catchup).

### Heavy probes (trust-sensitive code only)

Run on demand for changes to storage commit paths, parser code, or anything
under `chain_store::validation`:

```bash
cargo llvm-cov --workspace --all-features --no-report
cargo mutants --workspace --all-features \
  --file crates/zinder-store/src/chain_store.rs \
  --file crates/zinder-store/src/chain_store/validation.rs \
  --file crates/zinder-source/src/source_block.rs \
  --re 'chain_event_history|safe_tip_only_commit_without_artifacts|validate_reorg_window_change|from_raw_block_bytes'
```

These are slow (mutants often >30 min). Scheduled CI runs them weekly; locally,
gate on whether you actually changed trust-sensitive code.

## T3: Real Zallet binary gate

Zallet compatibility is a native-client claim, not a lightwalletd-compat claim.
The gate must exercise a Zallet binary and configuration that target Zinder's
native contract. A validator-only configuration is not a Zinder certification
path because it bypasses `zinder-query` and the `ChainIndex` client surface.

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
  `validator_password` entries are rejected because they are validator-direct
  configuration.
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
  that precedence order. Sensitive fields may come from TOML or the
  environment and are redacted at emit boundaries. Recipes for long-running
  binaries still prefer TOML or the operator secret-management layer so
  shell history and process environments stay clean.
- Pass `--ops-listen-addr 127.0.0.1:<port>` to expose `/healthz`,
  `/readyz`, and Prometheus `/metrics` on each runtime. Useful when
  debugging stuck live tests or watching ingest catch-up; the metrics
  endpoint is the same one CI scrapes.
- When the writer is configured with a shared-secret bearer token (per
  [ADR-0006 §Optional bearer token](../adrs/0006-ingest-control-transport-security.md#optional-shared-secret-bearer-token-loaded-from-a-file)),
  every reader needs `--ingest-control-token-path <PATH>` pointing at the
  same plain-text file. The file holds the token verbatim; the runtime
  loads it into `secrecy::SecretString` so it never appears in logs or
  `--print-config`. Without it, readers fall through to plaintext h2c
  (the localhost default).
- Each binary has its own config schema; `zinder-ingest` carries an
  `[ingest]` section while `zinder-query` and
  `zinder-compat-lightwalletd` reject it as an unknown field. Write per-process
  configs in `.tmp/`:

  ```bash
  mkdir -p .tmp

  # zinder-ingest (writer)
  cat >.tmp/regtest.ingest.toml <<'EOF'
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

  [ingest]
  source = "zebra-json-rpc"
  EOF

  # zinder-query and zinder-compat-lightwalletd share this reader config.
  # Same node + storage block, no `[ingest]` section.
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
# Terminal 1: zinder-ingest unified loop against z3 regtest
cargo run --release --bin zinder-ingest -- \
  --config .tmp/regtest.ingest.toml
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
# Configure the endpoint to http://127.0.0.1:9067, then run the wallet/SDK
# command (Zodl adb command, Android demo, zec-rocks-grpcurl probe, etc.):
#   ./run-wallet-against http://127.0.0.1:9067
```

What this catches that the deterministic test does not:

- `GetTransaction` NotFound mapping (the client sees plain `NOT_FOUND` for unknown transactions).
- `GetBlock { height=0, hash }` flow through the `BlockSelector` resolver.
- `GetTaddressTxids` / `GetTaddressTransactions` via the transparent-history
  index.
- Real-world streaming and connection-reuse patterns the in-process test cannot
  reproduce.

## External integration: Zodl / Android SDK

Same compat-shim path. The Android SDK speaks lightwalletd; point it
at the running `zinder-compat-lightwalletd:9067`.

What to validate by hand:

- `GetLightdInfo.taddr_support` is `true` only on stores produced by the
  wallet-serving bulk-catchup profile (per
  [wallet-data-plane §Transparent Address Outputs](../architecture/wallet-data-plane.md#transparent-address-outputs)).
- `GetMempoolStream` closes cleanly on tip-change. Force a tip change with
  `docker exec z3_regtest_sidecar_zebra zebrad ... generate 1` and observe
  the stream end on the SDK side.
- `SendTransaction` round-trips through `BroadcastTransaction` and the
  resulting `SendResponse.error_code` matches the documented scheme
  (`0`, `-22`, `-26`, `-27`, `-1`).

For production Zodl claims, keep the evidence stricter:

- Create-new-wallet bootstrap reaches the compat endpoint's advertised tip
  without protocol-shape errors.
- Restore/import and resync flows either request tree state and subtree roots at
  or above the wallet-serving store floor, or fail only with the documented
  strict `NOT_FOUND` unsupported-floor case. Unknown tree-state or subtree-root
  gaps are release blockers, not acceptable follow-up notes.
- `GetAddressUtxosStream` is backed by stored transparent output artifacts.
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
code zero is the contract; any drift fails CI.
`wallet.address.transparent_balance_v1` is always advertised: the confirmed
total reads the canonical unspent index, and the mempool overlay degrades to a
zero delta when no ingest-control endpoint is wired.

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
test asserts the list below equals the wallet and explorer rows of the
[`CAPABILITIES`](../../crates/zinder-proto/src/capabilities.rs) table:

<!-- capability-list:testing-runbook:start -->
```
wallet.read.latest_block_v1
wallet.read.block_id_by_selector_v1
wallet.read.block_header_by_selector_v1
wallet.read.compact_block_at_v1
wallet.read.compact_block_range_v1
wallet.read.compact_block_ironwood_v1
wallet.read.full_block_at_v1
wallet.read.full_block_range_v1
wallet.read.tree_state_at_height_v1
wallet.read.latest_tree_state_checkpoint_v1
wallet.read.subtree_roots_in_range_v1
wallet.read.subtree_roots_ironwood_v1
wallet.read.transaction_by_id_v1
wallet.read.transaction_bytes_v1
wallet.read.server_info_v1
wallet.broadcast.transaction_v1
wallet.events.chain_v1
wallet.snapshot.mempool_v1
wallet.events.mempool_v1
wallet.mempool.transparent_outputs_by_address_v1
wallet.mempool.transparent_spends_by_outpoint_v1
wallet.mempool.transparent_outputs_by_outpoint_v1
wallet.read.transparent_outputs_by_outpoint_v1
wallet.read.transparent_spends_by_outpoint_v1
wallet.read.transparent_unspent_outputs_by_outpoint_v1
wallet.read.chain_value_pools_at_tip_v1
wallet.read.transparent_utxo_set_summary_v1
wallet.read.transparent_utxo_set_commitment_v1
wallet.address.transparent_unspent_outputs_v1
wallet.address.transparent_history_v1
wallet.address.transparent_balance_v1
explorer.server_info_v1
explorer.transaction.detail_v1
explorer.block.summary_v1
explorer.block.detail_v1
explorer.search_v1
explorer.mempool.summary_v1
explorer.mempool.activity_v1
explorer.transparent_address.activity_v1
explorer.transparent_address.deltas_v1
explorer.fee.summary_v1
explorer.value_pool.summary_v1
explorer.utxo_set.summary_v1
explorer.utxo_set.commitment_v1
explorer.chain.reorg_history_v1
explorer.mempool.event_counts_v1
explorer.transaction.fees_v1
explorer.transaction.recent_v1
explorer.payment_disclosure.verify_v1
explorer.overview.snapshot_v1
```
<!-- capability-list:testing-runbook:end -->

Standalone `zinder-query` processes advertise the wallet rows above; the
`explorer.*` rows belong to a separately deployed `zinder-explorer`.

### `BlockSelector` smoke

```bash
# Hash → block id
grpcurl -plaintext -import-path crates/zinder-proto/proto \
  -proto crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto \
  -d '{"selector":{"hash":"<base64 hash>"}}' \
  127.0.0.1:9069 zinder.v1.wallet.WalletQuery/BlockIdBySelector

# Hash → block header
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
- gRPC `NOT_FOUND` (with a plain "not visible" message) for unknown transactions.

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

## External certification procedures

Use this table when a change makes a production-readiness or consumer-support
claim after the default and live-node gates are already green. Each item points
at the owning detail instead of repeating the full procedure here. Keep evidence
artifacts in `.tmp/` unless the result changes a durable claim, in which case
update the referenced architecture or reference page.

| Procedure | Friction | Run shape | Pass condition | Evidence to keep | Owning detail |
| --- | --- | --- | --- | --- | --- |
| Full wallet-serving testnet bulk catchup | Medium | Against a synced testnet Zebra, run `zinder-ingest --wallet-serving --target-height <safe height>` to seed the store, then `zinder-ingest --wallet-serving`, `zinder-query`, and `zinder-compat-lightwalletd` over the same store. | Store covers the wallet-serving floor, readers open from secondaries, `/readyz` reports ready or bounded syncing, `GetLightdInfo` reports a non-zero height and `taddrSupport: true`. | Ingest and reader logs, `/readyz` JSON, `GetLightdInfo` output, first and last ingested heights. | [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims), [Chain ingestion §Operation Shape](../architecture/chain-ingestion.md#operation-shape) |
| Lightwalletd-compatible wallet bootstrap and restore | Medium | Point a real lightwalletd-compatible wallet or SDK at the testnet compat endpoint. Exercise create-new-wallet first, then restore/import or resync against the same serving store. | Create-new-wallet and restored/resync wallets reach the chain tip when requested tree-state and subtree-root heights are at or above the store floor. Below-floor requests fail only as the documented strict `NOT_FOUND` unsupported-floor case; unknown tree-state or subtree-root gaps are blockers. | Wallet logs, endpoint config diff, wallet-visible height, store floor, requested tree-state/subtree-root heights, `GetAddressUtxosStream` sample. | [External integration: lightwalletd-compatible wallets](#external-integration-lightwalletd-compatible-wallets), [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims) |
| Send plus mempool end-to-end | Medium | With the unified `zinder-ingest` loop and the mempool surface running, open the mempool stream, then submit a self-send from a real wallet through `zinder-compat-lightwalletd`. | `SendTransaction` returns the expected success mapping. `GetMempoolStream` emits `RawTransaction`, stays open while the tx is pending, and closes on mining. `GetMempoolTx` returns the compact tx while pending and empties after mining. Wallet scan-back observes the mined tx, and writer-tip lag stays inside the wallet expiry window. | Wallet logs, compat logs, mempool stream excerpt, `GetMempoolTx` before/after output, txid, mined height, `/readyz` lag sample at submit time. | [Wallet data plane §External Wallet Compatibility Claims](../architecture/wallet-data-plane.md#external-wallet-compatibility-claims), [Wallet data plane §Mempool Snapshot](../architecture/wallet-data-plane.md#mempool-snapshot) |
| Real Zallet native-contract gate | Medium | Start a Zinder-native `zinder-query` integration target, then run `cargo nextest run --profile=ci-zallet-live --run-ignored=all` with `ZINDER_TEST_ZALLET*` pointing at a real Zallet binary and config. | The gate proves the Zallet config targets Zinder's native contract, rejects validator-direct config, and observes the required Zallet command output. | Zallet config path, command output, nextest result, `zinder-query` logs. | [T3: Real Zallet binary gate](#t3-real-zallet-binary-gate), [Service operations §Zallet with Zinder](../architecture/service-operations.md#zallet-with-zinder) |
| Mainnet bounded live sweep | Low when local mainnet is synced | Run the mainnet-only `ci-live` tests against the local operator-hosted Zebra before attempting longer sessions. Prefer targeted filters first, then the full live profile if the node is healthy. | Mainnet-only tests pass on `ZINDER_NETWORK=zcash-mainnet`; non-mainnet tests either pass or skip for the documented reason. No test mutates mainnet state. | Nextest output, Zebra tip height, tested block range, node auth source used. | [T3: Live mainnet](#t3-live-mainnet-operator-hosted-zebra) |
| Lightwalletd compatibility parity | Medium | Run the same lightwalletd-compatible probes or wallet flow against Zinder and a reference lightwalletd endpoint. Include `GetBlockRange`, transaction lookup, transparent history, UTXO, mempool, and send shapes that the support claim names. | Zinder returns compatible response and error shapes where the lightwalletd contract applies. Any intentional difference is documented against the native contract or compatibility adapter. Hot-path timings stay inside the published budgets. | Per-method request/response summaries, error codes, timing samples, endpoint versions. | [T3: Parity against a reference lightwalletd](#t3-parity-against-a-reference-lightwalletd), [Wallet data plane §Performance and Pagination](../architecture/wallet-data-plane.md#performance-and-pagination) |
| Public deployment shape | Medium for internal CA, high for public cert | Put TLS in front of `zinder-compat-lightwalletd` with Caddy, nginx, or traefik, forwarding h2c to the local compat process. Use a private CA only for internal pilots; use a publicly trusted cert for public claims. | A real lightwalletd-compatible wallet validation succeeds through TLS, `GetLightdInfo` works through the proxy, public traffic cannot reach plaintext gRPC, `IngestControl`, or ops endpoints, proxy rate limiting is present for public exposure, and `--print-config` plus logs redact secrets. | Proxy config, cert source, endpoint validation logs, `grpcurl` result through TLS, bind-address audit, redacted `--print-config` output. | [Integration surfaces §Lightwalletd Compatibility](../reference/integration-surfaces.md#lightwalletd-compatibility), [Service operations §Deployment guidance](../architecture/service-operations.md#deployment-guidance) |
| Observability and readiness warning semantics | Low | Run `scripts/observability-smoke.sh run` against regtest or a bounded public-network smoke. Inspect `/readyz`, Prometheus, and Grafana readiness panels after traffic generation. | Traffic-blocking readiness causes are zero. `cursor_at_risk` and `mempool_cursor_at_risk` are classified as warnings: `/readyz` still returns HTTP 200 with `"status": "ready"`, and metrics/dashboards expose the warning separately from load-balancer failure. | `.tmp/observability/reports/latest-readiness.json`, latest readiness Markdown, Prometheus query output, Grafana screenshot or panel JSON, service logs. | [Service operations §Health and Readiness](../architecture/service-operations.md#health-and-readiness), [Observability smoke](../../observability/README.md) |
| Backup, restore, and rolling restart | Low for smoke, medium for a long-running store | Run `scripts/observability-smoke.sh run` with the default backup restore enabled, or manually run `zinder-ingest backup --to <path>` and serve the checkpoint through restored `zinder-query` and `zinder-compat-lightwalletd` readers. Restart query/compat, then restart ingest and verify readers catch up. | The backup contains the canonical checkpoint and its bundled `derive` checkpoint. Restored `zinder-query` serves `WalletQuery/LatestBlock` at the checkpointed height, derive-backed wallet reads open from restored derive storage, secondaries reopen from the restored or live store, restart/shutdown is clean, and a second writer cannot silently take over an already-open primary store. | Backup path, restore `LatestBlock` output, derive-backed read sample, restart logs, `/readyz` after each restart, second-primary failure output. | [Service operations §Production Configuration](../architecture/service-operations.md#production-configuration), [Service operations §Recovery](../architecture/service-operations.md#recovery), [Observability smoke](../../observability/README.md) |
| Long soak | High | Run the unified `zinder-ingest` loop, `zinder-query`, `zinder-compat-lightwalletd`, and any derive or mempool surface needed by the claim for several hours or longer while scraping metrics. Exercise read streams during the run. | No unexplained readiness flaps, writer-tip and reader lag stay bounded, cursor-retention and RocksDB alerts stay quiet, memory growth is explainable, and restart/shutdown is clean. | Metrics scrape, readiness samples, process logs, memory/RSS samples, final restart result. | [Service operations §Validation Tiers](../architecture/service-operations.md#validation-tiers), [Wallet data plane §Performance and Pagination](../architecture/wallet-data-plane.md#performance-and-pagination) |
| Certification evidence manifest | Low | Before declaring a production-ready support claim, collect the commands, versions, network, node tip, consumer versions, and artifact paths from the rows above into one file under `.tmp/production-readiness/<run-id>/manifest.md` or `.json`. | A reviewer can replay the certification story without chat history: every claimed consumer, network, command, binary version, and evidence artifact has a path and pass/fail result. | Manifest file, command transcript, commit SHA, binary versions, Zebra version and tip height, consumer versions. | This runbook |

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
- [ ] Full wallet-serving bulk-catchup and tip-follow phase evidence exists for the target
      network, including store floor, tip height, `/readyz`, and reader secondary
      state.
- [ ] Zodl or Android SDK bootstrap, restore/resync, transparent output, send,
      and mempool evidence is green when claiming lightwalletd-compatible wallet
      support.
- [ ] Real Zallet binary gate is green when claiming Zallet support; the config
      must target Zinder's native contract and must reject validator-direct
      config.
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

- [ ] Default validation gate green (`cargo fmt`/`cargo clippy`/`cargo nextest run --profile=ci`/`cargo nextest run --profile=ci-perf`/`cargo doc`/`cargo deny`/`scripts/runbook-lint.sh`).
- [ ] If the change touched a consumer-facing wire contract or compatibility
      adapter: `cargo nextest run --profile=ci-parity`.
- [ ] Live regtest sweep green (`ci-live` profile against z3, with indexer
      gRPC env), including
      [Reorg sweep](#t3-reorg-sweep),
      [Network upgrade boundary crossing](#t3-network-upgrade-boundary-crossing),
      and [Writer fencing and crash recovery](#t3-writer-fencing-and-crash-recovery)
      where applicable. Hosted-network-only gates are expected to refuse regtest
      and should be rerun against testnet or mainnet.
- [ ] If the change touched storage commit, schema, or multi-process semantics:
      the [Writer fencing and crash recovery](#t3-writer-fencing-and-crash-recovery)
      `kill -9` and second-writer recipes have been replayed locally.
- [ ] If the change touched a streamed gRPC surface or the
      `zinder-compat-lightwalletd` adapter: the
      [Concurrent readers and stream cancellation](#concurrent-readers-and-stream-cancellation)
      tests have been replayed.
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
- [ ] If the change is mainnet-relevant: run the targeted mainnet live tests
      against an operator-hosted Zebra and record the evidence path.

## Runbook self-test

This runbook is operator-facing, dense, and changes often. Three classes
of drift are easy to introduce and expensive to discover during an
incident: a typo in a fenced shell command, a profile name that no
longer matches `.config/nextest.toml`, or a capability list that has
fallen behind the `CAPABILITIES` table. Each is caught by a piece of the
standard validation gate.

| Drift class | How it's caught | Where |
| ----------- | --------------- | ----- |
| Bash syntax in a fenced `bash` block | `scripts/runbook-lint.sh` runs every block through `bash -n`. Wired into the default validation gate above. | [`scripts/runbook-lint.sh`](../../scripts/runbook-lint.sh) |
| Profile-name drift between this runbook and `.config/nextest.toml` | `testing_runbook_profile_names_exist_in_nextest_toml` asserts every `--profile=<name>` quoted in this file resolves to a real `[profile.<name>]` section. | [`crates/zinder-proto/tests/integration/capability_docs.rs`](../../crates/zinder-proto/tests/integration/capability_docs.rs) |
| Default-filter drift between this runbook and `.config/nextest.toml` | `testing_runbook_default_filter_mirrors_nextest_toml` asserts the quoted expression matches the canonical one. | Same file. |
| Capability-list drift between this runbook and the `CAPABILITIES` table | `testing_runbook_capability_list_mirrors_zinder_capabilities` asserts the list under the `<!-- capability-list:testing-runbook -->` markers matches the wallet and explorer rows of the table. | Same file. |

All four checks ship with the default `cargo nextest run --profile=ci`
invocation; no separate runbook-self-test profile is needed. When you
add a new fenced command, profile, or capability, the corresponding
check fires on the next CI run if the runbook and code diverge.

To extend the self-test (for example, to gate a new profile name or a
new doc embedded constant), add an entry to
`RUNBOOK_REFERENCED_PROFILES` (or a peer constant) in
[`capability_docs.rs`](../../crates/zinder-proto/tests/integration/capability_docs.rs)
and let the existing assertion shape catch drift. The runbook-lint
script needs no changes for new blocks — it discovers them by markdown
fence syntax.

## Cross-references

- [ADR-0003: Epoch-bound storage access with RocksDB secondaries](../adrs/0003-canonical-storage-access-boundary.md) — secondary catchup and writer-status semantics that some live tests verify.
- [ADR-0006: IngestControl transport security](../adrs/0006-ingest-control-transport-security.md) — the bearer-token contract referenced in the external-integration conventions.
- [Service operations](../architecture/service-operations.md) — the operator-facing deployment story; the external-integration recipes here use its single-host, single-store conventions. For multi-host or multi-process deployments, follow that doc's recipes instead.
- [Wallet data plane](../architecture/wallet-data-plane.md) — the wire surface the external integration tests exercise.
- [Integration surfaces](../reference/integration-surfaces.md) — the consumer-facing support boundaries this runbook certifies.
- [Observability smoke](../../observability/README.md) — the local metrics, readiness, and backup-restore evidence harness.
- [`scripts/native-grpc-smoke.sh`](../../scripts/native-grpc-smoke.sh) — the scripted version of the manual `grpcurl` recipes below, callable from CI or a dev shell.
