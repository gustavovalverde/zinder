# Implementation Plan: PRD-0002 Self-Hosting and Integration Experience

| Field | Value |
|---|---|
| Status | In progress |
| PRD | [PRD-0002](../docs/prd-0002-self-hosting-and-integration-experience.md) |
| Created | 2026-05-12 |
| Owners | Zinder maintainers |

## Scope

[PRD-0002](../docs/prd-0002-self-hosting-and-integration-experience.md) closes the gap between Zinder being architecturally ready and Zinder being adoptable in an afternoon. Seventeen requirements split into 22 work items across five tiers. Tier A is foundational and must land first; Tiers B through E are parallel-safe once A is in place.

## Architectural decisions (committed)

The PRD records five decisions that ripple across the codebase. Each spawns an ADR alongside its code change. The decisions below are committed before any code lands; the ADRs are written in the same PR as the corresponding work item.

| Decision | ADR | Touches |
|---|---|---|
| Drop blanket env-var leaf rejection; `NodeAuth::Cookie` accepts `CookieSource::{File\|Inline}` | ADR-0018 | `crates/zinder-runtime/src/config.rs`, `crates/zinder-source/`, runbooks |
| Typed error reason vocabulary: `ErrorReason` proto enum attached via `tonic_types::ErrorInfo` (no custom `ErrorDetail` message) | ADR-0019 | `crates/zinder-proto/proto/zinder/v1/ops/error.proto` (new), `services/zinder-query/src/grpc/`, `crates/zinder-store/src/grpc_status.rs`, `crates/zinder-client/src/error.rs` |
| `ReadinessReport` promoted to a proto enum + message in `zinder-proto` | ADR-0020 | `crates/zinder-proto/proto/zinder/v1/ops/readiness.proto` (new), `crates/zinder-runtime/src/readiness.rs`, `crates/zinder-runtime/src/ops_endpoint.rs` |
| `chain_events` gains `address_filter` as an invalidation-hint filter; cursor stays opaque and `ChainEvent` variants are unchanged | ADR-0021 | `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`, `services/zinder-query/src/grpc/chain_events.rs` (new, extracted from `adapter.rs`) |
| Two-artifact release set: OpenAPI 3.0 YAML + `FileDescriptorSet`; OpenAPI is hosted with ReDoc | ADR-0022 | `buf.gen.yaml` (new), `.github/workflows/release-api-docs.yml` (new) |

### Decision context (key reasoning)

**`chain_events` semantics (Option C).** The chain-event canonical stream emits one envelope per epoch transition: `ChainCommitted` or `ChainReorged`. There are no per-address events. The `address_filter` field is therefore not a demultiplexer; it is an *invalidation hint*. The server narrows which envelopes a subscriber receives to those whose committed (or reverted) block touches at least one address in the filter; the client re-derives per-address state from the compact block it already fetches via `compact_block_at`. Cursor opacity, retention semantics, and the `ChainEvent` variant set all stay unchanged.

**`ErrorReason` shape.** `tonic-types 0.14.5` is already a workspace dependency, and `Status::with_error_details` is already used at every detail layer (`BadRequest`, `PreconditionFailure`, `ResourceInfo`). The new `ErrorReason` proto enum is attached via the existing `tonic_types::ErrorDetails::add_error_info(reason.as_str_name(), "zinder.dev", metadata)` builder. No new `ErrorDetail` proto message is introduced; the existing `ResourceInfo`/`PreconditionFailure`/`BadRequest` details are preserved alongside `ErrorInfo`.

**`ReadinessReport` (not `ReadinessStatus`).** The existing public Rust type is `ReadinessReport`. The proto message uses the same name to preserve vocabulary; the Rust enum becomes proto-generated and re-exported from `zinder-runtime`.

**Capability strings are the contract.** Agents pin to capability strings (`wallet.address.transparent_balance_v1`, etc.). The `_vN` suffix is part of the identity, never decoded. v1 has no deprecation surface (no published consumers exist yet); capability strings can be added, renamed, or removed between releases. A deprecation-window ADR lands when real consumers emerge.

