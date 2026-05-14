# ADR-0020: Machine-Readable Readiness Causes

## Context

The readiness-cause vocabulary defined in [Service operations §Required readiness causes](../architecture/service-operations.md#health-and-readiness) needs to be machine-readable so an automated probe can act on `not_ready` causes without code changes per Zinder release. The Rust enum in `zinder-runtime` is the authoring surface; gRPC consumers and language-agnostic SDK generators need a parseable contract that matches it.

The `/readyz` HTTP JSON endpoint carries the cause and its operator-actionable detail (e.g. `{"cause": {"reorg_window_exceeded": {"depth": 12, "configured": 10}}}`). Operator probes consume that wire shape. Breaking it would force every Zinder deployment's monitoring stack to update in lockstep with the proto landing; keeping the JSON intact lets the proto contract land as a purely additive change.

proto3 enums are scalar; they cannot carry a per-variant payload like the Rust `Syncing { lag_blocks: Option<u64> }` variant. Two routes follow: (1) keep the Rust enum hand-written and add the proto enum as a *categorical mirror* (proto is the documented source of truth for the cause vocabulary; the Rust struct variants stay), (2) collapse the payload into a sibling `oneof` so the proto message fully replaces the Rust types. Route 2 changes the JSON wire shape because proto-generated serialization names variants in `SCREAMING_SNAKE_CASE` with the payload as a sibling object; route 1 preserves the current JSON.

## Decision

**Route 1.** The proto file `proto/zinder/v1/ops/readiness.proto` defines:

- `enum ReadinessCause` with one variant per current Rust variant, scalar-coded (`READINESS_CAUSE_STARTING = 1`, etc.). Reserved slots `16..=31` keep additive growth cheap.
- `message ReadinessReport { ReadinessCause cause = 1; optional uint32 current_height = 2; optional uint32 target_height = 3; optional ReadinessCauseDetail detail = 4; }`.
- `message ReadinessCauseDetail` with a `oneof payload` carrying one detail message per parametric cause (`SyncingDetail`, `NodeCapabilityMissingDetail`, `ReorgWindowExceededDetail`, `ReplicaLaggingDetail`, `CursorAtRiskDetail`, `MempoolCursorAtRiskDetail`, `MempoolHydrationLaggingDetail`).

The hand-written `zinder_runtime::ReadinessCause` enum stays as the JSON wire shape for `/readyz`. Conversion is one-way: `impl From<&ReadinessCause> for ops_proto::ReadinessCause` for the categorical code, `impl From<&ReadinessCause> for Option<ops_proto::ReadinessCauseDetail>` for the payload, and `impl From<&ReadinessReport> for ops_proto::ReadinessReport` for the full snapshot. Future gRPC ops surfaces consume the proto message via `Into`; the existing `/readyz` JSON shape is byte-identical.

A roundtrip test (`proto_cause_maps_every_variant`) asserts that every Rust variant maps to a proto code; combined with the existing `metric_label_is_listed_in_all_metric_labels` test, adding a new Rust variant without extending the proto enum (or the metric label table) fails CI.

## Consequences

**Cause vocabulary is a published proto contract.** The descriptor set artifact published per release (per [ADR-0022](0022-release-artifact-set.md)) carries the enum. Orchestrators, SDK generators, and MCP bridges can parse it without reading the Rust source.

**`/readyz` JSON shape is byte-identical.** Existing operator probes continue to work unchanged. The breaking-change budget is preserved for changes that earn it.

**The Rust enum remains the authoring surface.** Adding a new cause means: (1) add the proto scalar code, (2) add the Rust variant, (3) extend the `From` impls and metric label table. The roundtrip test enforces correspondence. The proto file is the *documented* source of truth; the Rust enum is the *authored* source of truth. Within a single PR they cannot diverge.

**A future gRPC ops surface is unblocked.** Consumers wanting structured readiness over gRPC (rather than scraping `/readyz` JSON) can be added at any time using `ops_proto::ReadinessReport` without further refactoring. This is the path REQ-13 leaves open without committing to today.

**The `oneof ReadinessCauseDetail.payload` is a soft contract.** The proto guarantees only that the active variant *can* match the cause; it does not enforce it structurally. Producers (Zinder) honor the correspondence in the `From` impls. Consumers should treat a mismatch as a server bug rather than a protocol expansion path.

**`zinder-runtime` depends on `zinder-proto`** for proto-generated types only. `zinder-source` already pulls in `zinder-proto`, so the transitive graph stays unchanged.

## Alternatives Considered

**Route 2 (collapse the Rust enum into the proto message).** Rejected because it changes the `/readyz` JSON wire shape. The breaking change is not worth its cost when route 1 unlocks the same machine-readable contract for gRPC consumers.

**`tonic_types::ErrorDetails` shape.** Considered for cases where the readiness cause bubbles up as a request error (e.g. via the future `ServerInfo` rpc). Deferred: error vocabulary is [ADR-0019](0019-typed-grpc-error-reason-vocabulary.md)'s job; the readiness cause is a state, not a transient error.

**Hand-written JSON Schema for `/readyz`.** Rejected. JSON Schema is a separate documentation artifact that would drift from the Rust source; the proto contract is the canonical machine-readable form and OpenAPI generation via `protoc-gen-openapiv2` (per [ADR-0022](0022-release-artifact-set.md)) produces a JSON Schema for free.

## References

- [Service operations §Health and Readiness](../architecture/service-operations.md#health-and-readiness)
- [`crates/zinder-proto/proto/zinder/v1/ops/readiness.proto`](../../crates/zinder-proto/proto/zinder/v1/ops/readiness.proto)
- [`crates/zinder-runtime/src/readiness.rs`](../../crates/zinder-runtime/src/readiness.rs)
