# Product Requirements: v1 Self-Hosting and Integration Experience

| Field | Value |
|-------|-------|
| Status | Accepted |
| Created | 2026-05-12 |
| Product | Zinder |
| Audience | Zinder maintainers, infrastructure operators, server-side wallet developers, application integrators, LLM agents integrating against the public surface |
| Related | [PRD-0001](prd-0001-zinder-indexer.md), [RFC-0001](rfcs/0001-service-oriented-indexer-architecture.md), [ADR-0007](adrs/0007-multi-process-storage-access.md), [ADR-0008](adrs/0008-consumer-neutral-wallet-data-plane.md), [ADR-0009](adrs/0009-ingest-control-transport-security.md), [Service operations](architecture/service-operations.md), [Wallet data plane](architecture/wallet-data-plane.md). Spawns ADR-0018 through ADR-0022 alongside the corresponding code changes. |

## Problem Statement

[PRD-0001](prd-0001-zinder-indexer.md) commits to a self-hosted single-operator deployment target for v1 and a `WalletQuery` surface that intentionally never custodies keys. The architectural commitments are sound. In practice, however, a consumer that tries to take Zinder from "I cloned the repo" to "I am running it in production and a real application is consuming it" still encounters experience gaps. Those gaps fall into three buckets:

- **Operator experience**: how a real operator brings Zinder up on a modern hosting platform without writing infrastructure code from scratch.
- **Developer experience**: how a real Rust integrator builds an application on top of Zinder, with a clear understanding of which capabilities live in the indexer versus in the wallet they need to provide.
- **Agent experience**: how an LLM agent or external developer discovers, consumes, and stays compatible with Zinder's public surface without reading the source tree.

These are product gaps in the experience layer, not architectural gaps in the indexer. PRD-0001 establishes what Zinder is. This PRD establishes what running and integrating Zinder should feel like in v1.

The trigger for this PRD is a concrete consumer integration. A Zcash testnet faucet adopting Zinder as its chain-read plane surfaced every gap below from first contact: no reference Dockerfile, no documented single-host topology recipe, no worked example for receiving-address observation, no single page that says "this is what Zinder does not do and where to go for those needs." Each gap is solvable, none requires rethinking the architecture, and every one of them blocks adoption today.

## Solution

A focused set of consumer-facing and operator-facing deliverables that close the gap between Zinder being architecturally ready and Zinder being adoptable in an afternoon by an operator or a developer who has not read the architecture docs. The deliverables are:

- A reference deployment recipe for single-host PaaS targets that matches Zinder's v1 deployment scope.
- A worked Rust integration example that demonstrates the end-to-end consumer pattern in one file.
- A "what Zinder is and what it is not" boundary page that prevents new integrators from wasting time hunting for wallet primitives that intentionally live elsewhere.
- A canonical recipe for "building a server-side wallet on Zinder," because Zinder's wallet boundary makes this the recommended pattern but the recipe is not currently written down.
- Tightened operator-probe and capability-discovery surfaces so orchestrators and agents can drive Zinder without out-of-band documentation.

None of this changes the architectural seams. All of it makes the seams legible.

## Goals

- A fresh Zinder deployment on a single-host PaaS (Railway, Fly.io, Render, a single VM) is a documented, 30-minute job, not a multi-day infrastructure exercise.
- A Rust consumer integrates chain reads, broadcasts, and transparent-address subscriptions in a single afternoon, working from one worked example and one cookbook page.
- The indexer-versus-wallet boundary is legible at first contact. An integrator who lands on the docs and asks "how do I list my account's shielded unspent notes?" finds, within one page, why Zinder does not answer that question and where to go instead.
- Every long-running Zinder process exposes the same `/healthz`, `/readyz`, `/metrics`, and capability surface, so orchestrators and agents can drive any deployment with one playbook.
- Releases carry machine-readable capability descriptors and a documented compatibility window, so agents and integrators pin to features rather than versions.

## Non-Goals

Reaffirmed from PRD-0001 and ADR-0008; restated here to prevent each new integrator from rediscovering them:

