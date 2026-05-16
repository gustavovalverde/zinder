# ADR-0012: Typed Explorer Search And Privacy Refusal

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Explorer search surface, address classification, privacy boundary |
| Related | [ADR-0005](0005-consumer-neutral-wallet-data-plane.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0011](0011-explorer-freshness-envelope.md), [Explorer plane](../architecture/explorer-plane.md), [Public interfaces §ZIP cross-reference](../architecture/public-interfaces.md#zip-cross-reference) |

## Context

Explorer search is the entry point users type into. The PRD names the input classes the UI must route correctly: block height, block hash, transaction ID, transparent P2PKH/P2SH/TEX addresses, unified addresses, shielded addresses, unified viewing keys, and payment disclosure payloads. Some of these route to public history pages. Others must not.

A naive search endpoint returns a single match or no match. That collapses three different "no match" conditions: the input is malformed, the input is well-formed but no entity matches, and the input is well-formed but its entity is shielded and has no public history by protocol design. The first two are legitimate "not found." The third is "not publicly indexable" — a structurally different answer with privacy semantics. Returning an empty page for a shielded address tells the user "no history" when the truth is "shielded receivers do not have public history at all." That is the wrong UX.

The privacy invariant from [ADR-0005](0005-consumer-neutral-wallet-data-plane.md) and the PRD's explicit out-of-scope list (server-side shielded address scanning, persisted viewing keys, public shielded address history, memo decryption) means the explorer **must refuse** to perform a public-history lookup against shielded receivers. The refusal is the answer; it is not an error.

The decisions to record here are:

1. `ExplorerQuery.Search` returns a typed `SearchCandidate` oneof that distinguishes every input class explicitly.
2. Shielded addresses, unified viewing keys, and unified-address shielded receivers route to a typed `NotPubliclyIndexable` arm with structured reason and canonical human-readable string.
3. ZIP-316 unified addresses surface the present receiver typecodes so the UI can route the transparent receivers while refusing the shielded ones.
4. ZIP-320 TEX addresses classify as transparent-source-only and map to the underlying P2PKH address for indexable history.
5. The classifier runs locally; no derive consumer is required to ship the search RPC.

## Decision

### `SearchCandidate` is a oneof with explicit arms per class

```proto
message SearchRequest {
  string query = 1;
}

message SearchResponse {
  ExplorerFreshness freshness = 1;
  repeated SearchCandidate candidates = 2;
}

message SearchCandidate {
  oneof match {
    BlockMatch block = 1;
    TransactionMatch transaction = 2;
    TransparentAddressMatch transparent_address = 3;
    TexAddressMatch tex_address = 4;
    UnifiedAddressMatch unified_address = 5;
    ShieldedAddressMatch shielded_address = 6;
    ViewingKeyMatch viewing_key = 7;
    PaymentDisclosureMatch payment_disclosure = 8;
    UnclassifiedMatch unclassified = 9;
  }
  float confidence = 10;
}
```

Each arm carries its own typed body. `BlockMatch` carries the resolved `block_id` and a URL hint. `TransactionMatch` carries the resolved transaction ID and mined-or-mempool status. `TransparentAddressMatch` carries the canonical base58 form and the `address_script_hash`. `TexAddressMatch` carries the canonical `tex*` form, the equivalent `t*` P2PKH form, and a typed `transparent_source_only: true` flag. `UnifiedAddressMatch` carries the parsed receiver typecodes (P2PKH, P2SH, Sapling, Orchard) so the UI can route each receiver independently. `ShieldedAddressMatch`, `ViewingKeyMatch`, and the shielded receivers inside `UnifiedAddressMatch` carry the `NotPubliclyIndexable` shape (below).

`confidence` is a hint, not a contract. A search for "1000000" returns candidates for both `BlockMatch { height: 1_000_000 }` (high confidence) and `TransactionMatch { txid: ... }` (low confidence; only if a txid happens to start with those digits). The UI renders the high-confidence candidate first.

### `NotPubliclyIndexable` is the typed refusal shape

```proto
message NotPubliclyIndexable {
  NotPubliclyIndexableReason reason = 1;
  string human_reason = 2;       // from zinder-core canonical registry
  optional string canonical_form = 3;  // the classified form of the input, when safe
}

enum NotPubliclyIndexableReason {
  NOT_PUBLICLY_INDEXABLE_REASON_UNSPECIFIED = 0;
  NOT_PUBLICLY_INDEXABLE_SHIELDED_ADDRESS = 1;
  NOT_PUBLICLY_INDEXABLE_VIEWING_KEY = 2;
  NOT_PUBLICLY_INDEXABLE_SHIELDED_RECEIVER_IN_UNIFIED = 3;
}
```

`ShieldedAddressMatch { not_publicly_indexable: NotPubliclyIndexable }` is the canonical example. A unified address with both transparent and shielded receivers returns one `UnifiedAddressMatch` containing per-receiver routing, where the shielded receivers carry the same `NotPubliclyIndexable` shape inside the `UnifiedAddressMatch.receivers` repeated field.

The `human_reason` string comes from the canonical registry in `crates/zinder-core/src/explorer_reasons.rs` defined in [ADR-0011](0011-explorer-freshness-envelope.md): `SHIELDED_RECEIVER_NO_HISTORY`, `VIEWING_KEY_NEVER_INDEXED`. The UI renders these verbatim or branches on the structured reason.

`canonical_form` is the classified form of the input when it is safe to echo back (e.g. the Bech32m-encoded shielded address itself), so the UI can render "Shielded address: zs1... — no public history." For viewing keys, `canonical_form` is omitted because echoing a viewing key in the response would be a privacy regression even when the explorer never persists it.

### ZIP-316 unified addresses classify per receiver

```proto
message UnifiedAddressMatch {
  string canonical_form = 1;             // the original Bech32m-encoded UA
  string network = 2;                    // "zcash-mainnet" / "zcash-testnet" / "zcash-regtest"
  repeated UnifiedAddressReceiver receivers = 3;
}

message UnifiedAddressReceiver {
  UnifiedAddressReceiverKind kind = 1;
  oneof body {
    TransparentAddressMatch transparent = 2;
    TexAddressMatch tex = 3;             // not yet defined inside UAs by ZIPs, but reserved
    NotPubliclyIndexable shielded = 4;
  }
}

enum UnifiedAddressReceiverKind {
  UNIFIED_ADDRESS_RECEIVER_KIND_UNSPECIFIED = 0;
  UNIFIED_ADDRESS_RECEIVER_KIND_P2PKH = 1;
  UNIFIED_ADDRESS_RECEIVER_KIND_P2SH = 2;
  UNIFIED_ADDRESS_RECEIVER_KIND_SAPLING = 3;
  UNIFIED_ADDRESS_RECEIVER_KIND_ORCHARD = 4;
  UNIFIED_ADDRESS_RECEIVER_KIND_UNKNOWN = 5;
}
```

The classifier decodes ZIP-316 typecodes and surfaces each receiver as its own match. Transparent receivers (P2PKH, P2SH) populate the `transparent` arm with the standard `TransparentAddressMatch`. Shielded receivers (Sapling, Orchard) populate the `shielded` arm with `NotPubliclyIndexable { reason: NOT_PUBLICLY_INDEXABLE_SHIELDED_RECEIVER_IN_UNIFIED, ... }`. Unknown future typecodes populate `UNIFIED_ADDRESS_RECEIVER_KIND_UNKNOWN` with a `NotPubliclyIndexable` body referencing `UnavailableReason::UNAVAILABLE_PARSER_VERSION_UNSUPPORTED`.

### ZIP-320 TEX addresses classify as transparent-source-only

```proto
message TexAddressMatch {
  string canonical_tex_form = 1;        // "tex1..." or "textest1..."
  string equivalent_p2pkh_form = 2;     // "t1..." or "tm..." matching the underlying P2PKH hash
  TransparentAddressMatch transparent = 3;  // the routable P2PKH match for history
  bool transparent_source_only = 4;     // always true
  string spend_side_note = 5;           // canonical reason: "TEX address — transparent inputs only"
}
```

The classifier decodes the Bech32m `tex`/`textest` HRP, extracts the 20-byte P2PKH key hash, and re-encodes both as `tex1...` (canonical) and `t1.../tm...` (equivalent). The `transparent` arm carries the routable `TransparentAddressMatch`, so the UI can link directly to the transparent-address page. `transparent_source_only` and `spend_side_note` explain the semantic difference (per ZIP-320, the address restricts the *sender* to transparent inputs, but on-chain the output is indistinguishable from the underlying P2PKH).

### The classifier is local; no derive consumer required

`ExplorerQuery.Search` runs in the explorer service handler directly. It decodes the input string with:

- Numeric → `BlockMatch` candidate at that height.
- Hex 64-byte → both `BlockMatch` and `TransactionMatch` candidates, resolved against canonical artifacts.
- Base58check `t*`/`tm*` → `TransparentAddressMatch` candidate.
- Base58check `2*`/`zc*` Sapling-z → `ShieldedAddressMatch { NotPubliclyIndexable }`.
- Bech32m `u*`/`utest*`/`uregtest*` → `UnifiedAddressMatch` with per-receiver classification.
- Bech32m `tex*`/`textest*` → `TexAddressMatch`.
- Bech32m `uivk*`/`uvf*`/`zviews*` → `ViewingKeyMatch { NotPubliclyIndexable }`.
- Anything else → `UnclassifiedMatch` with a hint string ("could not classify; expected block height, transaction id, or supported address form").

The classifier never opens canonical storage for shielded inputs; the refusal is computed from the input string alone. For non-shielded inputs (transparent addresses, transaction IDs), the classifier may issue lookups against `WalletQuery.Transaction` and `WalletQuery.TransparentAddressBalance` to confirm the entity exists. Lookups against shielded receivers are forbidden by structural invariant: the classifier short-circuits before any storage call.

A `SearchIndexConsumer` derive view (deferred to a later slice) may pre-build sublinear address-prefix lookups for autocomplete; the consumer is optional and does not gate the `explorer.search.v1` capability.

## Consequences

### Operational

- The search RPC has bounded per-request cost: input classification is O(length-of-string); canonical lookups are O(1) per candidate. There is no fan-out or scan against the canonical store from a single search request.
- Operators get one new ops metric `zinder_explorer_search_classifications_total{kind="..."}` counting classifications by arm. This surfaces "users are pasting viewing keys" or "users are searching unified addresses with unknown typecodes" without needing log scraping.

### Implementation

- `crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto` adds the `SearchRequest`, `SearchResponse`, `SearchCandidate` shapes and all the per-arm bodies.
- `crates/zinder-core/src/explorer_search.rs` is new; it owns the classification logic, has zero dependencies on storage, and is unit-testable from input strings alone.
- `services/zinder-explorer/src/grpc/search.rs` is new; it composes the classifier with `WalletQueryClient` lookups for confirmation.
- `crates/zinder-core/src/explorer_reasons.rs` (defined in [ADR-0011](0011-explorer-freshness-envelope.md)) adds any new reason strings needed for the search vocabulary.

### Testing

- Classifier unit tests in `crates/zinder-core/tests/integration/explorer_search.rs` cover every input class with positive and negative cases. The classifier has no IO, so the test suite is fast and deterministic.
- Integration tests in `services/zinder-explorer/tests/integration/search.rs` assert the full RPC path against a mocked `WalletQuery` for the lookup confirmation step.
- A privacy regression test asserts that a search for any shielded address or viewing key never reaches a storage read: it instruments the mock `WalletQueryClient` to record calls and asserts the call count is zero.
- Live tests against regtest exercise mined-block and known-transaction lookups end to end.

## Alternatives Considered

### Return an empty match list for shielded inputs

Rejected. "No match" tells the user "you may have mistyped or the entity does not exist on this chain." The truth is "this entity exists by construction and has no public history." Collapsing the two answers is the wrong UX and the wrong privacy posture.

### Refuse with gRPC error instead of a typed match arm

Rejected. The PRD requires search to return "typed entity candidates, not only direct matches." A search for "zs1ABCD..." (shielded address) needs to surface "you searched a shielded address" alongside any other candidates (e.g. if the same prefix matches a non-shielded entity). Returning a gRPC error short-circuits the response and loses the structured refusal. The typed `NotPubliclyIndexable` arm preserves the search response shape while making the refusal explicit.

### Embed UA receivers as separate top-level candidates

Rejected. A unified address is one input that decomposes into multiple receivers; treating each receiver as a top-level candidate loses the "these belong to one UA" relationship. The `UnifiedAddressMatch.receivers` repeated field preserves the grouping, and clients that want to render per-receiver chips can iterate the array.

### Persist a search index in the canonical store

Rejected. The classifier is stateless and the canonical store already has every lookup the search needs (by-hash for blocks, by-id for transactions, by-script-hash for transparent addresses). A separate search index would duplicate data and create a second source of truth for the same lookups.

### Echo viewing keys in `canonical_form`

Rejected. Even though the explorer never persists the key, echoing it on the response surface is a privacy regression: any logging layer between the client and the explorer would see the key. The `canonical_form` field is omitted for `ViewingKeyMatch` by design.

## Out of Scope

- Payment disclosure payload verification. The PRD defers it as a future stateless tool; the search arm `PaymentDisclosureMatch` is reserved for that future work but is not implemented in this slice.
- Autocomplete suggestions. The classifier returns explicit candidates per input; the UI implements autocomplete locally.
- Cross-chain search (other Zcash forks). The classifier is Zcash-specific.
- ZIP-321 payment URI parsing. A URI like `zcash:address?amount=...` is a different concept (a payment request, not a search target); a future RPC may parse URIs but not through the search endpoint.
