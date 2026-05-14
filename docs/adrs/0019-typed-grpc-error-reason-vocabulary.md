# ADR-0019: Typed gRPC Error Reason Vocabulary

## Context

Every Zinder gRPC method needs to return a typed error vocabulary that consumers can map onto local retry, gate, and alert decisions without parsing message strings. `QueryError` and `StoreError` map to `tonic::Status` with the appropriate code and structured detail (`BadRequest`, `PreconditionFailure`, `ResourceInfo`); the structured detail answers "what specifically failed". The category name of the failure (`reorg_window_exceeded`, `broadcast_disabled`, `artifact_unavailable`, etc.) is the orthogonal axis a client gates on.

The `google.rpc.ErrorInfo` shape is the standard for this in the protobuf ecosystem. It carries three fields: a stable `reason` enum value, a `domain` namespacing the reason, and a `metadata` key-value map. `tonic-types` is already a workspace dependency and `Status::with_error_details` is already used at every detail layer.

## Decision

**Add a proto enum `zinder.v1.ops.ErrorReason`** in `crates/zinder-proto/proto/zinder/v1/ops/error.proto`. One variant per current failure mode that surfaces at a gRPC boundary; reserved slots `34..=63` keep additive growth cheap; existing variants' semantics are stable within a major version.

**Attach `ErrorReason` via the existing `tonic_types::ErrorDetails::set_error_info(reason.as_str_name(), "zinder.dev", metadata)` builder.** No custom `ErrorDetail` message is introduced. The auxiliary detail types (`BadRequest.field_violations`, `PreconditionFailure.violations`, `ResourceInfo.resource_type/name`) continue to ride alongside; they answer "what specifically failed" while `ErrorReason` answers "which category of failure."

**Domain is `zinder.dev`.** Clients match on the domain before trusting the reason to avoid mistaking a non-Zinder service's `ErrorInfo` for a Zinder one.

**Server side:** `status_from_query_error` and `status_from_store_error` each gain a `code_and_typed_detail_for` (or equivalent) split that returns the `(Code, ErrorDetails)` pair without the `ErrorInfo`, then `set_error_info` is applied uniformly before constructing the final `Status`. `error_reason_for_*` maps each Rust variant to its `ErrorReason`.

**Client side:** `IndexerError::from_status` parses `ErrorInfo` from `Status::get_error_details()`, validates the domain, and uses the reason to choose the most specific `IndexerError` variant. When `ResourceInfo` accompanies an `ARTIFACT_UNAVAILABLE` reason, the family/key fields are preserved into `IndexerError::ArtifactUnavailable`. Responses without a Zinder-domain `ErrorInfo` fail closed as `ServiceUnavailable`; the client refuses to guess a typed reason from the bare status code.

**Add `IndexerError::reason() -> Option<ErrorReason>`** so consumers can pattern-match on the typed reason without crossing an extra deserialization step.

**Add `RetryPolicy::{RetryWithBackoff, OperatorActionRequired, ClientError}`** and `IndexerError::retry_policy() -> RetryPolicy`. Each `IndexerError` variant maps to one policy. Consumers consult the policy to drive retry / alert / fail decisions.

## Consequences

**Clients pin to typed reasons, not strings.** A consumer that needs to detect "broadcast disabled" matches `IndexerError::reason() == Some(ErrorReason::BroadcastDisabled)`, not the message text. The wire shape carries the reason name (`"BROADCAST_DISABLED"`) so non-Rust SDKs and OpenAPI-driven consumers see the same vocabulary.

**Existing structured detail remains the failure-shape contract.** A `RangeTooLarge` failure still ships its `BadRequest` field violation; `ArtifactUnavailable` still ships its `ResourceInfo`. The `ErrorReason` is *additive*, not a replacement — it names the category, not the shape.

**One additional `ErrorInfo` per status.** A few hundred bytes of metadata per gRPC error. Negligible compared to the existing detail payload.

**`QueryError::Store` short-circuits to `status_from_store_error`.** Store errors get their reason via the store crate, so the query crate's `error_reason_for_query_error` returns `Unspecified` for the `Store` arm; the actual reason is set inside `status_from_store_error`. This mirrors the existing `Status` construction split and keeps the two crates' vocabularies independent.

**Public client API additions:** `RetryPolicy`, `IndexerError::reason()`, `IndexerError::retry_policy()`, and re-export of `zinder_proto::v1::ops::ErrorReason` from `zinder-client`. Consumers building on the existing `IndexerError` see the new accessors as additive.

**`tonic-types` is a direct dep of `zinder-client`** because the `ErrorInfo` extractor requires `tonic_types::StatusExt`. The dep is small and already in the workspace tree.

## Alternatives Considered

**Custom `ErrorDetail` proto message wrapping reason + metadata.** Rejected. Duplicates `google.rpc.ErrorInfo`. Every consumer SDK that already knows `ErrorInfo` would need a Zinder-specific path; aligning with the upstream convention gives the same expressiveness for free.

**Reason as a string field on `Status::metadata()`.** Rejected. Metadata is a flat key-value layer with no schema; consumers cannot tell a Zinder reason apart from an arbitrary HTTP header. `ErrorInfo` is the right layer because it is part of the structured detail payload.

**Use only `Status::code()` and skip `ErrorReason`.** Rejected. The gRPC code is a 16-value retry-semantics signal (`InvalidArgument`, `FailedPrecondition`, `NotFound`, `DataLoss`, `Unavailable`, etc.). Multiple Zinder failure categories share a code; the typed reason is what distinguishes them.

## References

- [`crates/zinder-proto/proto/zinder/v1/ops/error.proto`](../../crates/zinder-proto/proto/zinder/v1/ops/error.proto)
- [`services/zinder-query/src/grpc/mod.rs`](../../services/zinder-query/src/grpc/mod.rs) (`status_from_query_error`)
- [`crates/zinder-store/src/grpc_status.rs`](../../crates/zinder-store/src/grpc_status.rs) (`status_from_store_error`)
- [`crates/zinder-client/src/error.rs`](../../crates/zinder-client/src/error.rs) (`IndexerError::from_status`, `reason()`, `retry_policy()`)
- [ADR-0020: Machine-readable readiness causes](0020-machine-readable-readiness-causes.md) — sibling proto-defined vocabulary