**Unified `ops.ServerInfo`.** A shared `zinder.v1.ops.ServerInfo` proto message carries the minimum cross-service identity (`network`, `service_name`, `service_version`, `capabilities`). `WalletServerInfo` and `ExplorerServerInfo` embed it; `IngestControl` returns it directly through its own `ServerInfo` rpc. This prevents the per-service `*ServerCapabilities` proliferation that existed before.

## Tier A: Architectural foundations (must precede everything else)

Five work items. A3 and A4 share `crates/zinder-proto/build.rs`; A4 lands after A3 to avoid trivial merge friction. A1 and A2 are independent. A5 lands after A1+A4 because the extracted handler uses the new error vocabulary.

### A1. Lift env-var leaf restriction; evolve `NodeAuth::Cookie` (ADR-0018)

**Files to change:**

- `crates/zinder-runtime/src/config.rs`: delete the `SENSITIVE_ENV_LEAF_MARKERS` constant, the `SensitiveEnvironmentOverride` error variant, the `env_leaf_is_sensitive` function, and the rejection check in `zinder_environment_source`. Keep `--print-config` redaction (separate path).
- `crates/zinder-source/src/node_auth.rs`: refactor `NodeAuth::Cookie { path: PathBuf }` to `NodeAuth::Cookie(CookieSource)` with a new `pub enum CookieSource { File(PathBuf), Inline(SecretString) }`. When `Inline` is supplied, the runtime materializes the secret into a private `tempfile::NamedTempFile` (0600) at startup and returns a resolved `&Path` for the HTTP client. The tempfile's lifetime is bound to the runtime handle.
- `crates/zinder-source/src/node_target.rs`: extend `NodeAuthSection` to accept either `path` (file-shaped) or `cookie` (inline content). `resolve_node_auth` constructs `CookieSource::File` or `CookieSource::Inline` accordingly.
- `crates/zinder-source/src/zebra_json_rpc.rs`: update the `cookie_authorization_header` path to take a `&CookieSource` and dispatch to the file/inline branches.
- `crates/zinder-runtime/src/config.rs` `NodeAuthToml`: redaction emits `method = "cookie"` with `path = Some("[REDACTED]")` regardless of source; both `File` and `Inline` redact identically.
- `docs/architecture/public-interfaces.md`: update the env-var conventions section to list `ZINDER_NODE__AUTH__COOKIE` (inline), `ZINDER_NODE__AUTH__PATH` (file), `ZINDER_NODE__AUTH__USERNAME`, `ZINDER_NODE__AUTH__PASSWORD`. Cross-reference ADR-0018.

**New artifact:** `docs/adrs/0018-environment-variable-secret-policy.md`.

**Validation:**

- Delete the two `env_leaf_is_sensitive_*` unit tests and the three `sensitive_environment_override_is_rejected` integration tests across `services/zinder-{ingest,query,compat-lightwalletd}/tests/integration/cli.rs`.
- New unit tests in `crates/zinder-runtime/src/config.rs`: `ZINDER_NODE__AUTH__COOKIE=foo` loads, `--print-config` redacts every `cookie`/`password`/`path` leaf.
- New unit test in `crates/zinder-source/src/node_auth.rs`: `CookieSource::Inline` round-trips through tempfile materialization; the tempfile is mode 0600 (Unix only).
- Live regtest test: `LiveTestEnv` accepts both `ZINDER_NODE__AUTH__METHOD=cookie` with `__PATH=` and with `__COOKIE=`; `basic_auth_credentials` errors only when the test needs basic auth specifically.

### A2. `StartupPhase` enum and structured phase logging (REQ-5)

**Files to change:**

