# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

Zinder is a service-oriented Zcash indexer. Architecture, vocabulary, and boundary rules live under `docs/`; the docs are the source of truth and updating them is part of the change, not a follow-up.

## Where to read first

- [docs/README.md](docs/README.md): full doc index with lifecycle rules.
- [docs/architecture/service-boundaries.md](docs/architecture/service-boundaries.md): who owns what across the four release runtimes (`zinder-ingest`, `zinder-projector`, `zinder-query`, `zinder-compat-lightwalletd`) plus the optional explorer runtime.
- [docs/architecture/public-interfaces.md](docs/architecture/public-interfaces.md): the vocabulary spine (types, errors, config fields, capability strings).
- [docs/architecture/chain-ingestion.md](docs/architecture/chain-ingestion.md): the canonical commit pipeline.
- [docs/adrs/0003-canonical-storage-access-boundary.md](docs/adrs/0003-canonical-storage-access-boundary.md): the epoch-bound storage API, writer/reader topology, secondary catchup, and writer-status RPC.

Before changing public types, storage layouts, protocol bytes, or service boundaries, read the relevant doc above and amend it in the same change.

## Default Validation Gate

Run before considering any change complete:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
cargo nextest run --profile=ci-parity
cargo nextest run --profile=ci-perf
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo deny check
cargo machete
git diff --check
```

`cargo nextest run` is the canonical workspace runner; the test tiers and live configuration are documented in the [Testing Runbook](docs/runbooks/testing.md). Profiles in `.config/nextest.toml`: `default`, `ci`, `ci-postgres`, `ci-perf`, `ci-live`, `ci-parity`. `cargo test --workspace --all-features` works as a libtest fallback (and is what `cargo mutants` shells), but is not the documented gate.

Heavier probes for trust-sensitive storage/parser changes (also run by scheduled CI):

```bash
cargo llvm-cov --workspace --all-features --no-report
cargo mutants --workspace --all-features \
  --file crates/zinder-store/src/chain_store.rs \
  --file crates/zinder-store/src/chain_store/validation.rs \
  --file crates/zinder-source/src/source_block.rs \
  --re 'chain_event_history|settled_tip_only_commit_without_artifacts|validate_reorg_window_change|from_raw_block_bytes'
