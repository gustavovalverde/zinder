# ADR-0021: `chain_events.address_filter` as Address-Invalidation Hint

## Context

`chain_events` is the canonical low-latency push channel for confirmed transparent activity. Per-address consumers (faucets, payment receivers) need an optional `address_filter` on `ChainEventsRequest` so they do not pay the bandwidth cost of unrelated events.

The canonical chain-event stream defined by [Chain events §Canonical Event Boundary](../architecture/chain-events.md#canonical-event-boundary) emits only two variants: `ChainCommitted` (per epoch advance) and `ChainReorged` (per non-finalized replacement). There are no per-address event variants today; an address watcher subscribing to `chain_events` receives one envelope per chain transition, not one per address activity.

Three routes considered:

| Route | Semantics | Cost |
| ----- | --------- | ---- |
| A. New event variants `TransparentOutputCreated`/`Spent` | Per-address activity becomes a first-class event | Breaks the Reth-style canonical-event shape; bloats retention; needs a new cursor family |
| B. Server-side per-address envelope projection | One envelope per matching address per commit | Per-request CPU on the query plane; breaks cursor opacity |
| C. Filter-by-touch (this ADR) | `address_filter` narrows which `ChainCommitted` envelopes are delivered; `ChainReorged` always passes through | Server probes M4 transparent-address tx-history index; cursor stays opaque |

## Decision

**Route C.** `ChainEventsRequest.address_filter` is an **invalidation-hint filter**, not an event-stream demultiplexer:

- Empty filter (`address_filter: []`): server delivers every envelope, as before.
- Non-empty filter: server delivers each `ChainCommitted` envelope only when at least one filter address has activity in the committed block range, as observed through the M4 transparent-address tx-history index. `ChainReorged` envelopes always pass through — clients must invalidate cached derivations after a reorg regardless of which addresses they were watching.
- Cursor opacity is preserved: the cursor bytes the client persists are the cursor the server would have emitted without a filter. Resuming with a different filter after the same cursor produces an envelope set consistent with the new filter, applied from the cursor forward.

**Canonical consumer pattern:** *snapshot once, subscribe forever, re-derive on hint.*

1. Client snapshots per-address state via `transparent_address_utxos_stream` (paginated).
2. Client subscribes to `chain_events` with `address_filter: [<watched_address>, ...]`.
3. On every received envelope, the client re-derives its per-address state from the committed compact block it already fetches via `compact_block_at`. The envelope tells the client *when* to re-derive; the compact block tells it *what* to derive.

**Implementation:**

- `address_filter` parsing uses the same `address_lookup_to_script_hash` helper as every other transparent-address-accepting RPC; canonical Base58 t-addresses with network validation.
- `MAX_CHAIN_EVENTS_ADDRESS_FILTER = 256` caps the per-request fanout.
- The touch probe is a one-page `transparent_address_tx_ids_in_range` query with `max_entries = 1`: returns true on the first artifact, never reads the full index.
- The `chain_events` handler is extracted from `services/zinder-query/src/grpc/adapter.rs` to a new `services/zinder-query/src/grpc/chain_events.rs` module so the filter logic has a clear owner.

## Consequences

**Per-address consumers pay only for relevant epochs.** A faucet watching one address receives roughly one envelope per block whose committed range touches that address; quiet blocks pass without delivery.

**Reorgs are unconditional.** A client cannot opt out of reorg envelopes by filtering. This is the right contract: a reorg invalidates every derivation, not only those at the new tip's range; clients that drop reorgs because their watched addresses appear unchanged will hold stale derivations on the previous chain.

**The M4 transparent-address tx-history index only covers outputs.** Touch detection finds blocks where one of the filtered addresses *received* funds; it does not detect blocks where one of the filtered addresses *spent* funds. A consumer that depends on detecting spends in real time must subscribe without a filter (or with both the receiving address and the addresses whose UTXOs it cares about). The known M4 follow-up (spending-side history indexing) closes this gap; until then, the limitation is documented in `chain-events.md` and `wallet-data-plane.md`.

**Cursor opacity stays.** The server emits the same cursor bytes regardless of filter. Clients can store the cursor and replay with a different filter (or no filter at all). This preserves the "the cursor is opaque bytes the client persists" contract from [ADR-0005](0005-chain-event-cursor-sequence.md).

**No new capability string.** The semantics of `wallet.events.chain_v1` extend additively. Capability detection still happens through `ServerInfo`; the filter is a server feature that does not change which envelopes a client *might* receive, only which ones it *will* receive.

**Per-envelope work is bounded.** O(filter_size) touch probes per envelope, each one a single-key RocksDB seek with a bounded scan. Filter cap of 256 addresses keeps even the worst case well within the per-stream backpressure budget.

## Alternatives Considered

**Route A (new event variants):** Rejected. The canonical event shape is committed/reverted; adding per-address variants pollutes a stream that derived consumers also subscribe to (the derive plane reads the same envelopes to drive its overlays). The cost would propagate to every consumer.

**Route B (per-address envelope projection):** Rejected. Server emits N envelopes per commit per address in the filter. Cursor semantics become per-`(stream, address-set)` — the server must remember which client requested which addresses to deduplicate on resume, breaking the "cursor is opaque bytes the client owns" contract.

**Pre-deriving address-touch sets at commit time:** Considered. The ingest writer could maintain an inverted index mapping `(network, height_range, address)` for fast lookup. The cost is durable storage growth proportional to the active address universe, plus added work on every commit. The M4 transparent-address tx-history index already encodes the same information per address; using it directly avoids a duplicate inverted index without measurable cost on realistic workloads (where filter sizes are tens, not millions).

## References

- [Chain events §Canonical Event Boundary](../architecture/chain-events.md#canonical-event-boundary)
- [ADR-0005: Chain event cursor sequence](0005-chain-event-cursor-sequence.md)
- [`crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto`](../../crates/zinder-proto/proto/zinder/v1/wallet/wallet.proto) (`ChainEventsRequest.address_filter`)
- [`services/zinder-query/src/grpc/chain_events.rs`](../../services/zinder-query/src/grpc/chain_events.rs)