- `crates/zinder-core/src/observability/mod.rs` (new module): `pub enum StartupPhase` with eight variants (`LoadConfig`, `ValidateConfig`, `OpenStorage`, `CheckSchema`, `ConnectNode`, `RecoverState`, `StartApi`, `Ready`), `#[non_exhaustive]`, `#[serde(rename_all = "snake_case")]`, `Display` impl emits snake_case.
- `crates/zinder-core/src/observability/phase.rs`: `pub struct StartupPhaseHandle` with explicit `complete()` and `fail(&error)` methods; emits `phase_entry` on construction with `phase=<name>`; emits `phase_exit` with `outcome={ok|failed|aborted}` and `elapsed_ms=<n>`. `Drop` emits `aborted` only if neither `complete` nor `fail` was called. Returns an `impl StartupPhaseGuard` to keep call sites flat.
- `services/zinder-ingest/src/main.rs`, `services/zinder-query/src/bin/zinder-query/main.rs`, `services/zinder-compat-lightwalletd/src/bin/zinder-compat-lightwalletd/main.rs`, `services/zinder-derive/src/bin/zinder-derive/main.rs`: wrap each startup section in `let phase = StartupPhase::OpenStorage.start(); ...; phase.complete()?;`.
- `crates/zinder-testkit/src/observability.rs` (new): `LogCapture` test helper using `tracing-subscriber::Layer` to capture structured events for assertions.
- `docs/architecture/service-operations.md`: cross-link the `StartupPhase` enum from the Startup Phases section.

**Validation:**

- New per-binary integration test under `services/<svc>/tests/integration/startup_phases.rs` asserts every phase emits `phase_entry` and `phase_exit` with `phase=<name>` + `elapsed_ms` against a regtest fixture.

### A3. Promote `ReadinessCause` to a proto enum (ADR-0020, REQ-13)

**Files to change:**

- `crates/zinder-proto/proto/zinder/v1/ops/readiness.proto` (new file): `enum ReadinessCause` with 15 variants matching the current Rust enum + reserved slots; `message ReadinessReport { ReadinessCause cause = 1; optional uint32 current_height = 2; optional uint32 target_height = 3; optional ReadinessCauseDetail detail = 4; }` with a oneof `ReadinessCauseDetail` for the struct-variant payloads (`syncing.lag_blocks`, `reorg_window_exceeded.depth/configured`, etc.).
- `crates/zinder-proto/build.rs`: include `ops/readiness.proto` in the native compilation pass.
- `crates/zinder-runtime/src/readiness.rs`: delete the hand-written `ReadinessCause` enum and `ReadinessReport` struct; re-export the proto-generated types. Update `permits_traffic()` predicate and `ALL_METRIC_LABELS` to consume the new type.
- `crates/zinder-runtime/src/ops_endpoint.rs`: verify the JSON body of `/readyz` is byte-identical via a roundtrip test.

**New artifact:** `docs/adrs/0020-machine-readable-readiness-causes.md`.

**Validation:**

- Roundtrip test in `crates/zinder-proto/tests/integration/readiness_serde.rs` confirms every variant serializes to the same JSON shape it did pre-refactor.
- Existing `ops_endpoint.rs` tests pass unchanged.

### A4. `ErrorReason` proto enum + `tonic_types::ErrorInfo` wiring (ADR-0019, REQ-11)

**Files to change:**