```

Single-test execution under nextest: `cargo nextest run -p <crate> -E 'test(<test_name>)'`. Tier filter: `-E 'test(/^integration::cli::/)'`. Integration tests live in each crate's `tests/{integration,live,perf}/` submodules; the per-crate binary is `tests/acceptance.rs`.

## Live Node Tests (T3)

Network-touching tests live under `tests/live/`. They are double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and a `zinder_testkit::live::require_live()` runtime check, and read the same env-var schema as production binaries.

Regtest:

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

Without `ZINDER_NODE__INDEXER_GRPC_ADDR`, the `zebra_indexer_mempool_*` tests skip.

Testnet (Z3 stack with cookie auth):

```bash
# Pull the cookie out of the Z3 shared cookie volume; works regardless of
# Z3's container name and matches the pattern in z3/docs/integrations/.
cookie=$(docker run --rm -v z3-testnet-cookie:/auth:ro alpine cat /auth/.cookie)
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-testnet \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:18232 \
  ZINDER_NODE__INDEXER_GRPC_ADDR=http://127.0.0.1:18155 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=${cookie%%:*} \
  ZINDER_NODE__AUTH__PASSWORD=${cookie#*:} \
  cargo nextest run --profile=ci-live --run-ignored=all
```

The parity-against-lightwalletd suite needs two more endpoints
(a running Zinder compat shim and a reference `lightwalletd-go`,
both pointed at the same Zebra):

```bash
ZINDER_TEST_PARITY_ZINDER_ADDR=http://127.0.0.1:9087 \
ZINDER_TEST_PARITY_LIGHTWALLETD_ADDR=http://127.0.0.1:9088 \
  cargo nextest run --profile=ci-live --run-ignored=all \
    -E 'test(/^live::parity_against_lightwalletd::/)'
```

`require_live()` rejects mainnet by default. Tests that target mainnet must opt in via `require_live_for(&[Network::ZcashMainnet])` or `require_live_mainnet()`. Local mainnet runs are supported against an operator-hosted Zebra.

Mainnet (Z3 stack):

```bash
cookie=$(docker run --rm -v z3-mainnet-cookie:/auth:ro alpine cat /auth/.cookie)
ZINDER_TEST_LIVE=1 \
  ZINDER_NETWORK=zcash-mainnet \
  ZINDER_NODE__JSON_RPC_ADDR=http://127.0.0.1:8232 \
  ZINDER_NODE__AUTH__METHOD=basic \
  ZINDER_NODE__AUTH__USERNAME=${cookie%%:*} \
  ZINDER_NODE__AUTH__PASSWORD=${cookie#*:} \
  cargo nextest run --profile=ci-live --run-ignored=all
```

Tests that did not opt in via `require_live_for(&[Network::ZcashMainnet])` still skip on mainnet; opt-in is per-test, not per-invocation.

`ZINDER_TEST_*` is stripped from `zinder_runtime::zinder_environment_source` so test-only knobs never leak into production config. There is no `ZINDER_Z3_*` namespace; tests use the production schema directly.

Test functions under `tests/live/` use plain `snake_case_describing_behavior` names. Do not include `live`, `regtest`, `testnet`, `mainnet`, or `z3` in the function name; the directory and runtime parameterization handle that.

## Coding Constraints

The workspace MSRV is Rust 1.95. Keep `Cargo.toml`,
`rust-toolchain.toml`, `clippy.toml`, CI toolchain actions, and Docker
`RUST_VERSION` args in sync whenever the toolchain moves.

The workspace `Cargo.toml` denies (not warns):

- `unsafe_code`, `warnings`, `missing_docs`, `unreachable_pub`, `unnameable_types`
- Clippy: `all`, `cargo`, `pedantic` (nursery is warn)
- `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stderr`, `print_stdout`, `wildcard_enum_match_arm`, `allow_attributes_without_reason`, `disallowed_types`
- Rustdoc: `broken_intra_doc_links`, `bare_urls`

`clippy.toml` extends this:

- `unwrap`, `expect`, `dbg`, `print` are also banned in tests.
- `too-many-lines-threshold = 80`, `too-many-arguments-threshold = 5`.
- `std::sync::{Mutex,RwLock}` are banned. Use `parking_lot` for synchronous shared state and `tokio::sync` only when a guard must cross an async boundary.
- Disallowed names: `data`, `info`, `item`, `result`, `stuff`, `thing`, `tmp`, `value`.

Practical effects:

- Always use `?` and typed errors; never `unwrap()`/`expect()`, even in tests.
- Every public item needs a doc comment. `#[allow(..., reason = "...")]` requires the `reason =` field.
- Use `eprintln!`/`println!` only in CLI `main`/`run` paths, behind an explicit `#[allow(clippy::print_stderr, clippy::print_stdout, reason = "...")]` block (see `services/zinder-ingest/src/main.rs`).
- Library crates use per-boundary `thiserror` enums. Binaries may use `anyhow` only at `main.rs`. Public domain crates must not expose `tonic::Status`, `rocksdb::*`, `jsonrpsee::core::ClientError`, `reqwest::Error`, or transport-specific errors.
- Use `#[non_exhaustive]` for public enums expected to gain variants.
- Supported build targets must have a pointer width of at least 32 bits; `zinder-core` enforces this so infallible `u32` to `usize` conversions stay honest.
- "Service" means a deployable runtime, not a Rust trait or struct name. Do not create types named `*Service`, `*Manager`, `*Handler`, `*Helper`, or modules named `utils`, `common`, `helpers`. Full vocabulary in [public-interfaces.md](docs/architecture/public-interfaces.md).
- Wire-boundary identifier translations live in `crates/zinder-core/src/wire/` (pure-domain helpers) and `crates/zinder-proto/src/wire/` (proto-enum mappings). Adding a new wire field starts there: locate or add a function for the concept, then call it from the boundary. Inline `transaction_id.as_bytes()`, inline `format!("{:08x}", ...)`, hardcoded capability literals, and duplicate `Network` to wire-string tables outside those modules are forbidden patterns (see [Public interfaces §Wire Conventions](docs/architecture/public-interfaces.md#wire-conventions); enforced by `wire_invariants.rs` and `capability_string_uniqueness.rs` on every `cargo nextest run --profile=ci`).

## Protobuf Generation

`crates/zinder-proto/build.rs` compiles three `.proto` files via `tonic-prost-build` at build time. Generated code lives in `OUT_DIR` and is included via `include!`. The lightwalletd schemas under `proto/compat/lightwalletd/` are vendored from `zcash/lightwallet-protocol` (commit pinned in `crates/zinder-proto/proto/compat/lightwalletd/COMMIT`, surfaced as `zinder_proto::compat::lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT`). Do not edit them; the `vendored-proto` CI job diffs them against upstream.

Native protocol changes go in `crates/zinder-proto/proto/zinder/v1/wallet.proto`. After editing, add a generated message round-trip test in `crates/zinder-proto/tests/`.

## Local Storage Conventions

`.gitignore` excludes `target/`, `.tmp/`, `*.rocksdb/`, `*.zinder-store/`, `*.profraw`, and `lcov.info`. Use `.tmp/` for ad-hoc TOML configs, scratch stores, and fixture captures. Per-network store paths must be distinct (`zcash-mainnet`, `zcash-testnet`, `zcash-regtest`); a store opened for one `Network` rejects commits from another.

## Doc Maintenance Rule

When a change alters a service boundary, public API, storage byte layout, protocol surface, or vocabulary, update the owning document in `docs/` in the same change. ADRs are written in present tense and describe the current decision; clarifications and rewordings are edited in place. Substantive design changes get a new ADR with the next contiguous number.

## Ironwood Dependency Pins

`zebra-chain` is pinned to `11.0.0`, the first release that decodes Ironwood (NU6.3) version-6 transactions. Zinder uses the matching stable librustzcash stack (`zcash_address 0.13.0`, `zcash_protocol 0.10.0`, `zcash_primitives 0.29.0`, `zcash_transparent 0.9.0`, and `orchard 0.15.0`) so no duplicate librustzcash type exists at the `zebra-chain` boundary. There are no git dependencies; `cargo-deny` denies unknown git and registry sources.
