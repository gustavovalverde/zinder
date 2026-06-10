# Fact-First Indexer

Status: Current architecture

Zinder is a fact-first Zcash indexer. The canonical store keeps typed public
facts in hot tables, keeps raw payloads only as optional blob artifacts, and
feeds rebuildable product projections through durable event streams.

This document owns the storage vocabulary, ingest shape, derive-tailer
boundary, source-boundary expectations, and public naming rules for that model.

## Boundary Model

Zinder is split into three durable planes.

| Plane | Owns | Must not own |
| --- | --- | --- |
| Canonical index plane | Epoch-consistent block identity, transaction location, transaction facts, transparent output facts, compact wallet artifacts, tree state, subtree roots, mempool events, chain events | Explorer summaries, analytics views, per-consumer state |
| Derive plane | Rebuildable explorer and analytics projections fed by canonical events | Canonical truth, wallet correctness, source-node RPCs |
| Query planes | Wallet-shaped and explorer-shaped gRPC surfaces over canonical and derived facts | Storage migrations, background projection, chain ingestion |

The canonical plane may duplicate raw payloads as cold blobs for explicit raw
export. Raw blobs are not the source of block headers, transaction locations,
transaction detail, fee facts, transparent address activity, or derive
projections.

## Canonical Store

Canonical storage is fact-first. Hot reads use typed rows instead of reparsing
block bytes, transaction bytes, or compact-block payloads.

| Table | Hot key | Value | Purpose |
| --- | --- | --- | --- |
| `block_header` | `(network, height)` | block hash, parent hash, time, header commitment fields, size bytes | Header and block identity reads without raw block parsing |
| `block_transaction_index` | `(network, height, tx_index)` | transaction id | Canonical transaction order |
| `transaction_location` | `(network, txid)` | height, tx index, block hash | Direct transaction lookup without compact-block decoding |
| `transaction_facts` | `(network, txid)` | public transaction facts, component counts, size, auth digest, privacy shape, fee inputs that do not require private data | Transaction detail, search, recent transactions, fee summaries |
| `transparent_output` | `(network, outpoint)` | value, script pubkey, address script hash, produced height, produced block hash | Single canonical transparent output fact |
| `address_output_index` | `(network, address_script_hash, height, outpoint)` | address output row | Current unspent-output projection per address; finalized-spent rows are deleted by the safe-tip retention sweep |
| `transparent_spend_fact` | `(network, spent_outpoint)` | spending txid, input index, spending block, spent value, spent address script hash, spent block | Spend lookup, derive replay, and reorg repair |
| `compact_block` | `(network, height)` | encoded lightwalletd compact block | Wallet sync cache |
| `tree_state` | `(network, height)` | source tree-state payload or typed tree state | Wallet scan boundary |
| `subtree_root` | `(network, pool, start_index)` | completed subtree root | Wallet scan acceleration |
| `chain_event` | `(network, event_sequence)` | committed, reverted, safe-tip event envelope | Source of truth for derive tailers and chain subscriptions |
| `mempool_event` | `(network, event_sequence)` | mempool change envelope | Source of truth for mempool readers and mempool-derived projections |
| `block_blob` | `(network, height)` | compressed raw block bytes | Explicit raw block export and rebuild aid |
| `transaction_blob` | `(network, txid)` | raw transaction bytes | Explicit raw transaction export |

Transparent-output naming describes the row at creation time. A spend is one
consumer of that row, not the row identity. Public APIs may use `prevout` only
for transaction-input-facing request fields. Storage, core types, and canonical
code use `transparent_output`.

`compact_block` is never used to infer transaction order. Transaction-at-location
reads use `block_transaction_index` and `transaction_location`.

Transparent-address transaction history is a derive-plane projection over
`transaction_facts`, `transparent_output`, and `transparent_spend_fact`.
Keeping that projection off the canonical writer prevents explorer views from
throttling wallet sync and canonical catchup.

`block_blob` and `transaction_blob` are optional cold paths. Wallet sync,
explorer transaction detail, search, address history, and derive projections
must not depend on raw blob presence.

## Ingest Pipeline

Canonical ingest turns ordered source updates into atomic `ChainEpoch` commits.
It does not run product projections.

```text
SourceChainUpdate stream
  -> build canonical facts
  -> commit ChainEpoch
  -> append ChainEvent
  -> notify derive tailer
```

Implementation rules:

- `build_canonical_facts` parses each block once and emits the canonical rows
  listed above.
- `commit_chain_epoch` writes canonical rows and the chain event in one
  visible epoch transition.
- Batch limits are based on durable pressure units: block count, raw source
  bytes, transaction count, logical actions, transparent outputs, transparent
  inputs, compact outputs/actions, and write-batch bytes.
- Explicit RocksDB flush is a storage-pressure control. Metrics report it as
  storage work.

Bulk catchup uses the resource-budgeted staged pipeline in
[ADR-0022](../adrs/0022-resource-budgeted-bulk-catchup.md). Tip-follow uses the
same canonical block-preparation path and commits one live-edge transition at a time.

## Derive Tailer

The derive plane is an asynchronous tailer over canonical events.