- **Multi-tenant hosting, TLS termination, authentication, rate limiting, quota accounting** remain out of v1 scope. Operators put these in front of Zinder via standard reverse proxies; Zinder itself does not own them.
- **Server-side wallet scanning, viewing-key custody, spending-key custody, per-account decrypted note state** remain out of scope. ADR-0008 names this. PRD-0001 reaffirms it. This PRD reinforces it explicitly because the absence of these primitives is the single most common point of integration confusion (see [Boundary clarity](#boundary-clarity-developer-experience) below).
- **Cross-host RocksDB secondary access**. ADR-0007 places this out of v1; this PRD does not propose to revisit it. Single-host topology is the v1 recommendation and the recipes below document that explicitly.

## User Stories

### As an operator deploying to a single-host PaaS

1. As an operator, I want a repository-hosted reference `Dockerfile` so that I do not have to invent my own build for a binary I did not write.
2. As an operator on a PaaS that does not share filesystems across services (Railway, Fly.io, Render), I want a documented single-container topology that runs `zinder-ingest` and `zinder-query` against a shared local volume, so that I can deploy Zinder without provisioning a Kubernetes cluster.
3. As an operator whose PaaS injects secrets only as environment variables, I want Zinder to accept the Zebra cookie content directly through `ZINDER_NODE__AUTH__COOKIE` (and basic auth through `ZINDER_NODE__AUTH__USERNAME` + `__PASSWORD`), so that I do not write a secret-materialisation entrypoint shim for every deployment.
4. As an operator using a PaaS log viewer rather than `ssh`, I want each startup phase (`load_config`, `validate_config`, `open_storage`, `check_schema`, `connect_node`, `recover_state`, `start_api`, `ready`) to emit a single structured log line on entry and exit, so that a stuck startup is visible from the deploy log.
5. As an operator, I want one logical service ("zinder") that I can name, deploy, and watch in my PaaS, even if internally it is two processes. Splitting into multiple deployments should be an opt-in scaling choice, not the entry path.

### As a Rust integrator building a server-side application

6. As an integrator, I want a worked, runnable example that shows the entire pattern of "subscribe to a receiving address; print one line per new UTXO" so that I can copy-paste my way to a working integration in an hour.
7. As an integrator, I want a single cookbook page titled "Building a server-side wallet on Zinder" that names the canonical components (`zcash_client_backend` for shielded scanning, `zcash_client_sqlite` for wallet state, Zinder for chain reads, Zinder for transaction broadcast) so that I do not have to derive the architecture from ADRs.
8. As an integrator, I want the `zinder-client` crate's `ChainIndex` trait to come with rustdoc examples on every public method, so that I integrate against contract documentation, not source-code reading.
9. As an integrator, I want stable env-var conventions (every TOML field has a documented `ZINDER_<SECTION>__<FIELD>` env-var path with `__` as the nesting separator) so that my deployment configuration survives Zinder upgrades.

### As an LLM agent or external developer integrating against the public surface

10. As an agent integrator, I want every Zinder gRPC service to expose a `ServerInfo` capability descriptor that I can fetch on connect, so that I can detect supported features at runtime instead of pinning to a Zinder version. (Restated from PRD-0001 #24; called out here because it directly serves agent integrations.)
11. As an agent integrator, I want a stable typed error vocabulary across all gRPC methods, so that I can map remote errors onto local retry, gate, and alert decisions without parsing message strings.
12. As an agent integrator, I want generated protobuf documentation published with each release, so that I can introspect the contract from a URL rather than fetching the repo.
13. As an agent integrator, I want the readiness cause vocabulary in [Service operations](architecture/service-operations.md) to be machine-readable and frozen at v1, so that an automated probe can act on `not_ready` causes without code changes per Zinder release.

### As an end user of a Zinder-backed consumer

14. As the end user of a wallet, faucet, or explorer that uses Zinder, I want low-latency feedback when my transparent transaction lands at a watched address (zero-conf via mempool; one-conf via chain event), so that the consumer's UI updates within seconds, not the consumer's polling interval.
15. As the end user, I want consumers to be able to surface "your funds are confirmed" without the consumer maintaining its own backup notification pipeline. Zinder's chain events should be the canonical low-latency notification channel for transparent activity.

### As a consumer maintainer planning for future shielded support

16. As a consumer maintainer who intends to add shielded receive support later, I want Zinder's documentation to explicitly point me at the librustzcash component set (`zcash_client_backend`, `zcash_client_sqlite`, `zcash_primitives`) and the boundary contract (Zinder serves compact blocks + tree state; my wallet owns keys + decryption), so that I plan that work against the right interface from day one.

## Capability Requirements

### Reference deployment artifacts (operator experience)

**REQ-1: Repository-hosted reference Dockerfiles.** A `Dockerfile` lives next to each service binary (`services/zinder-ingest/Dockerfile`, `services/zinder-query/Dockerfile`, `services/zinder-compat-lightwalletd/Dockerfile`, `services/zinder-derive/Dockerfile`). Each builds its single binary with a multi-stage `rust:bookworm` → `debian:bookworm-slim` build, runs as a non-root user, and exercises only one process. CI builds every image; tagged releases push them to a container registry.

**REQ-2: Reference single-container topology.** A documented "all-on-one-host" topology composes `zinder-ingest` and `zinder-query` in one container with a single local volume mount, supervised by `s6-overlay`. Each process owns its own `secondary_path` directory beneath the shared volume; the entrypoint binds the public surface (query gRPC) externally and binds the internal surfaces (ingest control) on loopback. This topology is the v1 recommended deployment shape for single-operator self-hosting. Kubernetes operators who want sidecar-style separation use the per-service Dockerfiles from REQ-1 instead.

**REQ-3: PaaS-native secret intake.** Zinder accepts upstream-node credentials directly through the `ZINDER_NODE__AUTH__*` environment variable family on PaaS targets that inject secrets as environment variables. Operators set `ZINDER_NODE__AUTH__METHOD=cookie` and either `ZINDER_NODE__AUTH__COOKIE=<content>` (the cookie material, materialised into a private tempfile by Zinder at startup) or `ZINDER_NODE__AUTH__COOKIE__PATH=/path/to/cookie` (file-shaped secrets). Basic auth is `ZINDER_NODE__AUTH__USERNAME` + `ZINDER_NODE__AUTH__PASSWORD`. The previous blanket `SENSITIVE_ENV_LEAF_MARKERS` rejection in `crates/zinder-runtime/src/config.rs` is dropped; secrets continue to redact in `--print-config` output and structured logs. Per-surface rules that remain load-bearing (the `IngestControl` bearer token reads only from a file, per [ADR-0009](adrs/0009-ingest-control-transport-security.md)) survive the policy refactor.

**REQ-4: Deployment runbooks for at least one PaaS and one VM target.** `docs/runbooks/` gains a `deploying-on-railway.md` (or equivalent PaaS) and a `deploying-on-a-vm.md`. Each lists prereqs, exact env vars, expected first-run timings, the readiness cause sequence an operator should expect, and the rollback procedure. Other PaaS targets are documented by community contribution against the same template.

**REQ-5: Startup phase logging.** Each phase in [Service operations §Startup Phases](architecture/service-operations.md#startup-phases) emits a single structured log line on entry and exit, with elapsed milliseconds. A stuck startup is visible from the PaaS log viewer without `ssh`.

### Consumer integrator experience

**REQ-6: One-file worked example for transparent-address observation.** `services/zinder-query/examples/observe_transparent_address.rs` (or `crates/zinder-client/examples/`) is a runnable, single-file program that connects to a Zinder instance, subscribes to a transparent address, prints one line per new UTXO, and handles reconnect. CI builds and lints it as part of the regular workspace pipeline.

**REQ-7: Server-side wallet pattern reference.** `docs/reference/server-side-wallet-pattern.md` documents the canonical pattern for building a server-side Zcash wallet on top of Zinder. It names the components by ecosystem identity (`zcash_client_backend`, `zcash_client_sqlite`, `zcash_primitives`, `zcash_proofs`), lays out the boundary (keys and wallet state in the consumer; compact blocks, tree state, broadcast in Zinder), pins a tested version range, and includes a minimal worked code skeleton. The location matches the precedent set by [`serving-zebra-and-zallet.md`](reference/serving-zebra-and-zallet.md): a consumer-pattern reference that crosses Zinder and an external system.

**REQ-8: `ChainIndex` trait rustdoc with examples.** Every public method on the `ChainIndex` trait (`crates/zinder-client/src/chain_index.rs`) carries a rustdoc block that names the request shape, the response shape, the error categories, and a minimal usage example. `cargo doc --no-deps` produces the canonical integration reference.

**REQ-9: Env-var contract.** Every TOML field documented in [Public interfaces](architecture/public-interfaces.md) has a paired `ZINDER_<SECTION>__<FIELD>` env-var path (single-underscore prefix, double-underscore separator, matching Zebra's layered-config convention). The mapping is mechanically generated from config types into a table appended to [Public interfaces](architecture/public-interfaces.md) and is a stable contract within a major version. A CI doc-gen step asserts no drift between the published table and the live config schema.

### Agent and integrator experience

**REQ-10: `ServerInfo` on every gRPC service.** Restated from PRD-0001 #24, raised to a hard requirement: every public gRPC service (`WalletQuery`, `ExplorerQuery`, `IngestControl`) exposes a `ServerInfo` rpc returning a typed descriptor that embeds the cross-service `zinder.v1.ops.ServerInfo` shape. The common shape carries `network`, `service_name`, `service_version`, and the active `capabilities` strings; per-service descriptors layer service-specific fields on top (e.g. `WalletServerInfo` adds retention windows, reorg-window depth, and the upstream-node capability snapshot). Clients pin to capability strings, not to versions.

**REQ-11: Typed error vocabulary.** Every gRPC method returns a two-layer typed error following the `google.rpc.ErrorInfo` pattern: an outer gRPC `Status` code (from the 16 standard codes, chosen for retry semantics) and an inner `ErrorDetail` payload carrying a stable `ZinderReason` enum value, a `domain = "zinder.dev"`, and structured `metadata`. The `ZinderReason` enum is defined in `zinder-proto` and is the single source of truth for both the gRPC boundary (`QueryError`) and the Rust client boundary (`IndexerError`); both error enums map to and from the proto reason. Each reason carries a documented retry policy (`retry_with_backoff`, `operator_action_required`, `client_error`). The proto enum is `reserved`-friendly so new reasons are additive within a major version.

**REQ-12: Release artifact set.** Each tagged release publishes three machine-readable artifacts at a stable URL: HTML proto documentation (`protoc-gen-doc`), an OpenAPI 3.0 YAML transcoded from the proto surface (`protoc-gen-openapiv2`), and a binary `FileDescriptorSet` (`buf build -o image.bin`). The OpenAPI artifact is the agent-discovery substrate (consumed by MCP servers, language-agnostic SDK generators, and OpenAPI-to-MCP bridges); the descriptor set is the canonical schema export for `grpcurl`-style tooling.

**REQ-13: Machine-readable readiness cause vocabulary.** The list in [Service operations §Required readiness causes](architecture/service-operations.md#health-and-readiness) is promoted from a hand-written Rust enum in `zinder-runtime` to a proto-defined enum in `zinder-proto`. The generated Rust type replaces the current handwritten enum; the JSON wire shape of `/readyz` stays byte-identical. The proto enum is frozen at v1 with `reserved` slots; new causes added post-v1 are additive only, existing causes' semantics are stable.

### Real-time consumer needs (end-user experience)

**REQ-14: Push-style transparent-address change notifications.** `chain_events` is the canonical low-latency push channel for confirmed transparent-address activity. The request grows an optional `watched_addresses` filter parameter so per-address consumers (faucets, payment receivers) do not pay the bandwidth cost of unrelated events. `transparent_address_utxos_stream` remains the snapshot/pagination channel; the canonical consumer pattern is "snapshot once, subscribe forever". The semantics are documented: emitted on confirm, with explicit guarantees about ordering, deduplication after reorg, and behaviour during catch-up.

**REQ-15: Documented mempool visibility for zero-conf flows.** `transparent_mempool_outputs_by_address` is documented with a worked example showing the zero-conf-to-one-conf transition: mempool subscribe for unconfirmed activity, chain-event subscribe for confirmed activity, deduplication rule at the boundary.

### Boundary clarity (developer experience)

**REQ-16: "What Zinder is not" boundary page.** A dedicated docs page at `docs/architecture/indexer-wallet-boundary.md` lists, in plain language:

- What Zinder does not do (hold keys, scan shielded outputs per account, maintain per-consumer wallet state).
- Why (consumer-neutral wallet data plane; ADR-0008).
- Where to go for each capability Zinder does not provide:
  - For server-side wallet state and shielded scanning: `zcash_client_backend` and `zcash_client_sqlite` from librustzcash.
  - For a separate wallet process exposing its own RPC: Zallet.
  - For mobile wallet integration: the Zashi/Zodl SDK.
- A single architectural diagram showing the indexer / wallet split with named arrows.

This page is the first link any new integrator follows. Its absence is currently the single biggest first-contact tax.

**REQ-17: Consumer reference list.** A maintained `docs/reference/known-consumers.md` lists known consumers of Zinder (Zashi/Zodl mobile, Zallet, this PRD's faucet integration, others as they emerge) with one line on the integration shape. Establishes prior art and gives new integrators someone to read for context.

## Acceptance Criteria

Per requirement above, the deliverable is:

| REQ | Deliverable |
|-----|-------------|
| REQ-1 | Per-service `services/<binary>/Dockerfile` lands for each of `zinder-ingest`, `zinder-query`, `zinder-compat-lightwalletd`, `zinder-derive`; CI builds every image; tagged releases push to a container registry. |
| REQ-2 | `deploy/single-container/Dockerfile` (multi-binary, `s6-overlay`-supervised) plus `deploy/single-container/services/zinder-ingest/run`, `deploy/single-container/services/zinder-query/run`, and `deploy/single-container/services/zinder-query/finish`. An integration test boots the container against regtest and runs the REQ-6 worked example against it. |
| REQ-3 | `NodeAuth::Cookie` accepts `{ Path(PathBuf) \| Content(SecretString) }`; the `SENSITIVE_ENV_LEAF_MARKERS` constant is removed from `crates/zinder-runtime/src/config.rs`; `--print-config` redaction and structured-log redaction stay in place; ADR-0018 records the policy. |
| REQ-4 | `docs/runbooks/deploying-on-railway.md` and `docs/runbooks/deploying-on-a-vm.md`. Other PaaS targets accepted as community contributions against the same template. |
| REQ-5 | `StartupPhase` enum (8 variants, `#[non_exhaustive]`) lands in `zinder-core`. Every service binary emits one structured log line on entry and one on exit per phase, carrying `phase=<name>` and `elapsed_ms=<n>`. CI asserts both lines per phase for every binary. |
| REQ-6 | `crates/zinder-client/examples/observe_transparent_address.rs` builds in CI; an integration test runs it against a regtest fixture. |
| REQ-7 | `docs/reference/server-side-wallet-pattern.md` lands and is linked from `docs/README.md`. |
| REQ-8 | `cargo doc --no-deps -p zinder-client` produces a navigable reference with at least one `# Examples` block per public method on `ChainIndex`. CI gates on `# Examples` presence per public method. |
| REQ-9 | Generated env-var table appended to `docs/architecture/public-interfaces.md`, produced by a workspace `zinder-config-docgen` binary; CI runs it and fails on drift. |
| REQ-10 | Every public gRPC service exposes a `ServerInfo` rpc that returns a descriptor embedding `zinder.v1.ops.ServerInfo`. The common shape carries `network`, `service_name`, `service_version`, and `capabilities`; per-service descriptors (`WalletServerInfo`, `ExplorerServerInfo`) layer service-specific fields. CI asserts the response shape and that the capability list is non-empty. |
| REQ-11 | `ZinderReason` enum and `ErrorDetail` proto message land in `crates/zinder-proto`; `QueryError` and `IndexerError` both map to and from them; `docs/reference/error-vocabulary.md` tables each reason with its gRPC status code and retry policy. ADR-0019 records the decision. |
| REQ-12 | Release pipeline produces HTML proto docs (`protoc-gen-doc`), OpenAPI 3.0 YAML (`protoc-gen-openapiv2`), and a binary `FileDescriptorSet` (`buf build`). All three publish at a stable URL. ADR-0022 records the artifact contract. |
| REQ-13 | Readiness cause enum lands as a typed proto message in `zinder-proto`; `service-operations.md` cross-links it; the JSON shape of `/readyz` is byte-identical to today. ADR-0020 records the protocol-boundary promotion. |
| REQ-14 | `ChainEvents` request grows an optional `watched_addresses: repeated string` filter parameter; server-side filtering at the transparent-output family level; the REQ-6 worked example uses the filter. ADR-0021 records the canonical push-channel decision. |
| REQ-15 | Mempool-to-confirmed transition is the subject of a second worked example (`crates/zinder-client/examples/observe_mempool_to_confirmed.rs`) demonstrating `transparent_mempool_outputs_by_address` snapshot + `MempoolEvents` subscription + deduplication at the boundary. |
| REQ-16 | `docs/architecture/indexer-wallet-boundary.md` lands and is the first link in `docs/README.md` after the PRD. |
| REQ-17 | `docs/reference/known-consumers.md` lands with at least three named consumers. |

## Architectural Decisions

The implementation turns on five architectural decisions. Each spawns an ADR alongside the corresponding code change. The PRD records the choice; the ADR carries the design.

1. **Environment variable policy for secret intake (ADR-0018, planned).** Drop the blanket `SENSITIVE_ENV_LEAF_MARKERS` rejection in `crates/zinder-runtime/src/config.rs`. Operators can set `ZINDER_NODE__AUTH__COOKIE`, `__PASSWORD`, and the rest directly through environment variables on PaaS targets. `NodeAuth::Cookie` evolves to `{ Path(PathBuf) | Content(SecretString) }` so the runtime materialises cookie content into a private tempfile when supplied inline. Per-surface rules that remain load-bearing (the ingest-control bearer token reads only from a file per [ADR-0009](adrs/0009-ingest-control-transport-security.md)) survive the policy refactor. Secrets continue to redact in `--print-config` output and structured logs. The architecture moves from blanket-restriction to explicit-per-surface-policy, which is the cleaner shape.

2. **Typed gRPC error reason vocabulary (ADR-0019, planned).** Adopt the `google.rpc.ErrorInfo` two-layer pattern: outer gRPC `Status` code plus inner `ErrorDetail` carrying a stable `ZinderReason` enum, `domain = "zinder.dev"`, and structured `metadata`. The reason enum lives in `zinder-proto` and is the single source of truth for both `QueryError` (gRPC boundary) and `IndexerError` (Rust client boundary). Each reason carries a documented retry policy.

3. **Machine-readable readiness causes (ADR-0020, planned).** Promote `ReadinessCause` from the hand-written Rust enum in `zinder-runtime` to a proto-defined enum in `zinder-proto`. The Rust type is generated; the JSON wire shape of `/readyz` is byte-identical. Operators and orchestrators can act on `cause` programmatically.

4. **Canonical push channel for confirmed transparent activity (ADR-0021, planned).** `chain_events` is THE confirmed-activity stream. The request grows an optional `watched_addresses` filter so per-address consumers do not receive every transparent-output event. `transparent_address_utxos_stream` remains the snapshot/pagination channel. The canonical pattern: snapshot once, subscribe forever.

5. **Release artifact set (ADR-0022, planned).** Each tagged release publishes two machine-readable artifacts at a stable URL: OpenAPI 3.0 YAML and a binary `FileDescriptorSet`. The OpenAPI artifact is the agent-discovery substrate; the descriptor set is the canonical schema export. Capability strings are exact-match identifiers (the `_vN` suffix is part of the identity, never decoded). v1 carries no deprecation surface; Zinder has no published external consumers yet, so adding, renaming, or removing capability strings between releases is permitted. A deprecation-window ADR lands when real consumers emerge.

## Open Questions

1. **Health endpoint convention.** Kubernetes 2026 best practice is `/livez` + `/readyz` + (optional) `/startupz`. Zinder's existing `/healthz` + `/readyz` is ecosystem-compatible. Default: keep the current split. Revisit if a PaaS target surfaces a real problem with the `/healthz` semantics.

2. **`zinder-deploy` as a separate repository.** REQ-1 and REQ-2 keep reference deployment artifacts in the main Zinder repository for cohesion. A future `zinder-deploy` repo with Terraform modules, Helm charts, and additional PaaS templates is an option for v2 if the deployment surface grows. v1 keeps everything in one place.

3. **OpenAPI publication URL.** REQ-12 publishes three artifacts per release. The hosting URL (e.g., `zinder.zfnd.org/api/v1/`) is a release-engineering decision; GitHub Releases artifacts are an acceptable v1 fallback if a dedicated documentation domain is not yet provisioned.

## Stakeholders

- **Zinder maintainers**: own the architectural decisions (1-5), sequence the requirements against the v1 release plan, and resolve the remaining open questions.
- **Consumer integrators**: this PRD's first source is a Zcash testnet faucet integration. Additional input invited from any team building a server-side wallet, exchange integration, custody backend, or block explorer on top of Zinder.
- **Mobile wallet teams (Zashi, Zodl)**: review REQ-7 and REQ-16 to ensure the server-side guidance does not contradict the mobile-side patterns.
- **Zallet maintainers**: review REQ-7 and REQ-16 to ensure the "Zallet as a separate wallet process" pointer accurately reflects current Zallet's intended consumption model.
- **Release engineering**: own REQ-1 (CI image build) and REQ-12 (release artifact set: proto docs, OpenAPI, descriptor set).

## Document lifecycle

This PRD is `Accepted`. The five architectural decisions above are committed; ADR-0018 through ADR-0022 land alongside the corresponding code changes and carry the design detail. Substantive scope changes spawn a new PRD with an incremented number. Implementation sequencing lives in [`plans/0002-self-hosting-and-integration-experience.md`](../plans/0002-self-hosting-and-integration-experience.md); this PRD remains the product reference.
