# Error Vocabulary

This page lists every `zinder.v1.ops.ErrorReason` value, the gRPC `Status` code it pairs with, the recommended retry policy, and the auxiliary detail clients should expect alongside it. Use it as the reference when you map Zinder errors to your local retry, alert, and operator-action decisions.

The proto enum is defined in [`crates/zinder-proto/proto/zinder/v1/ops/error.proto`](../../crates/zinder-proto/proto/zinder/v1/ops/error.proto); this page owns the wire semantics clients should rely on.

## How to read the wire

Every Zinder gRPC failure ships:

- A `tonic::Status` carrying a standard gRPC `Code` (one of 16).
- A `google.rpc.ErrorInfo` detail with `domain = "zinder.dev"` and `reason = <ErrorReason name>` (the string-form name of the enum value, e.g. `"BROADCAST_DISABLED"`).
- For some reasons, an additional structured detail (`BadRequest.field_violations`, `PreconditionFailure.violations`, `ResourceInfo`).

Clients consult `ErrorInfo.reason` for the typed category and the auxiliary detail for the failure shape. Match on the domain first to avoid mistaking a non-Zinder service's `ErrorInfo` for a Zinder one.

The Rust client crate `zinder-client` exposes the typed accessors:

```rust
use zinder_client::{IndexerError, RetryPolicy};

match indexer_error {
    err if err.retry_policy() == RetryPolicy::OperatorActionRequired => alert_oncall(&err),
    err if err.retry_policy() == RetryPolicy::ClientError => fix_request(&err),
    err => retry_with_backoff(&err),
}
```

## Retry policy semantics

| Policy | Meaning |
| --- | --- |
| `RetryWithBackoff` | Remote service or upstream node is transiently unavailable. The request shape is correct. Retry with exponential backoff. |
| `OperatorActionRequired` | The deployment needs a manual fix before the request can succeed. Page the operator; do not retry without reconfiguration. |
| `ClientError` | The request itself is malformed or out of bounds. Fix the request and re-issue; retrying the same input will fail again. |

## Reason table

### `INVALID_ARGUMENT` family

The request shape failed validation. Retry policy: **ClientError**. Carries `BadRequest.field_violations` naming the offending field and a human-readable reason.

| Reason | What it means | Example metadata |
| --- | --- | --- |
| `INVALID_BLOCK_RANGE` | `start_height` exceeds `end_height` | `field_violations[start_height, end_height]` |
| `COMPACT_BLOCK_RANGE_TOO_LARGE` | Requested range exceeds the per-deployment cap | `field_violations[end_height]` |
| `CHAIN_EVENT_CURSOR_INVALID` | Cursor bytes failed to parse, or are for a different network / store / stream family | `field_violations[from_cursor]` |
| `ADDRESS_OUTPUT_CURSOR_INVALID` | Same as above for address-output streams | `field_violations[from_cursor]` |
| `TRANSPARENT_HISTORY_CURSOR_INVALID` | Same as above for transparent-history streams | `field_violations[from_cursor]` |
| `INVALID_ADDRESS` | Address selector is empty, malformed, or targets a different network | `field_violations[address]` |
| `UNSUPPORTED_SHIELDED_PROTOCOL` | Shielded protocol value is not supported by the wallet protocol | `field_violations[shielded_protocol]` |
| `INVALID_CHAIN_STORE_OPTIONS` | Operator misconfiguration in `[storage]` | — |
| `ARTIFACT_PAYLOAD_TOO_LARGE` | Stored artifact exceeds size limits (deployment data issue) | — |
| `INVALID_CHAIN_EPOCH_ARTIFACTS` | A future commit batch failed structural validation | — |

### `FAILED_PRECONDITION` family

The request shape is valid but the deployment is in a state that cannot serve it. Retry policy: **OperatorActionRequired** unless noted. Carries `PreconditionFailure.violations` with a typed `type` + `subject` + `description`.

