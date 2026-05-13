# ADR-0024: gRPC chain-tip notifications drive tip-follow wakeups

## Status

Accepted, 2026-05-13.

## Context

`zinder-ingest`'s `tip-follow` subcommand polls the upstream Zebra node every `poll_interval_ms` milliseconds (default 2000) to detect new tip blocks. Polling has two costs:

1. **Tip-propagation latency**: a new block can sit on Zebra for up to `poll_interval_ms` before Zinder observes it. For a consumer like a faucet, the visible "block landed in indexer" delay is `poll_interval_ms / 2` on average, never zero.
2. **Idle node load**: each tick issues at least one `tip_id` RPC even when the tip has not moved. On a deployment topology where Zinder and Zebra are colocated the cost is negligible, but the round trip itself prevents the loop from being maximally idle.

Zebra exposes an `Indexer` gRPC service (port 8155 by default) when built with `--features indexer`. The service streams `BlockHashAndHeight` notifications on tip changes through the `ChainTipChange` RPC. The notifications are push-based: every accepted tip update is broadcast to subscribers immediately, with no polling cadence.

Zinder already consumes this service for mempool change notifications via `ZebraIndexerMempoolSource`, so the gRPC client wiring, transport-error handling, and operator-facing `indexer_grpc_addr` configuration are all already in place. The only missing piece is a chain-tip subscriber and an integration point in the tip-follow loop.

## Decision

Wire Zebra's `Indexer.ChainTipChange` stream as an optional push-based wake-up signal for the tip-follow loop.

### New module: `zinder-source/src/zebra_indexer_chain_tip.rs`

A `ZebraIndexerChainTipSource` mirrors the existing `ZebraIndexerMempoolSource`: it accepts a `ZebraIndexerSourceTarget`, connects to the indexer gRPC endpoint, calls `chain_tip_change`, and forwards each `BlockHashAndHeight` to a typed `ChainTipNotification` over an mpsc channel exposed as a `ChainTipNotificationStream`. Hash decoding goes through the same `decode_display_block_hash` path used by the JSON-RPC source so the canonical byte ordering stays single-sourced.

### Integration into `tip_follow_with_primary_store`

The tip-follow loop adds a third arm to its `tokio::select!`:

```text
tokio::select! {
    () = cancel.cancelled() => return Ok(()),
    () = wait_for_chain_tip_notification(&mut chain_tip_stream) => {}
    () = tokio::time::sleep(config.poll_interval) => {}
}
```

When a chain-tip notification arrives, the loop wakes immediately and runs `tip_follow_once`. When the stream is `None`, the helper parks on a never-resolving future so the `select!` collapses to the existing poll-only behavior. When the stream errors or ends, the helper sets the stream to `None` and logs the cause, after which the loop reverts to polling.

The `poll_interval` arm stays in the `select!` as a deliberate safety net. Even on healthy streaming deployments the loop keeps a slow polling cadence so that:

1. A transient stream failure cannot stall ingest beyond `poll_interval`.
2. The first iteration runs through the polling arm and catches up against the current tip without waiting for a notification.

### No change to block fetching

The chain-tip stream is a **wake-up signal**, not a block-data source. Each notification triggers the existing `tip_follow_once` path which fetches the new block via JSON-RPC. The reason is that Zinder requires `z_gettreestate` per block, which is JSON-RPC-only on Zebra; switching block-data fetching to gRPC would deliver no additional latency reduction while doubling the number of upstream protocols ingest depends on.

`NonFinalizedStateChange`, which streams full block bytes inline, was evaluated and rejected for the same reason: every notification would still need a JSON-RPC follow-up for `z_gettreestate`, and the per-block round-trip count would remain the same as the JSON-RPC fan-out path in `ZebraJsonRpcSource::fetch_block_by_height`.

### Configuration

The `node.indexer_grpc_addr` field already exists on `NodeTarget` (used by mempool streaming). Setting `ZINDER_NODE__INDEXER_GRPC_ADDR=http://<zebra>:8155` activates both the mempool stream and the chain-tip stream. Operators that do not run Zebra with `--features indexer` leave the field unset and tip-follow operates in polling-only mode unchanged.

## Consequences

Steady-state tip-propagation latency drops from `poll_interval / 2` (~1 s with the 2 s default) to the gRPC notification dispatch latency (~milliseconds on a colocated PaaS network). The loop spends idle time parked on the stream instead of issuing periodic `tip_id` RPCs, which removes the steady-state JSON-RPC tick.

The chain-tip stream is a soft dependency: any failure mode (stream errors, transport reset, indexer endpoint missing) degrades cleanly to polling rather than failing the ingest loop. Operators who run Zebra without `--features indexer` or who deliberately omit `indexer_grpc_addr` continue to use polling-only tip-follow with no code-path differences.

The chain-tip stream is decoupled from the mempool stream. The two subscribers connect independently to the same indexer endpoint and reconnect independently on failure. Sharing the gRPC channel could be a future optimization but adds shutdown-ordering complexity that does not pay back the per-subscription connection cost on Railway-class deployments.

## Out of scope

- **Switching block-data fetch to gRPC**: covered in the Decision section above; the missing `z_gettreestate` on gRPC keeps JSON-RPC on the data plane.
- **Reorg-aware notification de-duplication**: `ChainTipChange` may emit notifications for short-lived non-canonical tips during a reorg. The tip-follow loop already handles reorgs through its existing planner; spurious notifications are a small wake-up overhead, not a correctness concern.
- **Replacing `poll_interval_ms` with a smaller streaming-aware default**: changing the default polling cadence now would couple the streaming-enabled and polling-only paths. The poll interval stays at 2 s for both; the streaming path simply ignores it most of the time.

## References

- `crates/zinder-source/src/zebra_indexer_chain_tip.rs`: chain-tip stream client.
- `services/zinder-ingest/src/tip_follow.rs`: integration point.
- `services/zinder-ingest/src/main.rs`: `build_chain_tip_notification_stream` helper.
- `zebra-rpc/proto/indexer.proto`: `ChainTipChange` RPC contract.
- [ADR-0023: Pipelined backfill and concurrent block fetch](0023-pipelined-backfill-and-concurrent-block-fetch.md): the companion ingest-throughput ADR; this ADR addresses live operation, ADR-0023 addresses historical backfill.
- [`chain-ingestion.md`](../architecture/chain-ingestion.md): ingestion contract.
- [`node-source-boundary.md`](../architecture/node-source-boundary.md): source-adapter ownership.