- `crates/zinder-proto/proto/zinder/v1/ops/error.proto` (new): `enum ErrorReason` with one variant per current `QueryError` variant (plus the readiness causes that bubble as request errors): `NODE_UNAVAILABLE`, `NODE_CAPABILITY_MISSING`, `STORAGE_UNAVAILABLE`, `SCHEMA_MISMATCH`, `REORG_WINDOW_EXCEEDED`, `EVENT_CURSOR_EXPIRED`, `EVENT_CURSOR_INVALID`, `TRANSPARENT_UTXO_CURSOR_INVALID`, `TRANSPARENT_HISTORY_CURSOR_INVALID`, `ARTIFACT_UNAVAILABLE`, `ARTIFACT_CORRUPT`, `CHAIN_EPOCH_PIN_UNSUPPORTED`, `CHAIN_EPOCH_PIN_UNAVAILABLE`, `CHAIN_EPOCH_PIN_MISMATCH`, `COMPACT_BLOCK_RANGE_TOO_LARGE`, `COMPACT_BLOCK_PAYLOAD_MALFORMED`, `INVALID_BLOCK_RANGE`, `INVALID_ADDRESS`, `UNSUPPORTED_SHIELDED_PROTOCOL`, `UNSUPPORTED_CHAIN_EVENT`, `UNSUPPORTED_BLOCK_SELECTOR`, `UNSUPPORTED_TRANSACTION_STATUS`, `BROADCAST_DISABLED`, `BLOCK_NOT_IN_BEST_CHAIN`, `BLOCKING_TASK_FAILED`, `NETWORK_MISMATCH`. Reserved slots for additive growth.
- **No new `ErrorDetail` proto message.** The reason is attached via `tonic_types::ErrorDetails::new().add_error_info(reason.as_str_name(), "zinder.dev", metadata)` on top of any existing detail type. The `domain = "zinder.dev"` and `metadata` map come from `google.rpc.ErrorInfo` for free.
- `services/zinder-query/src/grpc/mod.rs` `status_from_query_error`: every match arm builds an `ErrorDetails` that combines the existing typed detail (`BadRequest`/`PreconditionFailure`/`ResourceInfo`) with `ErrorInfo`. Helper `error_details_with_reason(reason: ErrorReason, metadata: HashMap<String,String>) -> ErrorDetailsBuilder`.
- `crates/zinder-store/src/grpc_status.rs` `status_from_store_error`: same enrichment.
- `crates/zinder-client/src/error.rs`: `IndexerError::from_status` parses `ErrorDetails` from the status:
  - reads `ErrorInfo` for the `ErrorReason`,
  - preserves `ResourceInfo.resource_type/resource_name/owner` into the appropriate variants,
  - preserves `PreconditionFailure.type/subject/description`,
  - preserves `BadRequest.field_violations`,
  - falls back to status-code mapping when no details are present.
- `crates/zinder-client/src/error.rs`: add `pub fn reason(&self) -> Option<ErrorReason>`, `pub fn retry_policy(&self) -> RetryPolicy` with `RetryPolicy::{RetryWithBackoff, OperatorActionRequired, ClientError}`.
- `crates/zinder-proto/src/lib.rs`: re-export `ErrorReason` from `zinder_proto::v1::ops`.

**New artifact:** `docs/adrs/0019-typed-grpc-error-reason-vocabulary.md`.

**Validation:**

- Roundtrip test `crates/zinder-proto/tests/integration/error_reason_roundtrip.rs` confirms every `QueryError` and `StoreError` variant produces a `Status` whose `ErrorInfo.reason` parses to the expected `ErrorReason` and preserves auxiliary detail (resource family/key, precondition subject, field violations).
- `IndexerError::from_status` roundtrip test: `QueryError -> Status -> IndexerError` preserves the reason and the auxiliary detail.
- New CI gate test in `crates/zinder-proto/tests/integration/error_reason_coverage.rs` (sibling to `capability_string_uniqueness.rs`) asserts every `QueryError` variant has a corresponding `ErrorReason` value.

### A5. `chain_events` `address_filter` as invalidation hint (ADR-0021, REQ-14)

**Files to change:**