| Reason | What it means | Example metadata |
| --- | --- | --- |
| `BROADCAST_DISABLED` | The `broadcaster` is the no-op `()` implementation; deployment is read-only | `type=TRANSACTION_BROADCAST_DISABLED, subject=wallet.broadcast.transaction_v1` |
| `CHAIN_EVENT_CURSOR_EXPIRED` | Cursor sequence is older than retained history (consumer fell behind) | `type=CHAIN_EVENT_CURSOR_EXPIRED, subject=chain_event:<seq>` |
| `MEMPOOL_EVENT_CURSOR_EXPIRED` | Same as above for the mempool event stream | `type=MEMPOOL_EVENT_CURSOR_EXPIRED, subject=mempool_event:<seq>` |
| `CHAIN_EPOCH_PIN_UNSUPPORTED` | Request pinned `at_epoch` but the query implementation does not support it | `subject=at_epoch` |
| `CHAIN_EPOCH_PIN_UNAVAILABLE` | Pinned chain epoch is no longer retained | `subject=chain_epoch:<id>` |
| `CHAIN_EPOCH_PIN_MISMATCH` | Pinned chain epoch resolves to incompatible storage state | `subject=chain_epoch:<id>` |
| `SCHEMA_MISMATCH` | Persistent store schema is incompatible with the running binary | — |
| `SCHEMA_TOO_NEW` | Store was opened by a newer Zinder; rolling back is unsafe | — |
| `REORG_WINDOW_EXCEEDED` | A reorg crossed the configured non-finalized window; operator must reconcile | — |
| `CHAIN_EPOCH_CONFLICT` | Detected chain-epoch contention between writer and reader | — |
| `CHAIN_EPOCH_NETWORK_MISMATCH` | Store opened against the wrong `network` | — |

### `NOT_FOUND` family

The requested resource does not exist. Retry policy: **RetryWithBackoff** (the resource may appear after a future commit). Some reasons carry `ResourceInfo` naming the family/key.

| Reason | What it means | Example metadata |
| --- | --- | --- |
| `ARTIFACT_UNAVAILABLE` | A specific artifact is missing from the named family at the visible epoch | `resource_type=<family>, resource_name=<key>` |
| `CHAIN_EPOCH_MISSING` | Chain epoch is not retained (often: pruned) | `resource_type=ChainEpoch, resource_name=chain_epoch:<id>` |
| `BLOCK_NOT_IN_BEST_CHAIN` | The requested block exists but is not on the visible best chain | — |

### `DATA_LOSS` family

Persistent data could not be decoded. Retry policy: **OperatorActionRequired**. Pages on-call; this is the strongest signal that storage corruption needs to be investigated.

| Reason | What it means |
| --- | --- |
| `COMPACT_BLOCK_PAYLOAD_MALFORMED` | A persisted compact block failed to deserialize |
| `ARTIFACT_CORRUPT` | A persisted artifact's checksum or shape is invalid |

### `UNAVAILABLE` family

Service is reachable but a dependency is not, or the operation is not yet supported. Retry policy: **RetryWithBackoff** unless the reason names a permanent unsupported feature.

| Reason | What it means |
| --- | --- |
| `NODE_UNAVAILABLE` | Upstream node (Zebra) is unreachable or returning errors |
| `STORAGE_UNAVAILABLE` | Local storage cannot serve the request (fall-through case) |
| `BLOCKING_TASK_FAILED` | An internal background task failed unexpectedly |
| `UNSUPPORTED_CHAIN_EVENT` | A future event type is not yet supported by this binary |
| `UNSUPPORTED_BLOCK_SELECTOR` | A future selector shape is not yet supported |
| `UNSUPPORTED_TRANSACTION_STATUS` | A future tx-status variant is not yet decodable |

### `INTERNAL` family

A self-inflicted failure that needs investigation. Retry policy: **OperatorActionRequired**.

| Reason | What it means |
| --- | --- |
| `ENTROPY_UNAVAILABLE` | The deployment cannot read OS-level entropy (very rare) |

### Sentinel

`ERROR_REASON_UNSPECIFIED = 0` is the default scalar. It is never emitted intentionally; if a client receives it, treat as a Zinder bug and report it. Clients with `IndexerError::reason() == None` for an error carried over the wire have hit a server that omitted `ErrorInfo` or emitted `Unspecified`.

## Stability

The set above is the v1 contract. Additions are allowed within a major version; the reserved range in [`error.proto`](../../crates/zinder-proto/proto/zinder/v1/ops/error.proto) leaves room for additive growth. Existing reasons' semantics are stable within a major version. Removing or repurposing a reason requires a major-version boundary because clients make retry and alerting decisions from this vocabulary.

## References

- [`crates/zinder-proto/proto/zinder/v1/ops/error.proto`](../../crates/zinder-proto/proto/zinder/v1/ops/error.proto)
- [`crates/zinder-client/src/error.rs`](../../crates/zinder-client/src/error.rs)
- [Server-side wallet pattern](server-side-wallet-pattern.md)
- [Public interfaces](../architecture/public-interfaces.md)
