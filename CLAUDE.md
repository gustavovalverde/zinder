# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

Zinder is a service-oriented Zcash indexer. Architecture, vocabulary, and boundary rules live under `docs/`; the docs are the source of truth and updating them is part of the change, not a follow-up.

## Where to read first

- [docs/README.md](docs/README.md): full doc index with lifecycle rules.
- [docs/architecture/service-boundaries.md](docs/architecture/service-boundaries.md): who owns what across the four runtimes (`zinder-ingest`, `zinder-query`, `zinder-compat-lightwalletd`, `zinder-derive`).
- [docs/architecture/public-interfaces.md](docs/architecture/public-interfaces.md): the vocabulary spine (types, errors, config fields, capability strings).
- [docs/architecture/chain-ingestion.md](docs/architecture/chain-ingestion.md): the canonical commit pipeline.
- [docs/adrs/0007-multi-process-storage-access.md](docs/adrs/0007-multi-process-storage-access.md): the writer/reader topology, secondary catchup, and writer-status RPC.

Before changing public types, storage layouts, protocol bytes, or service boundaries, read the relevant doc above and amend it in the same change.

## Default Validation Gate

Run before considering any change complete:

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

`cargo nextest run` is the canonical workspace runner ([ADR-0006](docs/adrs/0006-test-tiers-and-live-config.md)). Profiles in `.config/nextest.toml`: `default`, `ci`, `ci-perf`, `ci-live`. `cargo test --workspace --all-features` works as a libtest fallback (and is what `cargo mutants` shells), but is not the documented gate.

Heavier probes for trust-sensitive storage/parser changes (also run by scheduled CI):

```bash
cargo llvm-cov --workspace --all-features --no-report
cargo mutants --workspace --all-features \
  --file crates/zinder-store/src/chain_store.rs \
  --file crates/zinder-store/src/chain_store/validation.rs \
  --file crates/zinder-source/src/source_block.rs \
  --re 'chain_event_history|finalized_only_commit_without_artifacts|validate_reorg_window_change|from_raw_block_bytes'
```

Single-test execution under nextest: `cargo nextest run -p <crate> -E 'test(<test_name>)'`. Tier filter: `-E 'test(/^integration::cli::/)'`. Integration tests live in each crate's `tests/{integration,live,perf}/` submodules; the per-crate binary is `tests/acceptance.rs`.

## Live Node Tests (T3)

Network-touching tests live under `tests/live/` ([ADR-0006](docs/adrs/0006-test-tiers-and-live-config.md)). They are double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and a `zinder_testkit::live::require_live()` runtime check, and read the same env-var schema as production binaries.

Regtest:

```bash
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-regtest \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:39232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=zebra \
  ZINDER_NODE__AUTH__PASSWORD=zebra \
  cargo nextest run --profile=ci-live --run-ignored=all
```

Testnet (Zebra cookie auth):

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

`require_live()` rejects mainnet by default. Tests that target mainnet must opt in via `require_live_for(&[Network::ZcashMainnet])` or `require_live_mainnet()`. T3 mainnet runs are `workflow_dispatch`-only until [ADR-0012 §Open mainnet infrastructure questions](docs/adrs/0006-test-tiers-and-live-config.md#open-mainnet-infrastructure-questions-parked) resolve.

`ZINDER_TEST_*` is stripped from `zinder_runtime::zinder_environment_source` so test-only knobs never leak into production config. There is no `ZINDER_Z3_*` namespace; tests use the production schema directly.

Test functions under `tests/live/` use plain `snake_case_describing_behavior` names. Do not include `live`, `regtest`, `testnet`, `mainnet`, or `z3` in the function name; the directory and runtime parameterization handle that.

## Coding Constraints

The workspace `Cargo.toml` denies (not warns):

- `unsafe_code`, `warnings`, `missing_docs`, `unreachable_pub`, `unnameable_types`
- Clippy: `all`, `cargo`, `pedantic` (nursery is warn)
- `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stderr`, `print_stdout`, `wildcard_enum_match_arm`, `allow_attributes_without_reason`
- Rustdoc: `broken_intra_doc_links`, `bare_urls`

`clippy.toml` extends this:

- `unwrap`, `expect`, `dbg`, `print` are also banned in tests.
- `too-many-lines-threshold = 80`, `too-many-arguments-threshold = 5`.
- Disallowed names: `data`, `info`, `item`, `result`, `stuff`, `thing`, `tmp`, `value`.

Practical effects:

- Always use `?` and typed errors; never `unwrap()`/`expect()`, even in tests.
- Every public item needs a doc comment. `#[allow(..., reason = "...")]` requires the `reason =` field.
- Use `eprintln!`/`println!` only in CLI `main`/`run` paths, behind an explicit `#[allow(clippy::print_stderr, clippy::print_stdout, reason = "...")]` block (see `services/zinder-ingest/src/main.rs`).
- Library crates use per-boundary `thiserror` enums. Binaries may use `anyhow` only at `main.rs`. Public domain crates must not expose `tonic::Status`, `rocksdb::*`, `jsonrpsee::core::ClientError`, `reqwest::Error`, or transport-specific errors.
- Use `#[non_exhaustive]` for public enums expected to gain variants.
- Supported build targets must have a pointer width of at least 32 bits; `zinder-core` enforces this so infallible `u32` to `usize` conversions stay honest.
- "Service" means a deployable runtime, not a Rust trait or struct name. Do not create types named `*Service`, `*Manager`, `*Handler`, `*Helper`, or modules named `utils`, `common`, `helpers`. Full vocabulary in [public-interfaces.md](docs/architecture/public-interfaces.md).

## Protobuf Generation

`crates/zinder-proto/build.rs` compiles three `.proto` files via `tonic-prost-build` at build time. Generated code lives in `OUT_DIR` and is included via `include!`. The lightwalletd schemas under `proto/compat/lightwalletd/` are vendored from `zcash/lightwallet-protocol` (commit pinned in `crates/zinder-proto/proto/compat/lightwalletd/COMMIT`, surfaced as `zinder_proto::compat::lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT`). Do not edit them; the `vendored-proto` CI job diffs them against upstream.

Native protocol changes go in `crates/zinder-proto/proto/zinder/v1/wallet.proto`. After editing, add a generated message round-trip test in `crates/zinder-proto/tests/`.

## Local Storage Conventions

`.gitignore` excludes `target/`, `.tmp/`, `*.rocksdb/`, `*.zinder-store/`, `*.profraw`, and `lcov.info`. Use `.tmp/` for ad-hoc TOML configs, scratch stores, and fixture captures. Per-network store paths must be distinct (`zcash-mainnet`, `zcash-testnet`, `zcash-regtest`); a store opened for one `Network` rejects commits from another.

## Doc Maintenance Rule

When a change alters a service boundary, public API, storage byte layout, protocol surface, or vocabulary, update the owning document in `docs/` in the same change. ADRs are written in present tense and describe the current decision; clarifications and rewordings are edited in place. Substantive design changes get a new ADR with the next contiguous number.

## Workspace Patches

`Cargo.toml` patches `core2` to a git source because `equihash 0.2.2` (transitive through `zebra-chain 6.0.2`) depends on a yanked crates.io version. The patch is gated by the `cargo-deny` `allow-git` list. Remove it only when Zebra publishes a resolution path.
