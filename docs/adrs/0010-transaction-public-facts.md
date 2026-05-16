# ADR-0010: Transaction Public Facts As The Single Transaction Parser

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Transaction parsing, explorer wire surface, ingest pipeline, mempool pipeline |
| Related | [ADR-0008](0008-network-parameter-discovery.md), [ADR-0009](0009-explorer-plane-as-product-surface.md), [Explorer plane](../architecture/explorer-plane.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Public interfaces §ZIP cross-reference](../architecture/public-interfaces.md#zip-cross-reference) |

## Context

Three independent transaction parsers exist in the workspace today:

- Ingest block parsing in `services/zinder-ingest/src/artifact_builder.rs` (parses a whole `ZebraBlock`, iterates `parsed_block.transactions`, extracts vouts for `TransparentAddressUtxoArtifact` and vins for `TransparentUtxoSpendArtifact`).
- Mempool hydration in `services/zinder-ingest/src/mempool/entry.rs` (parses one `ZebraTransaction`, extracts outputs and inputs for `MempoolEntry` overlays).
- M6 prevout resolution in `crates/zinder-source/src/source_transaction.rs` (parses stored `payload_bytes` at read time, extracts one output by index).

None of them extracts transaction version, lock time, expiry height, consensus branch ID, sapling/orchard/sprout component counts, fee, privacy shape, or auth digest for mined transactions. Each parser calls `zcash_deserialize_into::<ZebraTransaction>` independently and pulls only what its single caller needs.

The explorer plane's `TransactionDetail` view needs all of those facts. Adding a fourth parser duplicates state in a way that fights the codebase's "fight entropy" discipline. The right boundary is one parser that produces a single typed value covering every public fact, called once per transaction at each consumption site.

The decisions to record here are:

1. The typed value is `TransactionPublicFacts`, owned by `zinder-core`.
2. The parser lives in `zinder-source` and takes raw bytes plus the network upgrade activations table.
3. Privacy-shape classification is a pure function on the count fields, not a separate parse.
4. Fee is tri-state: computable (with prevout resolution), unavailable with reason, or unsupported (for future tx versions the parser cannot fully decode).
5. The first read-path consumer (`ExplorerQuery.TransactionDetail`) uses Shape C compute-at-read-time per [ADR-0014 pattern documented in extending-the-wallet-data-plane.md](../architecture/extending-the-wallet-data-plane.md). Promotion to Shape A (a `TransactionFactsArtifact` family) is a non-breaking optimization and is deferred until profiling justifies it.

## Decision

### `TransactionPublicFacts` is the single typed shape

`crates/zinder-core/src/transaction_public_facts.rs` owns the type:

```rust
pub struct TransactionPublicFacts {
    // Identity
    pub transaction_id: TransactionId,
    pub auth_digest: Option<AuthDigest>,    // v5+; None for v1-v4
    pub wtxid: Option<Wtxid>,                // v5+; None for v1-v4 where it equals txid
    // Header
    pub version: TransactionVersion,
    pub consensus_branch_id: Option<ConsensusBranchId>,
    pub lock_time: LockTime,
    pub expiry_height: Option<BlockHeight>,
    pub size_bytes: u32,
    // Component counts (privacy-shape inputs)
    pub counts: TransactionComponentCounts,
    // Classification
    pub privacy_shape: PrivacyShape,
    pub is_coinbase: bool,
    // Forward-compatibility
    pub unsupported_sections: Vec<UnsupportedSection>,
}

pub enum TransactionVersion {
    V1, V2, V3, V4, V5,
    Unsupported { effective_version: u32, version_group_id: Option<u32> },
}

pub enum PrivacyShape {
    TransparentOnly,
    Shielding,        // transparent in, shielded out
    Deshielding,      // shielded in, transparent out
    ShieldedOnly,
    Mixed,
    Coinbase,
    ShieldedCoinbase, // ZIP-213
    Unclassified,
}
```

The struct and its enums are plain (not `#[non_exhaustive]`). The `Unsupported`/`Unclassified` variants are the extension points for future Zcash protocol additions: a hypothetical v6 lands as `TransactionVersion::Unsupported { effective_version: 6, .. }` and degrades to `PrivacyShape::Unclassified`; no cross-crate matcher needs to add a wildcard arm. Adding a new struct field is a breaking change for external constructors by design, which keeps the construction surface honest. `UnsupportedSection` stays `#[non_exhaustive]` because new section kinds may add variants without forcing constructor changes.

### The parser is a single helper in `zinder-source`

`crates/zinder-source/src/source_transaction.rs` exports:

```rust
pub fn parse_transaction_public_facts(
    raw_transaction_bytes: &[u8],
    mined_height: Option<BlockHeight>,
    activations: &NetworkUpgradeActivations,
) -> Result<TransactionPublicFacts, SourceError>
```

The parser is pure: input bytes + optional mined height + activations → struct. It uses `zebra-chain`'s accessors directly (`tx.network_upgrade()`, `tx.lock_time()`, `tx.expiry_height()`, `tx.auth_digest()`, `tx.is_coinbase()`, `WtxId::from(tx)`, etc.). The accessors live in `zebra_chain::transaction::Transaction` at version 6.0.2; no upstream gap.

The activations argument carries [ADR-0008](0008-network-parameter-discovery.md)'s node-discovered consensus branch ID table. The parser uses it to fill `consensus_branch_id` for v3/v4 transactions whose header omits the upgrade tag: `consensus_branch_id_at(mined_height)` resolves the branch ID from the activations table when the transaction is mined. Mempool v3/v4 transactions surface `consensus_branch_id: None` because no mined height is available to anchor the resolution. v5+ transactions carry the upgrade tag in the header and the parser reads it from `tx.network_upgrade()?.branch_id()` regardless of mined height. v1/v2 transactions are pre-Overwinter and have no branch ID.

The explorer plane lacks a node-side view of the activations table today and constructs `NetworkUpgradeActivations::empty(network)` for the call. For mined transactions the explorer overrides the parser's result with `MinedDetails.consensus_branch_id` from the wallet response (which is authoritative because it came from the canonical commit). Mempool v3/v4 transactions therefore show `consensus_branch_id: None` until the explorer plane federates the activations table.

### Three call sites consume the parser

- **Explorer read path** (`services/zinder-explorer/src/grpc/transaction_detail.rs`): _shipped._ Parses on demand from the canonical `TransactionArtifact.payload_bytes` returned by `WalletQuery.Transaction`. Each request parses once.
- **Ingest block path** (`services/zinder-ingest/src/artifact_builder.rs`): _follow-up._ Will replace the ad-hoc `transaction.outputs()`/`transaction.inputs()` calls. The parser runs once per transaction during block processing; the existing compact-tx builder will consume the same parsed `ZebraTransaction` to avoid a second parse.
- **Mempool hydration** (`services/zinder-ingest/src/mempool/entry.rs`): _follow-up._ Will replace the second `zcash_deserialize_into` call. `MempoolEntry` will gain a `public_facts: TransactionPublicFacts` field.

### Privacy-shape classification is pure

```rust
pub fn classify_privacy_shape(facts: &TransactionPublicFacts) -> PrivacyShape;
```

The classifier reads only the count fields and `is_coinbase`. It does not look at scripts, values, or shielded encryption blobs. The classifier is unit-testable from synthetic count tuples; the parser builds the counts.

The classification rules:

- `is_coinbase` and `(sapling + orchard + sprout) == 0` → `Coinbase`
- `is_coinbase` and `(sapling + orchard) > 0` → `ShieldedCoinbase` (ZIP-213)
- non-coinbase, transparent-only on both sides → `TransparentOnly`
- non-coinbase, transparent in + shielded out, no shielded in → `Shielding`
- non-coinbase, shielded in + transparent out, no shielded out → `Deshielding`
- non-coinbase, shielded on both sides, no transparent → `ShieldedOnly`
- non-coinbase, mixed (transparent and shielded on both sides) → `Mixed`
- fallback (parser could not classify a version) → `Unclassified`

### Fee is tri-state

Fee is never zero by default. The explorer wire response carries `FeeFacts` as a oneof:

```proto
message FeeFacts {
  oneof fee {
    FeeComputed computed = 1;          // value_zat, conventional_zat, zip317_logical_actions
    FeeUnavailable unavailable = 2;     // reason (e.g. prevouts not resolved)
    FeeUnsupported unsupported = 3;     // tx version the parser cannot fully decode
  }
}
```

A response that returns `FeeFacts::Computed { value_zat: 0 }` truthfully reports a zero-fee transaction (rare but legitimate in early Zcash). A response that does not know the fee returns `FeeFacts::Unavailable { reason }` with a structured reason code the UI can render.

Fee computation requires prevout resolution for all transparent inputs. The explorer's `TransactionDetail` handler invokes the existing `WalletQuery.TransparentPrevouts` (canonical) and `WalletQuery.TransparentMempoolPrevouts` (mempool) endpoints. When a prevout is unavailable, the response carries `FeeFacts::Unavailable { reason: PREVOUTS_MISSING }`.

### Shape C now; Shape A promotion is non-breaking

The first read-path consumer (`ExplorerQuery.TransactionDetail`) reads `TransactionArtifact.payload_bytes` from the canonical store and parses on demand. No new column family. No new artifact family. The parse cost is one `zcash_deserialize_into::<ZebraTransaction>` per request, deduplicated within a batch.

When load profiling shows the per-tx parse is hot, the promotion path is:

1. Add `TransactionFactsArtifact` family with `kind = 11`, `PayloadFormat::ZinderTransactionFactsArtifactV1 = 9`.
2. Populate at ingest from the same `parse_transaction_public_facts` call already running for ingest-side fact extraction.
3. The read handler prefers the materialized artifact when present, falls back to parse-on-demand when absent.
4. The wire shape and capability string do not change. The promotion is invisible to clients.

The promotion is explicitly non-breaking by design: `TransactionPublicFacts` is the durable contract; Shape A vs Shape C is a storage-tier detail.

### Auth digest for mined transactions

`TransactionArtifact` does not currently carry `auth_digest`. Two choices:

- **Compute on demand from `payload_bytes`** (Shape C): the parser's `tx.auth_digest()` call returns `Some(AuthDigest)` for v5+, `None` for v1–v4. The cost is one `zcash_deserialize_into` + one `auth_digest()` call. Acceptable for explorer-tier latency.
- **Pre-extract into `TransactionArtifact` v2** (Shape A): bump artifact schema version, add `auth_digest: Option<AuthDigest>` field, populate at ingest. Saves the read-time parse but is a breaking artifact-schema change.

This ADR takes Shape C now. Shape A promotion (with the artifact schema bump) is deferred to the same trigger as the broader `TransactionFactsArtifact` decision.

## Consequences

### Operational

- The three existing parsers' call sites collapse to one. Future changes to transaction parsing (NU7 transaction v6, ZSA, memo bundles, explicit fees) land in one place.
- Memory profile is unchanged: the parser produces a flat struct of `u32`s, `Option`s, and small enums. Per-transaction cost is dominated by the existing `zcash_deserialize_into` call, not the field extraction.

### Implementation

- `crates/zinder-core/src/transaction_public_facts.rs` adds the typed shape.
- `crates/zinder-source/src/source_transaction.rs` extends with `parse_transaction_public_facts`.
- `services/zinder-ingest/src/artifact_builder.rs` and `services/zinder-ingest/src/mempool/entry.rs` migrate to the shared parser.
- `MempoolEntry` (in `zinder-core`) gains `public_facts: TransactionPublicFacts`. The `MempoolEntryRecord` prost message in `zinder-store/src/format/artifact_codec.rs` gains the encoded shape. This is a breaking change to `MempoolEntry`; the persistent column family `mempool_event` is rebuilt on writer restart from the source snapshot so a schema bump is observable but recoverable.
- `services/zinder-explorer/src/grpc/transaction_detail.rs` is new; it pulls `TransactionArtifact` via the wallet plane's `WalletQuery.Transaction`, calls `parse_transaction_public_facts`, fetches prevouts as needed, and constructs the `TransactionDetailResponse`.
- `crates/zinder-proto/proto/zinder/v1/explorer/explorer.proto` adds `TransactionPublicFacts`, `TransactionDetail*`, `FeeFacts`, `TransparentInputsAndOutputs`, `ShieldedSummary` messages.

### Testing

- Parser fixture suite in `crates/zinder-core/tests/integration/transaction_public_facts.rs` covers (privacy shape × transaction version) matrix: transparent-only v4/v5, shielding v4/v5, deshielding v4/v5, shielded-only v4/v5, mixed v4/v5, transparent coinbase v4/v5, ZIP-213 shielded coinbase, and a future-version placeholder byte sequence that asserts `unsupported_sections` is populated.
- Each fixture asserts the full `TransactionPublicFacts` output against a hand-rolled expected value.
- The classifier unit tests live alongside the type definition and exercise the classification rules from synthetic count tuples; they never call into the parser.
- `services/zinder-explorer/tests/integration/transaction_detail.rs` end-to-end tests serve `TransactionDetail` for known mined and mempool transactions and assert every field.
- Live regtest + testnet tests under `services/zinder-explorer/tests/live/` exercise the full path against running Zebra instances.

## Alternatives Considered

### Keep the three parsers and accept the duplication

Rejected. Each parser pulls a slightly different subset; the next "what does field X look like" question forces a fourth parser. The duplication is already a known source of subtle bugs (e.g. coinbase detection is sentinel-pattern-only because no parser exposes `tx.is_coinbase()`).

### Put the parser inside `zinder-core` instead of `zinder-source`

Rejected. The parser depends on `zebra-chain` for deserialization; placing it in `zinder-core` would force every consumer of `zinder-core` to pull `zebra-chain`. `zinder-source` already depends on `zebra-chain` and already owns transaction-byte parsing helpers; the boundary is consistent.

### Store the full `TransactionPublicFacts` in `TransactionArtifact` at ingest

Rejected for the initial slice. Storing the parsed facts in canonical storage requires an artifact schema bump, increases the canonical store size, and prematurely optimizes a parse cost that has not been measured. Shape C compute-at-read is the documented default for facts that can be extracted from existing payload bytes; the promotion path to Shape A is non-breaking and tied to load profiling.

### Use `zcash_primitives` instead of `zebra-chain` for parsing

Rejected. `zcash_primitives` is already a `[dev-dependencies]` for `zinder-testkit` only and pulling it into production code carries circuit/prover features Zinder does not need. `zebra-chain` is already in the production dependency tree, already used by the three existing parsers, and exposes every accessor the explorer needs.

### Compute wtxid manually instead of using `WtxId::from(&tx)`

Rejected. zebra-chain ships `WtxId` directly: it is the 64-byte concatenation of `txid || auth_digest` for v5+ per ZIP-239. The `From<&Transaction> for WtxId` impl panics on pre-v5; the parser guards with `match tx.version() { 5 => Some(WtxId::from(&tx)), _ => None }`.

## Out of Scope

- A persistent `TransactionFactsArtifact` column family. Deferred until profiling justifies the Shape A promotion.
- ZIP-244 txid recomputation from parsed sub-digests. The parser trusts `tx.hash()` from zebra-chain, which routes correctly to SHA256d for pre-v5 and BLAKE2b ZIP-244 `txid_digest` for v5+.
- Memo decryption, viewing-key-based scanning, or any field that depends on a private key. Out of scope by product invariant.
- Fee computation for shielded value flows. The `value_zat` field reflects transparent value movement only; shielded value balances are surfaced as part of `ShieldedSummary` counts without re-accounting consensus invariants. ZIP-209 chain value pool tracking is a separate explorer view (ADR pending).
