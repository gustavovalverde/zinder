# ADR-0023: Pipelined backfill and concurrent block fetch

## Status

Accepted, 2026-05-13.

## Context

The `backfill` subcommand reads historical blocks from the upstream node and writes the resulting canonical artifacts into the local RocksDB store. The first deployment of this code path against a public testnet exposed a throughput floor of roughly 0.5 blocks/sec, which projected to ~90 days for a fresh testnet sync from genesis (testnet tip ~4M blocks at the time of measurement).

Two structural choices drove that floor:

1. The backfill loop fetched blocks sequentially. Each iteration awaited `fetch_block_with_retry` to completion before starting the next height, so the wall clock collapsed to a linear sum of per-block latencies. Network round trips dominate that latency on PaaS-style deployments (~10 ms colocated).
2. The Zebra JSON-RPC adapter fetched each block via four sequential calls (`getblockhash`, `getblockheader`, `getblock`, `z_gettreestate`). Calls 2-4 all key on the hash returned by call 1, but they are otherwise independent. The serial chain multiplied the per-block round-trip cost by four.

Zebra exposes a gRPC `Indexer` service (port 8155 by default), but inspection of `zebra-rpc/proto/indexer.proto` confirms it serves only live subscriptions: `ChainTipChange`, `NonFinalizedStateChange`, and `MempoolChange`. It does not return historical blocks. The only path to historical block data is the JSON-RPC interface, so the optimization surface is the JSON-RPC client and the caller's concurrency shape.

## Decision

Two changes are accepted as the v1 throughput design for backfill.

### Within `fetch_block_by_height`: drive the three hash-dependent calls concurrently

`ZebraJsonRpcSource::fetch_block_by_height` awaits `getblockhash` first, then issues `getblockheader`, `getblock` (verbosity=0), and `z_gettreestate` concurrently via `tokio::join!`. The implementation reduces per-block round-trip count from four serial RTTs to one (`getblockhash`) plus one parallel triple, regardless of how the caller dispatches blocks.

### Within `backfill`: pipeline block fetches with bounded concurrency

`backfill_from_source_with_store` constructs a `futures_util::stream::iter(height_range).map(fetch).buffered(N)` pipeline with `N = BACKFILL_FETCH_CONCURRENCY = 32`. `buffered` preserves submission order, which keeps the artifact-assembly and commit path strictly ordered. The 32 in-flight fetches saturate the JSON-RPC connection pool without pinning the node's CPU; operators driving ingest against a dedicated node can raise the constant in source.

Each in-flight fetch carries its own `IngestRetryState`. The run-wide retry budget continues to gate the post-batch operations (subtree-root fetches, finalization).

`tip-follow` is unchanged: it commits one block per poll because by definition it is following the tip, where pipelining offers no headroom. The 2 s default `poll_interval_ms` remains the right shape for live operation.

## Consequences

Throughput measured against a colocated PaaS-hosted Zebra testnet node:

- Before: ~0.5 blocks/sec via serial fetch.
- After: ~75-150 blocks/sec during early-Sapling batches, expected to settle in the 30-100 blocks/sec band as block weight grows toward mainnet density.

This brings a full testnet historical backfill (Sapling activation through reorg-window boundary, ~3.7M blocks) into the single-digit-hours range instead of weeks.

The pipelined shape does increase pressure on the upstream node's JSON-RPC connection pool. Operators running Zinder against a shared Zebra (a node already serving wallets or block explorers) should tune `BACKFILL_FETCH_CONCURRENCY` downward or run ingest against a dedicated reader. Zebra's default RPC server accepts the load comfortably; the operational concern is fairness, not capacity.

The per-block parallel-triple changes the failure shape: when any of the three concurrent calls returns a transient error, the awaiting `tokio::join!` still drives the others to completion before returning the first error. This is the standard `join!` semantic and matches the existing retry behavior for the block as a whole; partial responses are discarded and the block is re-fetched.

## Out of scope

The following optimizations are deliberately deferred. They are accepted as follow-up ADRs if a future deployment shape needs them.

- **JSON-RPC batching**: `jsonrpsee::http_client` supports batching multiple requests in one HTTP POST. Combined with pipelining, this would compress the per-block round trips further. The marginal win on top of `tokio::join!` is modest because HTTP/1.1 already keeps connections alive and the joined calls fit in the local TCP send buffer.
- **gRPC live tip-follow**: subscribing to Zebra's `Indexer.ChainTipChange` stream would replace tip-follow's 2 s poll with push notifications. This is a separate concern that does not move backfill throughput; documented for completeness so that the rationale for keeping JSON-RPC on the ingest side is explicit.
- **Configurable concurrency knob**: `BACKFILL_FETCH_CONCURRENCY` is a constant in this revision. A `BackfillConfig` field plus CLI and env-var plumbing follows the established pattern but is not yet warranted by an operator-facing requirement.

## References

- `services/zinder-ingest/src/backfill.rs`: pipelined backfill loop.
- `crates/zinder-source/src/zebra_json_rpc.rs`: concurrent per-block RPC fan-out.
- `zebra-rpc/proto/indexer.proto`: confirmed gRPC indexer surface (live subscriptions only).
- [`chain-ingestion.md`](../architecture/chain-ingestion.md): ingestion contract.
- [`node-source-boundary.md`](../architecture/node-source-boundary.md): source-adapter ownership.