- `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`: add `repeated string address_filter = N;` to `ChainEventsRequest` with a doc comment "Empty list disables filtering. When non-empty, the server narrows envelopes to commits/reorgs that touch at least one of these transparent addresses; clients still re-derive per-address state from the committed compact block".
- Extract the `chain_events` handler from `services/zinder-query/src/grpc/adapter.rs` to a new module `services/zinder-query/src/grpc/chain_events.rs`. Pure mechanical refactor: same logic, different home. The new module hosts the address-filter logic.
- `services/zinder-query/src/grpc/chain_events.rs`: when `address_filter` is non-empty, for each envelope, look up the commit's address-touch set against the M4 transparent-address index; emit only when the intersection is non-empty. Other families (block tip, reorg) always pass through when `address_filter` is empty, and reorgs always pass through regardless because the client needs to know its derived state may be invalid.
- `crates/zinder-store/src/transparent_address_tx_index.rs`: expose `addresses_touched_in_block_range(network, start_height, end_height) -> Result<HashSet<TransparentAddressScriptHash>>` if not already present; used by the filter check.
- `crates/zinder-proto/src/capabilities.rs`: no new capability string; document on `wallet.events.chain_v1` that `address_filter` is supported.

**New artifact:** `docs/adrs/0021-canonical-confirmed-push-channel-for-transparent-activity.md`.

**Validation:**

- Integration test `services/zinder-query/tests/integration/chain_events_filter.rs`: (a) empty filter receives all events; (b) single-address filter receives only envelopes whose commit touches that address; (c) reorgs always pass through regardless of filter; (d) filter does not affect cursor opacity (cursor is byte-identical with and without filter).
- Update `docs/architecture/chain-events.md` to document the filter semantics under "Address invalidation hint".
- Update `docs/architecture/wallet-data-plane.md` to document the canonical "snapshot once, subscribe forever, re-derive on hint" pattern.

## Tier B: Deployment artifacts (operator surface)

### B1. Per-service Dockerfiles (REQ-1)

**Files to create:**

- `services/zinder-ingest/Dockerfile`
- `services/zinder-query/Dockerfile`
- `services/zinder-compat-lightwalletd/Dockerfile`
- `services/zinder-derive/Dockerfile`

**Shape:** Multi-stage. Stage 1 builds in `rust:1.86-bookworm` (matching `rust-toolchain.toml`), with workspace mounted and a Cargo build cache. Stage 2 ships the single binary on `debian:bookworm-slim` with `ca-certificates`, non-root `USER zinder:zinder` (uid/gid 1000), workdir `/var/lib/zinder`, volume mount points `/var/lib/zinder/store` and `/var/lib/zinder/secondary`, config mount point `/etc/zinder/config.toml`. `EXPOSE` only the service's port. `HEALTHCHECK` runs `curl -fsS http://localhost:${ZINDER_OPS_ADDR##*:}/readyz || exit 1` against the existing `/readyz` endpoint (NOT `/livez`; that endpoint does not exist).

**Validation:**

- CI workflow `.github/workflows/build-images.yml` builds each image on every PR (cached layers).
- Image size budget: < 200MB compressed per service.

### B2. Single-container `s6-overlay` image (REQ-2)

**Files to create:**

- `deploy/single-container/Dockerfile`: multi-stage `COPY --from=zinder-ingest`, `COPY --from=zinder-query`, adds `s6-overlay` (pinned version), copies service definitions, sets `ENTRYPOINT ["/init"]`. Includes `zinder-ingest` and `zinder-query` only; `zinder-derive` is optional and added by topology if requested.
- `deploy/single-container/services/zinder-ingest/run`: `exec zinder-ingest tip-follow --config /etc/zinder/config.toml`.
- `deploy/single-container/services/zinder-ingest/finish`: propagates exit codes; triggers container exit on terminal failure.
- `deploy/single-container/services/zinder-query/run`: `exec zinder-query --config /etc/zinder/config.toml`, waits for ingest readiness via `s6-svwait`.
- `deploy/single-container/services/zinder-query/finish`: same propagation rule.
- `deploy/single-container/config.example.toml`: reference TOML with the single-container topology baked in (`storage.path = /var/lib/zinder/store`, distinct `secondary_path` per process).
- `deploy/single-container/README.md`: includes a topology diagram showing operator-supplied reverse proxy (Caddy/Nginx/Cloudflare) in front of the container's exposed gRPC port. PRD non-goals say Zinder does not own TLS termination; the diagram makes the boundary visible.

**Validation:**