```text
chain_event cursor
  -> hydrate canonical facts for the event range
  -> apply derive consumers
  -> write derive rows and cursor atomically
```

Implementation rules:

- `zinder-ingest` hosts the derive tailer because it owns the primary derive
  store handle.
- The tailer runs independently from canonical commit.
- Startup replay and steady-state projection use the same chain-event cursor
  contract.
- Each derive consumer reads typed canonical facts, not raw block bytes.
- Per-consumer projection may parallelize block work inside one event range,
  but cursor advancement stays serial and atomic.
- A derive failure leaves canonical ingest healthy. Readiness and server-info
  surfaces report derive lag and capability freshness.
- `zinder-explorer` is a stateless secondary reader. It does not run derive
  consumers and does not open primary stores.

The derive cursor is the recovery contract. If canonical event `N` committed
and the derive cursor is still at `N - 1`, the tailer replays event `N`. If the
derive store is deleted, the tailer rebuilds from retained canonical events or
operator-provided canonical snapshots.

## Source Boundary

`NodeSource` is the only upstream-node boundary. The current catchup path uses
`fetch_chain_segment` with `SourceChainSegmentLimits` so the writer can size
source work by connected block count and response bytes.

Source adapters emit Zinder source values, not store rows. Zebra-specific JSON,
gRPC, and parser details stay inside `zinder-source`; canonical ingest receives
ordered source updates and maps them into the fact-first store.

Streaming source adapters must emit the same internal `SourceChainUpdate` shape
as the JSON-RPC adapter. That keeps the canonical store, query APIs, derive
tailer, and metrics independent of the transport used to observe the chain.

## Public APIs

The public API is split by user job, not by storage layout.

Wallet-facing APIs:

- compact block by height and compact block ranges
- tree-state checkpoints and latest tree state
- subtree roots
- chain events and mempool events
- transparent outputs by outpoint
- transparent address outputs
- transparent address transaction ids
- transaction status without raw bytes by default
- explicit raw transaction read when `transaction_blob` is enabled
- broadcast transaction

Explorer-facing APIs:

- block summary and block detail from `block_header`, `block_transaction_index`,
  and `transaction_facts`
- transaction detail from `transaction_facts`, `transaction_location`,
  `transparent_output`, and optional `transaction_blob`
- transparent address activity from derive rows over canonical facts
- recent transactions from derive rows or direct fact indexes
- explicit raw block and raw transaction reads only when blob capabilities are
  enabled

## Naming Rules

- Use `transparent_output` for storage, core types, and canonical code.
- Use `prevout` only in transaction-input-facing request names.
- Use `transaction_facts` for durable parsed public facts.
- Use `transaction_blob` and `block_blob` for raw bytes.
- Use `derive_tailer` for the asynchronous event consumer. Do not call it a
  dispatcher, manager, worker, or replay service.
- Use `canonical_lag_blocks` and `derive_lag_blocks` in readiness surfaces.

Capability rules:

- Wallet capability names stay under `wallet.read.*`, `wallet.mempool.*`, and
  `wallet.write.*`.
- Explorer capability names use `explorer.<domain>.<view>_vN`, for example
  `explorer.transaction.detail_v1` and `explorer.block.summary_v1`.
- Raw bytes are explicit capabilities, not fields quietly attached to typed
  reads.

## Metrics

Canonical ingest metrics:

- committed height, target height, and `canonical_lag_blocks`
- source fetch duration and bytes by source method
- block, transaction, logical-action, compact-output, compact-action, nullifier,
  transparent-input, and transparent-output counts per batch
- canonical write duration and write bytes by table
- RocksDB flush duration and flushed bytes by store
- batch close trigger by pressure unit

Derive tailer metrics:

- derive cursor height and `derive_lag_blocks`
- event hydration duration by source table
- per-consumer apply duration
- per-consumer rows staged and bytes staged
- derive-store write duration and write bytes
- event replay count and failure reason

Query metrics:

- request duration by service, method, status, and response item count
- raw blob reads separated from typed fact reads
- secondary catch-up duration and lag by store

## Validation Gates

Local validation:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
```

Storage validation:

- fresh stores start with the schema version for this architecture
- mismatched stores fail closed with a schema mismatch
- reorg replacement repairs `transparent_output` visibility and secondary
  indexes atomically
- pinned epoch reads never see rows from a later epoch
- raw blob absence disables only raw capabilities

Performance validation:

- canonical height advances while derive lag can trail
- typed transaction detail does not parse raw transaction bytes
- block header reads do not parse raw block bytes
- transaction-at-location reads do not decode compact block bytes
- write bytes by table show raw blobs separated from hot fact rows
- catchup reports pressure by bytes, actions, and rows, not only blocks

## External References

- [ECC sandblasting retrospective](https://electriccoin.co/blog/a-look-back-nu5-and-network-sandblasting/)
- [ZIP-307: Light Client Protocol for Payment Detection](https://zips.z.cash/zip-0307)
- [ZIP-317: Proportional Transfer Fee Mechanism](https://zips.z.cash/zip-0317)
- [ZIP-401: Addressing Mempool Denial-of-Service](https://zips.z.cash/zip-0401)
- [Zebra mempool specification](https://zebra.zfnd.org/dev/mempool-specification.html)
