# ADR-0011: Cross-Plane Chain-View Envelope

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Cross-plane chain-state contract, freshness, error vocabulary |
| Related | [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0010](0010-transaction-public-facts.md), [ADR-0024](0024-wire-format-rpc-byte-order.md), [Explorer plane](../architecture/explorer-plane.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Public interfaces §Vocabulary](../architecture/public-interfaces.md#vocabulary), [Reference: error vocabulary](../reference/error-vocabulary.md) |

## Context

Every Zinder read response across the wallet, explorer, and ingest-control planes describes the chain snapshot it answers from. The chain-state axes are the same everywhere: which chain epoch, the visible and settled tips, how far the derive projections have replayed, and what the upstream node sees. One `ChainView` message carries all of them at field tag 1 on every response.

Wallet correctness needs the epoch identity and the visible/settled tips: a wallet either reads a pinned epoch or accepts whatever epoch the server offers, and scans up to the settled tip. Explorer consumers need the rest: whether the mempool snapshot is 200 ms old or 8 s old, whether the derive view has fallen behind canonical state, whether a specific field is unavailable because it is private-by-design or because the parser does not support it, and which capability version produced the response so a UI can branch on shape changes.

A mempool entry from 8 s ago looks visually identical to one from 200 ms ago. Without an age field, a stale mempool dashboard becomes silently wrong. Likewise, fee fields and value pool fields can be absent for several distinct reasons (shielded; missing prevouts; source not supported; not yet indexed); collapsing all of them to "null" forces the frontend to invent its own reason vocabulary. Each explorer page would invent a slightly different one, drift would compound, and the privacy-language conventions the Zcash ecosystem has converged on ("Shielded: not visible by design") would be re-implemented per page.

The decisions to record here are:

1. A single `ChainView` message carrying the chain-state axes at field tag 1 on every `WalletQuery`, `ExplorerQuery`, and `IngestControl` read response.
2. The `{role}_tip` taxonomy (`visible_tip`, `settled_tip`, `indexed_tip`, `upstream_tip`) so the reorg-vs-replay distinction is self-evident in the field names.
3. An explorer-only `ExplorerFreshness` envelope that wraps `ChainView` with response metadata that genuinely varies per call (`snapshot_age_millis`, `unavailable[]`, `capability_version`).
4. A `UnavailableField` shape that lets a response declare specific field paths absent with a structured reason and a canonical human-readable string.
5. Three explorer-specific `ErrorReason` variants: `NOT_PUBLICLY_INDEXABLE`, `VALUE_POOL_UNSUPPORTED`, `EXPLORER_DERIVE_NOT_READY`.
6. A canonical reason-string registry in `zinder-core` so frontend and BFF render the same words.

## Decision

### `ChainView` carries the chain-state axes on every response

The chain-view family is defined in `wallet.proto` (which `explorer.proto` and `ingest.proto` already import):

```proto
message BlockTip { uint32 height = 1; string hash = 2; }   // hash: RPC byte order, 64 hex

message ChainEpoch {
  uint64 chain_epoch_id = 1;
  string network_name = 2;
  uint32 artifact_schema_version = 3;
  uint64 created_at_millis = 4;
  BlockTip visible_tip = 5;              // best visible block in the epoch
  BlockTip settled_tip = 6;              // reorg-window ceiling; the wallet scan ceiling
  uint32 sapling_commitment_tree_size = 7;
  uint32 orchard_commitment_tree_size = 8;
  uint32 ironwood_commitment_tree_size = 9;
}

message IndexedTip { BlockTip tip = 1; int64 block_time_unix_seconds = 2; }
message UpstreamTip { optional uint32 committed_height = 1; optional uint32 estimated_height = 2; }

message ChainView {
  ChainEpoch chain_epoch = 1;
  optional IndexedTip indexed_tip = 2;   // derive-replay ceiling; absent = unknown
  optional UpstreamTip upstream_tip = 3;  // upstream node's view; no single hash
  optional DeriveStatus derive = 4;       // absent = not a derive-backed read
}
```

Every read response carries `ChainView chain_view = 1;` as its first field. Wallet responses fill `chain_view.chain_epoch` and leave the derive-plane axes unset; explorer and ingest-control responses fill the axes their plane owns. Consumers read chain state the same way on every surface through `response.chain_view`.

The `{role}_tip` taxonomy names the four chain heights on one axis. `visible_tip` is the best visible block in the epoch. `settled_tip` is the reorg-window ceiling and the wallet scan ceiling; it keeps the exact semantics the former `safe_tip` fields carried, only the name changed. `indexed_tip` is the derive-replay ceiling. `upstream_tip` is the upstream node's view. "Finalized" stays forbidden (it collides with NU7/Crosslink). An absent `indexed_tip` means "derive head unknown", never "at tip"; index lag is `chain_view.chain_epoch.visible_tip.height - chain_view.indexed_tip.tip.height`. `upstream_tip` carries heights only because the upstream probe has no single block hash. The commitment-tree sizes stay on `ChainEpoch` as visible-tip scan aids.

### `ExplorerFreshness` wraps `ChainView` with explorer response metadata

```proto
message ExplorerFreshness {
  zinder.v1.wallet.ChainView chain_view = 1;   // cross-plane chain-state axes
  uint64 snapshot_age_millis = 2;               // age of the mempool snapshot, when relevant
  reserved 3, 4;                                // was derive_cursor_lag_{blocks,millis}
  string capability_version = 5;                // capability string that produced this response
  repeated UnavailableField unavailable = 6;
  reserved 7, 8;                                // was upstream, indexed_head (now on ChainView)
}
```

Every `ExplorerQuery` response carries `ExplorerFreshness freshness = 1;` as its first field. The chain-state axes (epoch, tips, upstream, derive) live on `chain_view`; this envelope keeps only the metadata that genuinely varies per explorer call.

`capability_version` carries the exact string from `ZINDER_CAPABILITIES` that produced the response (e.g. `explorer.transaction.detail_v1`). When a future `_v2` ships alongside `_v1`, clients can branch on which version a particular response uses without parsing the descriptor again.

`chain_view.upstream_tip` carries the upstream node's view of the chain at response-construction time, mirroring `committed_height` and `estimated_height` from `UpstreamHealthSnapshot`. The axis is optional because the source-plane probe is async; a response that fires before the first probe leaves it unset. Consumers MUST treat the absence as "unknown", not zero. This lets explorer consumers render an honest sync-progress UI ("block X of Y") against the real chain tip without reinventing protocol invariants (block-time math) client-side.

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

### Indexed tip is computed at response time; derive health is persisted

The explorer reads the highest materialized `BlockSummary` row at response time and returns it as `chain_view.indexed_tip` (a `BlockTip` plus block time). All chain-event derive consumers advance under one shared cursor, so the block-summary head is an accurate indexed tip for every capability: a single value, not one per consumer. The consumer computes index lag as `chain_view.chain_epoch.visible_tip.height - chain_view.indexed_tip.tip.height`.

Block lag alone cannot distinguish a derive plane that is healthily catching up from one that has stalled; a paused replay holds the lag constant. The ingest plane therefore persists a `DeriveStatus { health, indexed_height, lag_blocks, observed_at_millis }` record into the shared derive store on every replay tick, including the paused branch. The explorer folds it into `chain_view.derive` on derive-backed responses and onto `ExplorerServerInfo.derive_status`. `DeriveStatus` and `DeriveHealth` live in `wallet.proto` as part of the chain-view family:

```proto
enum DeriveHealth {
  DERIVE_HEALTH_UNSPECIFIED = 0;
  DERIVE_HEALTH_LIVE = 1;          // indexed head at the canonical tip
  DERIVE_HEALTH_CATCHING_UP = 2;   // replay advancing, behind tip
  DERIVE_HEALTH_PAUSED = 3;        // replay paused (e.g. memory pressure)
}
```

`DeriveStatus`'s `observed_at_millis` lets an operator detect a status record that has itself gone stale (the ingest plane stopped writing it). When index lag exceeds the deployment threshold, a request may still return `EXPLORER_DERIVE_NOT_READY` for views that strictly require fresh state.

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
- The full `ExplorerFreshness` metadata (`snapshot_age_millis`, `unavailable[]`, `capability_version`) on `WalletQuery` responses. Wallet responses carry `ChainView` for the chain-state axes; the explorer-only metadata stays on `ExplorerFreshness`.
- Localization of `human_reason` strings. The strings are English-only in v1; per-locale rendering is a UI concern.

## Revision history

- 2026-07-05: Added `uint32 ironwood_commitment_tree_size = 9` to `ChainEpoch` for the Ironwood (NU6.3) shielded pool, alongside the existing Sapling and Orchard tree-size fields. Additive; consumers that do not read it see no behavior change. Because every Zinder/Zexplorer consumer is unreleased alpha, the change lands on `_v1` in place.
- 2026-06-24: Generalized the explorer freshness envelope into the cross-plane `ChainView` envelope carried at field tag 1 on every `WalletQuery`, `ExplorerQuery`, and `IngestControl` read response. `ChainView` folds the three prior freshness shapes (the bare wallet `ChainEpoch` echo, the `ExplorerFreshness` wrapper, the flat `WriterStatus` scalars) into one. Introduced the `{role}_tip` taxonomy: `ChainEpoch` reshaped to carry `BlockTip visible_tip`/`settled_tip` (replacing `tip_height`/`tip_hash`/`safe_tip_height`/`safe_tip_hash`); `IndexedHead` became `IndexedTip { BlockTip tip; ... }`; `UpstreamObservation` became `UpstreamTip { committed_height, estimated_height }` (dropping `upstream_verification_progress`). `settled_tip` keeps the exact reorg-window semantics of the former `safe_tip`. `DeriveStatus`/`DeriveHealth` moved from `explorer.proto` into `wallet.proto` so the chain-view family is self-contained. `ExplorerFreshness` now wraps `ChainView` and keeps only `snapshot_age_millis`, `unavailable[]`, and `capability_version` (its former `chain_epoch`, `upstream`, `indexed_head` fields fold into `chain_view`; tags 7, 8 reserved). `WriterStatusResponse` replaced its flat `latest_writer_*` scalars with `ChainView`. Breaking, response-side only; requests still pin with the existing `ChainEpoch`. Because every Zinder/Zexplorer consumer is unreleased alpha, the change lands on `_v1` in place.
- 2026-05-29: Replaced `derive_cursor_lag_blocks`/`derive_cursor_lag_millis` (tags 3, 4, now reserved) with `optional IndexedHead indexed_head = 8`, and added `DeriveHealth` + `DeriveStatus` surfaced on `ExplorerServerInfo.derive_status`. Naming the indexed head explicitly, instead of a derived lag number, removes the proto3 zero-vs-absent ambiguity that made "at tip" and "field unset" indistinguishable, and gives consumers the indexed block's identity and time for honest age display. The persisted `DeriveStatus` makes a memory-paused derive plane observable on the wire (`DERIVE_HEALTH_PAUSED`) instead of silent. All explorer read RPCs now build freshness through one shared builder, so `indexed_head` is populated uniformly rather than hardcoded to zero on the RPCs that previously did not compute lag. Because every Zinder/Zexplorer consumer is unreleased alpha, the change lands on `_v1` in place rather than as a new `_v2` capability.
- 2026-05-26: Added `optional UpstreamObservation upstream = 7` to `ExplorerFreshness`. Lets explorer consumers render honest sync-progress UI against the upstream node's own committed/estimated tips and verification progress instead of reinventing protocol invariants (block-time math) client-side. The field is optional and additive; consumers that do not read it see no behavior change. Source plane already produces the values via `UpstreamHealthSnapshot`; the explorer adapter caches the snapshot in a small background probe and folds it into every freshness envelope on response construction. Because every Zinder/Zexplorer consumer is unreleased alpha, the change lands on `_v1` in place rather than as a new `_v2` capability.