- `deploy/single-container/Dockerfile` builds in CI.
- Smoke test (see B5) boots the image against a regtest fixture.

### B3. `docker-compose` and `systemd` units (REQ-2, REQ-4)

**Files to create:**

- `deploy/docker-compose.yml`: composes the per-service images from B1 against a named Docker volume; documents env-var overrides operators need.
- `deploy/systemd/zinder.service`: wraps `docker compose up` with `Restart=on-failure`, `RestartSec=30`, `StartLimitBurst=5`, `StartLimitIntervalSec=600`.

**Validation:**

- Lint via `docker compose config -q deploy/docker-compose.yml` in CI.
- Lint the systemd unit via `systemd-analyze verify` in CI on an Ubuntu runner.

### B4. CI release-image pipeline (REQ-1, REQ-12)

**Files to create:**

- `.github/workflows/release-images.yml`: on tagged releases, builds each per-service image and the single-container image, pushes to `ghcr.io/zcashfoundation/zinder-<binary>`.

**Validation:**

- Successful image push on a dry-run release (`workflow_dispatch` for first verification).

### B5. Single-container smoke test (REQ-2 + REQ-6)

**Files to create:**

- `services/zinder-query/tests/integration/single_container_smoke.rs`: shells out to `docker run` (no `testcontainers-rs` dependency) to boot `deploy/single-container/Dockerfile` against a regtest fixture, then runs the `crates/zinder-client/examples/observe_transparent_address.rs` binary against it and asserts it reports the expected UTXO line within the regtest mining window.

**Validation:**

- Runs under a new `--profile=ci-deploy` (separate from `ci-live`) to isolate "deployment artifact broken" from "live integration broken".

## Tier C: Documentation and boundary clarity

### C1. `indexer-wallet-boundary.md` (REQ-16)

**File to create:** `docs/architecture/indexer-wallet-boundary.md`.

**Contents:**

- One-paragraph statement of what Zinder is NOT (no key custody, no shielded scanning, no per-consumer wallet state).
- Three pointers: server-side wallet -> `zcash_client_backend` + `zcash_client_sqlite`; wallet process -> Zallet; mobile -> Zashi/Zodl.
- One Mermaid diagram showing the indexer/wallet split with named arrows.
- Cross-references to ADR-0008, PRD-0001, `wallet-data-plane.md`.

**Files to update:** `docs/README.md`: move the boundary page to first link after the PRDs.

### C2. `server-side-wallet-pattern.md` (REQ-7)

**File to create:** `docs/reference/server-side-wallet-pattern.md`.

**Contents:**

- Named components: `zcash_client_backend`, `zcash_client_sqlite`, `zcash_primitives`, `zcash_proofs`, Zinder.
- Pinned tested version range (read from `Cargo.toml` at the time of landing).
- Boundary contract: keys + decryption in consumer; compact blocks + tree state + broadcast in Zinder.
- Worked code skeleton (~60-80 lines) showing the pairing.
- Cross-references to ADR-0008, `wallet-data-plane.md`, `indexer-wallet-boundary.md`.

### C3. `known-consumers.md` (REQ-17)

**File to create:** `docs/reference/known-consumers.md`.

**Contents:** Registry of named consumers with one-line integration shape each:

- Zashi/Zodl mobile (via `zinder-compat-lightwalletd`)
- Zallet (via `zinder-client::RemoteChainIndex` or `LocalChainIndex`)
- Zcash testnet faucet (via `zinder-client::RemoteChainIndex` + `chain_events`)
- Public-`lightwalletd`-client population (Android SDK), reserved section

### C4. `error-vocabulary.md` (REQ-11)

**File to create:** `docs/reference/error-vocabulary.md`.

**Contents:** Table for every `ErrorReason` value with identifier, description, mapped gRPC status code, retry policy, example metadata fields.

**Depends on:** A4.

### C5. PaaS and VM runbooks (REQ-4)

**Files to create:**

