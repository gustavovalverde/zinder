# ADR-0013: Source failure recovery topology

## Status

Accepted on 2026-05-16.

## Context

`zinder-ingest` runs four long-lived loops that observe upstream node state:

- `tip-follow`: per-poll tip observation plus chain-tip push subscription.
- `backfill-until-complete`: historical range catchup.
- `mempool-orchestrator`: mempool source stream consumption.
- `chain-tip-notification`: indexer gRPC re-subscriber.

Before this decision, the source boundary tagged every failure with `is_retryable: bool` and the loops above used that bool as the discriminator between "drain readiness and retry" and "exit the process." On 2026-05-15 the Railway production deployment `637cf727-3267-46ce-8e9c-008d3b448e7b` exited because Zebra returned a `getblockhash` error whose JSON-RPC code was not on the adapter's small retryable whitelist (`-28` only); the adapter stamped `is_retryable: false` on the resulting `SourceError::BlockUnavailable` for the string `"block height not in best chain"`, the tip-follow loop saw a non-retryable error, and the writer exited.

A pre-existing runbook draft already specified that retryable upstream failures must be readiness transitions and not process exits, but it tied recovery to "errors that the source boundary classifies as retryable." That phrasing made the bool the load-bearing contract field, and the bool was wrong by construction: the adapter could not enumerate every JSON-RPC error code Zebra would ever return, and any code it had not yet whitelisted defaulted to fatal.

## Decision

1. **Loop lifecycle is owned by the loop, not by the source.** A new function `services/zinder-ingest/src/source_recovery.rs::decide_recovery(&IngestError, SourceRecoveryBackoff) -> SourceRecoveryDecision` returns `Recover { failure_class, last_reason, backoff }` or `Exit`. Every long-lived writer loop consults it.
2. **Source errors are always loop-recoverable.** Every `SourceError` variant returns `Recover`. The only exit paths are storage failures, reorg-window violations, and internal logic errors (`EmptyCanonicalBatch`, etc.) where data integrity is at stake.
3. **JSON-RPC error codes are default-open.** `zebra_json_rpc.rs::JsonRpcCallError::structural_failure()` returns `Some(...)` only for codes that are definitively caller-side bugs (`-22` invalid encoding, `-27` duplicate broadcast). Every other code, including unknown codes the adapter has not seen before, becomes a recoverable `SourceError` variant by structural identity.
4. **Source errors describe what happened, not what to do.** The `is_retryable: bool` field is removed from every `SourceError` variant. A new descriptive enum `SourceFailureClass` (`NodeUnreachable`, `UpstreamViewChanged`, `StreamDisconnected`, `CapabilityMissing`, `ProtocolMismatch`, `Malformed`, `Configuration`) is the operator-facing label; `SourceError::upstream_classification()` returns it.
5. **Readiness carries operator detail.** `ReadinessCause::NodeUnavailable` becomes `NodeUnavailable(NodeUnavailableDetail)` carrying `{ failure_class, last_reason, consecutive_failures, outage_seconds }`. `/readyz`, the proto report, and `zinder_readiness_node_failure_class{class=...}` Prometheus gauge all surface this detail.
6. **Per-call retry handles narrow transport classes.** `retry_source_request` retries only `NodeUnreachable` and `StreamDisconnected` failures. Everything else bubbles to the loop, which re-observes upstream state before issuing dependent requests.

## Consequences

- The production scenario (Zebra returns a non-`-28` JSON-RPC error during tip-follow) is now a `node_unavailable` readiness transition with `failure_class = "upstream_view_changed"` and the writer stays alive across the outage.
- The four loops share one recovery primitive instead of three differently shaped retry bodies (and a fourth orchestrator with no retry classification at all). Adding a new long-lived loop follows the same template.
- Operators reading `/readyz` learn *which kind* of upstream failure is happening, how many iterations the writer has been retrying, and how long the outage has lasted, without consulting logs.
- New `SourceError` variants only have to pick a `SourceFailureClass`; the bool is gone and cannot be set wrong.
- New Zebra error codes default to recoverable rather than fatal. The whitelist of structural codes is the small set; unknown codes keep the writer alive.
- The breaking changes (removing `is_retryable: bool`, extending `ReadinessCause::NodeUnavailable`, renaming `set_tip_follow_source_unavailable` to `set_tip_follow_node_unavailable`) are intentional: the previous shape was load-bearing for the incident. There is no value in carrying it forward.

## Alternatives considered

- **Add more JSON-RPC error codes to the retryable whitelist.** Rejected: shifts the burden of code enumeration to every Zebra upgrade and keeps unknown codes fatal. The default direction is the bug.
- **Reuse the `is_retryable: bool` but invert the default.** Rejected: the bool is in the wrong place. The loop, not the source, owns lifecycle. A bool field would persist the indirect-authority problem.
- **Three-class taxonomy (Transient / ViewChanged / Fatal) consumed by the loop.** Considered. Rejected for the loop's control flow because the loop's posture is identical for the first two (re-observe, back off, continue). Differentiation is preserved at the readiness/observability layer via `SourceFailureClass` (seven classes) without multiplying loop branches.
- **Three-backoff constants (one per loop).** Rejected: drifts independently. Folded into a single `SourceRecoveryBackoff` struct with per-class fields.

## Related

- `docs/architecture/node-source-boundary.md` — Capability Model section: `SourceFailureClass` taxonomy and the operator-action table (`failure_class` → action).
- `docs/architecture/service-operations.md` — Health and Readiness section: the `NodeUnavailableDetail` payload contract.
- `docs/architecture/chain-ingestion.md` — Backfill and Tip Following: loop-owned recovery posture.
