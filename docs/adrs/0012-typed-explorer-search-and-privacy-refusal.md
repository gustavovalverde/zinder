# ADR-0012: Typed Explorer Search And Privacy Refusal

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Explorer search surface, address classification, privacy boundary |
| Related | [ADR-0005](0005-consumer-neutral-wallet-data-plane.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [ADR-0011](0011-explorer-freshness-envelope.md), [Explorer plane](../architecture/explorer-plane.md) |

## Context

Explorer search accepts public chain identifiers and address forms. A single match-or-empty response cannot distinguish malformed input, an absent public entity, and an input whose history is private by protocol design. In particular, returning an empty result for a shielded receiver incorrectly implies that no history exists rather than that the history is not public.

The explorer never scans shielded receivers, persists viewing keys, exposes shielded-address history, or decrypts memos. Search must uphold that boundary while still giving clients a typed answer they can render.

## Decision

`ExplorerQuery.Search` returns a `SearchResponse` containing typed `SearchCandidate` values. A candidate has one explicit arm for a block, transaction, transparent address, TEX address, unified address, shielded address, unified viewing key, or unclassified input. `confidence` is a rendering hint rather than a compatibility promise.

```proto
message SearchCandidate {
  oneof match {
    BlockMatch block = 1;
    TransactionMatch transaction = 2;
    TransparentAddressMatch transparent_address = 3;
    TexAddressMatch tex_address = 4;
    UnifiedAddressMatch unified_address = 5;
    ShieldedAddressMatch shielded_address = 6;
    ViewingKeyMatch viewing_key = 7;
    UnclassifiedMatch unclassified = 9;
  }
  float confidence = 10;
}
```

Shielded addresses, unified viewing keys, and shielded receivers within a unified address produce `NotPubliclyIndexable`. This is a successful, typed privacy refusal, not a gRPC error. The response carries a structured reason and a canonical human-readable reason from `zinder-core`; it only echoes a canonical form when doing so is safe. Viewing keys are never echoed.

ZIP-316 unified addresses are classified receiver by receiver. P2PKH and P2SH receivers are routable through transparent-address history. Sapling, Orchard, and unknown receivers use the same typed refusal shape. ZIP-320 TEX addresses classify as transparent-source-only and expose the equivalent P2PKH form for public history.

The classifier runs locally. It may confirm public block, transaction, and transparent-address candidates through the wallet-facing read boundary, but it never opens storage for shielded inputs. This structural short circuit is the privacy invariant.

## Consequences

- Clients can distinguish absent public history from non-public history and render each honestly.
- Search remains bounded: classification is proportional to input length, with only public candidates eligible for point lookups.
- The `explorer.search_v1` capability does not require a materialized-view store.
- Metrics count classifications by candidate kind without recording sensitive input values.

## Alternatives Considered

### Empty results for shielded inputs

Rejected. Empty results conflate a private-history boundary with a malformed or absent entity.

### gRPC errors for shielded inputs

Rejected. A typed candidate preserves the search response shape and lets a client explain the refusal without making the request fail.

### Persisted search index

Rejected. The classifier is stateless and public point lookups already use canonical indexes. A second index would duplicate state without changing the privacy boundary.

## Out of Scope

- Autocomplete suggestions.
- Cross-chain search for other Zcash forks.
- ZIP-321 payment URI parsing; a payment request is not a search target.