- `docs/runbooks/deploying-on-railway.md`: Railway prerequisites, env-var examples (using `ZINDER_NODE__AUTH__COOKIE` per A1), volume setup, expected readiness cause sequence, rollback procedure, reverse-proxy / TLS placement diagram.
- `docs/runbooks/deploying-on-a-vm.md`: VM with Docker Compose + systemd; references `deploy/docker-compose.yml` and `deploy/systemd/zinder.service` from B3.

**Depends on:** A1, B1-B3, A2.

### C6. Generated env-var contract table (REQ-9)

**Files to create/change:**

- `crates/zinder-config-doc/` (new crate, single binary `zinder-config-doc`): uses `schemars` to introspect the config types and emit a Markdown table mapping every TOML path to its `ZINDER_<SECTION>__<FIELD>` env-var.
- Document explicitly: the `ZINDER_NODE__AUTH__*` family parsed by `NodeTarget::from_environment()` for live tests is *also* in the schema (same struct); no separate test-only env-var table is needed.
- `docs/architecture/public-interfaces.md`: append a "Generated env-var table" section with `<!-- generated:start -->` / `<!-- generated:end -->` markers.
- `.github/workflows/check-docs.yml` (new): runs `zinder-config-doc` and fails on drift.

**Depends on:** A1.

## Tier D: Agent and integrator surface

### D1. Unified `ops.ServerInfo` across services (REQ-10)

**Files to change:**

- `crates/zinder-proto/proto/zinder/v1/ops/server_info.proto` (new): `message ServerInfo { string network = 1; string service_name = 2; string service_version = 3; repeated string capabilities = 4; }`. No deprecation surface, no fingerprint, no MCP endpoint, no protocol-schema version. YAGNI: these land alongside their first real consumer.
- `crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`: replace `ServerCapabilities` with `message WalletServerInfo { ops.ServerInfo common = 1; string lightwalletd_protocol_commit = 2; uint32 schema_version = 3; uint32 reorg_window_blocks = 4; uint64 chain_event_retention_seconds = 5; uint64 mempool_mined_retention_seconds = 6; uint64 mempool_invalidated_retention_seconds = 7; NodeCapabilitiesDescriptor node = 8; }`. Breaking change (per CLAUDE.md, no users).
- `crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto`: replace `ExplorerServerCapabilities` with `message ExplorerServerInfo { ops.ServerInfo common = 1; string vendor = 2; }`.
- `crates/zinder-proto/proto/zinder/v1/ingest/ingest.proto`: returns `ops.ServerInfo` directly through its `ServerInfo` rpc.
- `services/zinder-query/src/grpc/native.rs` `build_server_capabilities_message`: rename to `build_wallet_server_info`; populates the embedded `ops::ServerInfo` from operator settings.
- `docs/architecture/public-interfaces.md`: document the unified ops descriptor shape.

**Depends on:** A4.

### D2. Release artifacts: OpenAPI + descriptor set (ADR-0022, REQ-12)

**Files to create/change:**

- `buf.gen.yaml` (new): configures `buf build` (binary descriptor set) and `protoc-gen-openapiv2` (OpenAPI 3.0 YAML).
- **Drop `protoc-gen-doc` HTML.** Two-artifact release: OpenAPI YAML (hosted with ReDoc by tooling that wants a human-readable doc) + binary descriptor set (`grpcurl`-style tooling).
- `.github/workflows/release-api-docs.yml`: on tagged release, runs `buf generate`, attaches the two artifacts to the GitHub Release.
- `docs/architecture/public-interfaces.md`: document the two-artifact contract.
- ADR-0022 explicitly states: agents integrate via the OpenAPI artifact; OpenAPI-to-MCP bridges (e.g., `openapi-mcp`) materialize an MCP server from the artifact for free. A bespoke `zinder-mcp` adapter is a future optimization, not a missing primitive. v1 carries no deprecation surface (no published consumers exist); a deprecation-window ADR lands when real consumers emerge.

**Depends on:** A4, D1.

