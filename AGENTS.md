# Repository Guidelines

## Project Structure & Module Organization

This is a Rust 2024 workspace. Domain crates live under `crates/`: `zinder-core` for shared types, `zinder-store` for RocksDB-backed canonical storage, `zinder-source` for upstream node adapters and the shared `NodeTarget` config type, `zinder-proto` for generated protocol modules, `zinder-runtime` for the operational HTTP surface and config loader, and `zinder-testkit` for fixtures and the `live::` test-helper module. Service crates live under `services/`: `zinder-ingest` owns backfill and canonical writes, `zinder-query` owns wallet-facing read APIs, `zinder-compat-lightwalletd` translates the lightwalletd protocol. Integration tests sit beside each crate in `tests/{integration,live,perf}/` per [ADR-0006](docs/adrs/0006-test-tiers-and-live-config.md). Architecture, ADRs, and prototype tracking live under `docs/`; update them when a change alters boundaries, protocol bytes, storage semantics, or public vocabulary.

## Build, Test, and Development Commands

- `cargo fmt --all --check`: verify formatting.
- `cargo check --workspace --all-targets --all-features`: type-check all crates and test targets.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: run the strict lint gate.
- `cargo nextest run --profile=ci`: run T0/T1 (unit + integration) tests.
- `cargo nextest run --profile=ci-perf`: run T2 (perf) tests.
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps`: validate docs.
- `cargo deny check` and `cargo machete`: check dependency policy and unused dependencies.
- `cargo llvm-cov --workspace --all-features --no-report`: run coverage locally before risky storage/parser changes.
- T3 (live) tests: see [CLAUDE.md §Live Node Tests](CLAUDE.md). Set `ZINDER_TEST_LIVE=1` plus the unified `ZINDER_NETWORK` and `ZINDER_NODE__*` env vars, then `cargo nextest run --profile=ci-live --run-ignored=all`.

## Coding Style & Naming Conventions

Use workspace-managed Rust 2024 settings and `rustfmt.toml`. The lint baseline denies warnings, unsafe code, `unwrap`, `expect`, `panic`, `todo`, debug prints, and unreachable public API. Prefer domain names from `docs/architecture/public-interfaces.md`: `ChainEpoch`, `ChainEvent`, `NodeSource`, `WalletQueryApi`, and related terms. Avoid generic modules such as `utils`, `helpers`, or `manager`. (`tests/common/` is the one exception: per-crate shared test helpers, included via `mod common;` in `tests/acceptance.rs`.)

Test functions under `tests/live/` use plain `snake_case_describing_behavior` names. Do not include `live`, `regtest`, `testnet`, `mainnet`, or `z3` in the function name; the directory and runtime parameterization handle that.

## Testing Guidelines

Tests should exercise public boundaries and contract shapes: append, reorg, finality, cursor validation, storage recovery, and parser edge cases. Tier organization is by directory ([ADR-0006](docs/adrs/0006-test-tiers-and-live-config.md)): T0 unit (`#[cfg(test)] mod tests` in `src/`), T1 integration (`tests/integration/`), T2 perf (`tests/perf/`), T3 live (`tests/live/`). T3 tests are double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and `zinder_testkit::live::require_live()`; mainnet is rejected by default. Mutation testing is targeted at critical storage and parser functions through the CI workflow; expand that target set when changing those contracts.

## Commit & Pull Request Guidelines

Use concise imperative commits with an optional scope, for example `store: reject invalid reorg replacement`. Pull requests should summarize behavior changes, list validation commands run, link related docs or ADR updates, and call out any deferred production gap.

## Security & Configuration Tips

Never print secrets or raw authorization material. `--print-config` output must show explicit redaction markers. Production-like storage should be opened with an explicit network, and schema, network, and reorg-window mismatches should fail closed.
