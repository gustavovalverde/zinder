# ADR-0016: Drop the per-block `getblockhash` round trip on the JSON-RPC source

## Status

Accepted. The resource-budgeted catchup stage shape is owned by
[ADR-0022](0022-resource-budgeted-bulk-catchup.md).

## Context

Zinder's cold-start time and recovery-after-downtime are bounded by the
shape of the source-adapter fetch path, not by the storage layer, not by
Zinder CPU, and not by the network. A bulk-catchup run committing 1.92M
testnet blocks against a local Zebra in a sibling Docker container shows
where the time goes.

### Per-RPC latency averages

| RPC | Calls | Sum (s) | Avg latency |
| --- | --- | --- | --- |
| `z_gettreestate` | 1,921,758 | 29,518.88 | **15.36 ms** |
| `getblockheader` | 1,921,759 | 29,092.48 | 15.14 ms |
| `getblock` | 1,921,764 | 24,599.81 | 12.80 ms |
| `getblockhash` | 1,921,780 | 16,485.05 | **8.58 ms** |

### Per-batch commit (1,000-block batches)

| Stage | Sum (s) | Batches | Avg per batch | Per block |
| --- | --- | --- | --- | --- |
| `ingest_commit_duration` | 94.46 | 1,921 | 49.2 ms | **0.049 ms** |
| `store_write_batch_duration` | 55.02 | 1,921 | 28.6 ms | 0.029 ms |

RocksDB writes are two orders of magnitude faster than the upstream
RPCs. Storage is not the bottleneck.

### CPU utilisation

| Container | CPU |
|---|---|
| zinder-ingest (bulk catchup) | 33% of one core |
| z3-testnet-zebra | **224% (2.24 cores)** |

Zinder is mostly idle; Zebra is the throughput limit. The limit sits
inside Zebra's per-request handler path, and Zebra's `HashOrHeight::new`
accepts a numeric height string on each of the per-block lookups, so the
serial `getblockhash` call buys nothing.

### Per-block latency under the previous trait shape

Each block costs one serial round-trip plus one parallel triple:

```text
getblockhash       8.58 ms   (serial; blocks the parallel triple)
+ max(getblockheader 15.14 ms, getblock 12.80 ms, z_gettreestate 15.36 ms)
= 8.58 + 15.36
= ~24 ms per block of upstream-side latency
```

Dropping the serial call leaves just the parallel source reads and raises the
theoretical ceiling while the response-size regime is light. In dense eras,
`ingest.bulk_catchup.source_segment_max_blocks = 128` is only a hard ceiling: bulk
catch-up adapts the actual request size from observed source response bytes and
resets its density estimate at consensus-branch changes.

## Decision

`ZebraJsonRpcSource::fetch_block_at` keys `getblockheader`, `getblock`,
and `z_gettreestate` directly on the requested height string and drives
them through `tokio::join!`. The serial `getblockhash` round trip is
removed.

Cross-call agreement is the de-facto mid-flight reorg detector: the
parsed `getblock` hash is compared against `getblockheader.hash`,
`getblockheader.previousblockhash`, and the tree-state response's
`hash`. Disagreement surfaces as a typed error.

### New error

`SourceError::BlockReorgDuringFetch { height, reason }`.

Classified under `SourceFailureClass::UpstreamViewChanged`. Distinct
from `SourceProtocolMismatch` (a wire-contract violation: a broken
node) and from `BlockUnavailable` (a height that left the best chain
before any fetch landed). The long-running writer loop's recovery
primitive treats it as a re-observation signal under the same class as
`BlockUnavailable`, so no recovery-policy change is required.

### New core primitive

`BlockHeight::next() -> Option<Self>`. Returns the successor height or
`None` at `u32::MAX`. Consolidates five inline
`BlockHeight::new(h.value().saturating_add(1))` callers that previously
silently saturated at the chain ceiling; centralising the
ceiling-handling decision in one method removes a class of subtle
rollover bugs.

### Design space considered

Four shapes were evaluated end-to-end before settling on the minimal
change.

| Option | Throughput vs today | Upstream cooperation | Cross-host deployable | Verdict |
| --- | --- | --- | --- | --- |
| A: drop `getblockhash` on the JSON-RPC adapter | ~1.6–2.0× | None | Yes | **Accepted** |
| B: range-streaming via Indexer gRPC | 5–10× | Yes ([Zebra #10579]) | Yes | Defer to a future ADR when the upstream lands |
| C: in-process via `zebra_state::ReadStateService` | 20–50× | Workspace dependency | No | Defer to a future ADR; opt-in for advanced operators |
| D: RocksDB secondary against Zebra | 20–50× | Implicit schema dependency | No | Rejected: Zebra has never committed to its on-disk schema as a public API |

**A** is the unilateral near-term improvement. It is the only option
that requires no upstream cooperation and is the only one that ships
today.

**B** and **C** are deferred. Their proto shape, capability strings,
config schema, trait additions, and `WriterStatus` fields will be
decided in their own ADRs when the corresponding backends have working
implementations to validate the shape against. Pre-declaring vocabulary
for backends that do not exist violates the project's "aspirational
vocabulary is not pre-declared" rule (see
[Node source boundary](../architecture/node-source-boundary.md)) and
locks in a wire shape that may be wrong by the time the backend lands.

**D** is rejected at the architectural level: any Zebra storage refactor
would break Zinder silently, regardless of throughput.

## Consequences

**For operators (UX).** Cold-start time on testnet drops from ~1 hour
to ~30 minutes; mainnet drops from ~24 hours to ~12 hours. No config
change is required; the optimisation is automatic.

**For developers (DX).** `NodeSource` keeps its single-call shape
(`fetch_block_at`). No new trait methods, no new capability bits, no
new config knobs. When a streaming backend lands, the new trait method
will arrive together with it in a single coordinated change.

**For agents (AX).** `IngestControl.WriterStatus` is unchanged. Proto
field number 6 on `WriterStatusResponse` is reserved for the future
transport-identity field, so adding it later is additive.

**Removed surface.**

- The serial `getblockhash` round trip on `ZebraJsonRpcSource::fetch_block_at`.

**Added surface.**

- `SourceError::BlockReorgDuringFetch { height, reason }` (classified as
  `SourceFailureClass::UpstreamViewChanged`).
- `BlockHeight::next(self) -> Option<Self>` core primitive.

## References

- [ADR-0013: Source Failure Recovery Topology](0013-source-failure-recovery-topology.md): the failure-recovery shape this change preserves.
- [ADR-0015: Unified Phase-Driven Ingest](0015-unified-phase-driven-ingest.md): the writer-side phase model this change serves.
- [Node source boundary](../architecture/node-source-boundary.md): the trait and capability contract this change does not touch.
- [Chain ingestion](../architecture/chain-ingestion.md): the broader pipeline this ADR drills into.
- [`crates/zinder-source/src/zebra_json_rpc.rs`](../../crates/zinder-source/src/zebra_json_rpc.rs): the adapter that holds the optimised `fetch_block_at`.
- [Zebra #10579]: upstream issue tracking the future `GetBlockRange` Indexer gRPC RPC that would unlock Option B.

[Zebra #10579]: https://github.com/ZcashFoundation/zebra/issues/10579