## Tier E: Developer Experience polish

### E1. Worked examples (REQ-6, REQ-14, REQ-15)

**Files to create:**

- `crates/zinder-client/examples/observe_transparent_address.rs`: `RemoteChainIndex::connect(endpoint)` -> `transparent_address_utxos_stream(address)` (snapshot) -> `chain_events_for_family { address_filter: vec![address] }` (subscribe). Prints one line per new UTXO. Handles reconnect with exponential backoff.
- `crates/zinder-client/examples/observe_mempool_to_confirmed.rs`: mempool snapshot + `MempoolEvents` subscribe + dedup at the boundary.
- `crates/zinder-client/Cargo.toml`: adds `[[example]]` entries.
- `crates/zinder-client/tests/integration/example_smoke.rs`: boots the example against a regtest fixture, asserts expected output.

**Depends on:** A5.

### E2. Rustdoc `# Examples` on every `ChainIndex` method (REQ-8)

**Files to change:**

- `crates/zinder-client/src/chain_index.rs`: every public method on the `ChainIndex` trait gains a rustdoc block with request shape, response shape, error categories (referencing `ErrorReason`), and a minimal `# Examples` block.
- `crates/zinder-client/tests/integration/rustdoc_coverage.rs` (new): parses generated rustdoc and asserts every public method on `ChainIndex` has a `# Examples` block. Fails the build if any are missing.

**Depends on:** A4.

## Sequencing constraints

```
A1 ────────┐
A2 ────────┤
A3 ──┐     │
     ▼     │
A4 ──┘ ────┤
A5 ────────┘  (A5 depends on A1 and A4)
        │
        ├──> B1 ──> B2, B3 ──> B4, B5
        ├──> C1, C2, C3        (parallel; docs)
        ├──> C4 (after A4)
        ├──> C5 (after B1-B3, A2)
        ├──> C6 (after A1)
        ├──> D1 (after A4)
        │         │
        │         └──> D2 (after D1)
        ├──> E1 (after A5)
        └──> E2 (after A4)
```

## ADRs to land

| ADR | Subject | Lands with |
|---|---|---|
| 0018 | Environment variable secret policy | A1 |
| 0019 | Typed gRPC error reason vocabulary (`ErrorReason` via `tonic_types::ErrorInfo`) | A4 |
| 0020 | Machine-readable readiness causes (proto-defined `ReadinessReport`) | A3 |
| 0021 | Address invalidation hint on `chain_events` (Option C) | A5 |
| 0022 | Release artifact set (OpenAPI + descriptor set) and capability deprecation window | D2 |

## Default validation gate

Every PR must pass:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo deny check
cargo machete
git diff --check
```

Tier A items that touch proto definitions additionally run `cargo nextest run --profile=ci-perf`. Tier B5 runs under `--profile=ci-deploy` (new profile; isolated from `ci-live`).

## Out-of-band follow-ups

These surfaced during research and are tracked here, but are out of scope for PRD-0002:

- Consolidate `.github/workflows/parity-compat.yml` to use the B1 Zinder Docker images instead of `cargo build --release` + ad-hoc PID management.
- A `zinder-mcp` adapter as a performance/curation optimization on top of the published OpenAPI. ADR-0022 states the v1 path: OpenAPI-to-MCP bridges materialize the MCP server for free.
- Migration to `/livez` + `/readyz` + `/startupz` per Kubernetes 2026 conventions. PRD Open Question 1; revisit if a PaaS target surfaces a real problem with the current `/healthz` + `/readyz` split.
- Cross-host RocksDB secondary access. ADR-0007 §Out of Scope.
- Cookie rotation while running: today Zinder reads the cookie file once on connection construction. Operators must restart on rotation. A reload-on-401 path is a follow-up.

## Status tracking

This plan is living. Each Tier item is a TaskCreate entry in the project task list; status is mirrored there. When all items are complete the plan moves to `Status: Completed` and the PRD's status comment updates accordingly.
