# ADR-0011: Explorer Freshness Envelope

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Explorer wire surface, freshness contract, error vocabulary |
| Related | [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0010](0010-transaction-public-facts.md), [Explorer plane](../architecture/explorer-plane.md), [Public interfaces §Capability Discovery](../architecture/public-interfaces.md#capability-discovery), [Reference: error vocabulary](../reference/error-vocabulary.md) |

## Context

The wallet plane already carries `ChainEpoch` on every read response. `ChainEpoch` tells a wallet which chain snapshot it is reading from, what the visible tip height is, and what the artifact schema version is. That is enough for wallet correctness: a wallet either reads a pinned epoch or accepts whatever epoch the server offers.

Explorer consumers need more. An explorer page needs to know whether the mempool snapshot it is rendering is 200 ms old or 8 s old, whether the derive view it is reading has fallen behind canonical state, whether a specific field on the response is unavailable because it is private-by-design or because the parser does not support it, and which capability version produced the response so a UI can branch on shape changes.

A mempool entry from 8 s ago looks visually identical to one from 200 ms ago. Without an age field, a stale mempool dashboard becomes silently wrong. Likewise, fee fields and value pool fields can be absent for several distinct reasons (shielded; missing prevouts; source not supported; not yet indexed); collapsing all of them to "null" forces the frontend to invent its own reason vocabulary. Each explorer page would invent a slightly different one, drift would compound, and the privacy-language conventions the Zcash ecosystem has converged on ("Shielded — not visible by design") would be re-implemented per page.

The decisions to record here are:

1. A single `ExplorerFreshness` message embedded on every explorer response as field tag 1.
2. A `UnavailableField` shape that lets a response declare specific field paths absent with a structured reason and a canonical human-readable string.
3. Three new `ErrorReason` variants for explorer-specific conditions: `NOT_PUBLICLY_INDEXABLE`, `VALUE_POOL_UNSUPPORTED`, `EXPLORER_DERIVE_NOT_READY`.
4. A canonical reason-string registry in `zinder-core` so frontend and BFF render the same words.

## Decision

### `ExplorerFreshness` embeds on every explorer response

```proto
message ExplorerFreshness {
  ChainEpoch chain_epoch = 1;            // canonical wallet-plane primitive
  uint64 snapshot_age_millis = 2;        // age of the mempool snapshot, when relevant
  uint64 derive_cursor_lag_blocks = 3;   // explorer derive cursor lag vs canonical tip
  uint64 derive_cursor_lag_millis = 4;   // wall-clock equivalent of the block lag
  string capability_version = 5;         // capability string that produced this response
  repeated UnavailableField unavailable = 6;
}
```

Every `ExplorerQuery` response carries `ExplorerFreshness freshness = 1;` as its first field. Responses that do not touch mempool state leave `snapshot_age_millis = 0`; responses that do not depend on the derive cursor leave `derive_cursor_lag_*` zero. The field is present unconditionally so consumers can write `response.freshness.chain_epoch` without conditional checks.

`capability_version` carries the exact string from `ZINDER_CAPABILITIES` that produced the response (e.g. `explorer.transaction.detail_v1`). When a future `_v2` ships alongside `_v1`, clients can branch on which version a particular response uses without parsing the descriptor again.

### `UnavailableField` carries structured reasons

```proto
message UnavailableField {
  string field_path = 1;                 // dotted path: "fee.computed.zip317_conventional_zat"
  UnavailableReason reason = 2;
  string human_reason = 3;               // canonical reason string for UI render
}

enum UnavailableReason {
  UNAVAILABLE_REASON_UNSPECIFIED = 0;
  UNAVAILABLE_PRIVATE_BY_DESIGN = 1;     // shielded; not missing
  UNAVAILABLE_PREVOUTS_MISSING = 2;      // fee can't compute without prevouts
  UNAVAILABLE_UPSTREAM_NOT_SUPPORTED = 3;
  UNAVAILABLE_NOT_INDEXED = 4;
  UNAVAILABLE_STALE = 5;
  UNAVAILABLE_PARSER_VERSION_UNSUPPORTED = 6;
}
```

`field_path` uses dotted notation matching the proto field hierarchy. A response that omits the ZIP-317 conventional fee carries an `UnavailableField` with `field_path = "fee.computed.zip317_conventional_zat"`, `reason = UNAVAILABLE_PREVOUTS_MISSING`, and `human_reason = "Conventional-fee comparison needs all input prevouts; one or more are unresolved."`

The `reason` enum is the branch-on value for client logic. The `human_reason` string is the render-verbatim value for UI display. Both are populated server-side from the canonical reason-string registry in `zinder-core` so a frontend, a CLI, and a coding agent all see the same words for the same condition.

### New `ErrorReason` variants for explorer conditions

Three additions to the existing `ErrorReason` enum in `crates/zinder-proto/proto/zinder/v1/ops/error.proto`:

| Variant | Numeric value | gRPC `Status` | Use case |
| ------- | ------------- | ------------- | -------- |
| `NOT_PUBLICLY_INDEXABLE = 34` | 34 | `InvalidArgument` | `ExplorerQuery.Search` receives a shielded address or viewing key; the response refuses with a typed reason rather than returning an empty match list. |
| `VALUE_POOL_UNSUPPORTED = 35` | 35 | `Unavailable` | `ExplorerQuery.ValuePoolSummary` is called on a deployment whose source does not surface chain value pools. The capability `explorer.value_pool.summary_v1` is also absent. |
| `EXPLORER_DERIVE_NOT_READY = 36` | 36 | `Unavailable` | The explorer derive cursor is too far behind canonical state to answer the requested view. The error carries a `derive_cursor_lag_blocks` detail so the client can render a "catching up" state. |

Reserved error numbers 37–63 stay reserved for additive growth. These three slot into the existing namespace without breaking any prior `ErrorReason` variant.

### Canonical reason-string registry

`crates/zinder-core/src/explorer_reasons.rs` declares the canonical reason strings:

```rust
pub mod reasons {
    pub const SHIELDED_NOT_PUBLICLY_VISIBLE: &str =
        "Shielded — not publicly visible by design.";
    pub const SHIELDED_RECEIVER_NO_HISTORY: &str =
        "No public history — shielded receiver.";
    pub const VIEWING_KEY_NEVER_INDEXED: &str =
        "Viewing keys are never indexed server-side.";
    pub const TEX_ADDRESS_TRANSPARENT_ONLY: &str =
        "TEX address — transparent inputs only (ZIP-320).";
    pub const FEE_PREVOUTS_UNRESOLVED: &str =
        "Fee requires all input prevouts; one or more are unresolved.";
    pub const FEE_FUTURE_TX_VERSION: &str =
        "Fee unavailable — transaction version is newer than this indexer.";
    pub const ZIP317_NEEDS_FEE: &str =
        "Conventional-fee comparison needs the actual fee, which is unavailable.";
    pub const VALUE_POOL_SOURCE_UNSUPPORTED: &str =
        "Chain value pools are not exposed by this upstream source.";
    pub const EXPIRY_HEIGHT_NOT_SET: &str =
        "Transaction has no expiry (ZIP-203 nExpiryHeight = 0).";
    pub const PARSER_VERSION_UNSUPPORTED: &str =
        "Transaction includes sections this indexer cannot decode.";
    // ...
}
```

Wire responses populate `human_reason` from these constants. UI consumers can either render the string verbatim or branch on the `UnavailableReason` enum to render locale-specific text. The constants are intentionally short, present-tense, end with a period, and do not include emoji.

The list grows additively. A new condition adds a new constant; existing constants are stable and grep-able.

### Derive cursor lag is computed at response time

The explorer service tracks its derive cursor per consumer. When constructing a response, it computes:

- `derive_cursor_lag_blocks = canonical_tip_height - explorer_cursor_height`
- `derive_cursor_lag_millis = wall_clock_now - canonical_tip_observed_at`

These two fields together describe both "how many blocks behind" (deterministic) and "how long has it been since the last advance" (wall-clock; surfaces stuck consumers).

A consumer that has caught up to within one block reports `derive_cursor_lag_blocks = 0`, `derive_cursor_lag_millis = small`. A consumer that is stuck on a particular block reports the lag growing in `derive_cursor_lag_millis` even though `derive_cursor_lag_blocks` stays constant.

When the derive cursor lag exceeds the deployment's `explorer.freshness.max_lag_blocks` threshold (operator-configured), the response carries `ExplorerFreshness.unavailable` entries flagging affected fields as `UNAVAILABLE_STALE`, and the request may return `EXPLORER_DERIVE_NOT_READY` for views that strictly require fresh state.

## Consequences

### Operational

- Every explorer response is heavier by the size of `ExplorerFreshness`. Typical payload addition: ~80 bytes (`ChainEpoch` is ~60 bytes; the surrounding fields add ~20 bytes). For high-throughput endpoints the overhead is bounded; for typical explorer requests (one page render) the overhead is negligible.
- Operators get a new ops metric `zinder_explorer_derive_cursor_lag_blocks{consumer="..."}` surfaced from the same calculation that populates the wire field. Dashboards graph cursor lag over time; alerts fire when lag exceeds the configured threshold.

### Implementation

- `crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto` adds `ExplorerFreshness`, `UnavailableField`, `UnavailableReason`.
- `crates/zinder-proto/proto/zinder/v1/ops/error.proto` extends `ErrorReason` with the three new variants. Numbering is additive; existing variants keep their numbers.
- `crates/zinder-core/src/explorer_reasons.rs` is new.
- Every explorer RPC handler constructs `ExplorerFreshness` as part of its response. A helper `freshness_for_capability(capability, derive_store, chain_epoch) -> ExplorerFreshness` keeps the boilerplate small.
- `docs/reference/error-vocabulary.md` adds rows for the three new `ErrorReason` variants with their retry semantics.

### Testing

- A `freshness_envelope_present_on_every_explorer_response` integration test in `services/zinder-explorer/tests/integration/` introspects the proto descriptors and asserts every `ExplorerQuery` RPC's response message has `ExplorerFreshness freshness = 1`.
- Per-RPC tests assert `capability_version` matches the expected `explorer.*` string.
- Privacy tests assert that responses for shielded entities carry `UNAVAILABLE_PRIVATE_BY_DESIGN` rather than silently empty fields.

## Alternatives Considered

### Carry only `ChainEpoch` and let clients fetch separate `/freshness` data

Rejected. The frontend round-trip cost is real, and the freshness fields are tightly coupled to the specific response: an `UnavailableField` is per-response, not per-server. Fetching freshness separately also makes it impossible to consistently render "this page is stale" when the underlying read raced an epoch advance.

### Use the gRPC error model exclusively for unavailability

Rejected. gRPC errors are one-per-RPC; a `TransactionDetailResponse` whose `expiry_height` is genuinely "not set" (ZIP-203 sentinel) and whose `fee.zip317_conventional_zat` is unavailable (prevouts missing) needs two distinct reasons on one successful response. Forcing them into errors would either lose the partial response or invent a "partial success" error shape that does not exist in gRPC's vocabulary.

### Render reason strings on the client only

Rejected. Each client would invent its own copy. Worse, agents calling the API as a JSON contract would not see the canonical strings at all and would invent prose. Centralizing the strings server-side gives every consumer the same words and lets a single PR change the language for every UI.

### Add unavailability as nullable proto fields without structured reasons

Rejected. A nullable field tells the client "value absent" but not why. The PRD's privacy-language requirement (Shielded vs missing vs unsupported) is exactly the case where a nullable bit collapses three different conditions into one. Structured `UnavailableField` carries the distinction explicitly.

## Out of Scope

- HTTP-level ETags or `Cache-Control` headers. gRPC is the primary transport; HTTP fronting is operator concern, not part of the wire contract.
- A subscription channel for derive-cursor-lag updates. Clients re-poll explorer responses; the lag fields surface on every response and are sufficient for UI rendering.
- A `Freshness` message on `WalletQuery` responses. The wallet plane's correctness model does not depend on the same fields; `ChainEpoch` alone covers what wallets need.
- Localization of `human_reason` strings. The strings are English-only in v1; per-locale rendering is a UI concern.
