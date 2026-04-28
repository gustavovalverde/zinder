# M3 Mempool (Working Spec)

| Field | Value |
| ----- | ----- |
| Status | Working spec; mutable until M3 ships |
| Created | 2026-04-29 |
| Domain | Mempool source ingestion, live mempool index, mempool-event retention, native wallet API, lightwalletd compatibility, typed Rust client, product validation |
| Canonical product map | [Wallet data plane §Mempool Snapshot and Subscription](../architecture/wallet-data-plane.md#mempool-snapshot-and-subscription-m3) |
| Reference evidence | [Serving Zebra and Zallet](../reference/serving-zebra-and-zallet.md), [Android wallet integration findings](../reference/android-wallet-integration-findings.md), [Lessons from Zaino](../reference/lessons-from-zaino.md) |

## Why this spec exists

M3 is the first unconfirmed-transaction milestone. It must not become a
Zashi/Zodl-only implementation, a Zallet-only Rust helper, or a lightwalletd
compatibility cache. The ecosystem evidence points to one architecture:

```text
NodeSource
  -> MempoolSourceEvent
  -> zinder-ingest MempoolIndex + MempoolEventLog
  -> WalletQueryApi / zinder-client
  -> native gRPC and compatibility adapters
```

Native Zinder surfaces are the durable product contract. The lightwalletd
compatibility methods are adapter views over that contract. This keeps Zallet,
Android SDK/Zashi/Zodl, existing lightwalletd clients, explorers, Zebra, and
future agents from learning different mempool models.

## Product priority

The product priority is grounded in the local source audit under
`/Users/gustavovalverde/dev/zfnd`:

| Priority | Consumer | What M3 must unlock |
| -------- | -------- | ------------------- |
| P0 | Zallet (`wallet/zallet`) | Typed mempool presence, transaction lifecycle, rebroadcast decisions, transparent unmined UTXO overlays, and removal of the "mempool stream closed means tip changed" workaround, validated by both a `ChainIndex` contract harness and a real Zallet binary/app run. |
| P0 | Android SDK, Zashi/Zodl, existing lightwalletd clients | `GetMempoolStream` and `GetMempoolTx` compatibility over the native M3 index, so wallets can observe pending shielded activity and improve pending-send feedback without changing their lightwalletd client. |
| P0 | Operators and agents | Capability-gated, typed, introspectable mempool readiness. No M3 production-ready claim before source hydration, live index, event log, cursor retention, native API, and compat mappings are all wired; individual services still advertise only the capabilities they actually serve. |
| P1 | Explorers and analytics | Live mempool pages, pending transaction lifecycle, and pending transparent overlays through native `WalletQuery` or replayable `zinder-derive` views. Explorer parity also needs transparent history and balance work outside M3. |
| P1 | Zebra and Zaino comparison | Zebra is the source of verified observations; Zaino is the comparator. Neither defines Zinder public types. |

The first production slice therefore prioritizes the source/index/event-log
spine, native Rust/native gRPC access, and the two lightwalletd mempool methods.
Explorer-specific derived views come after those contracts exist.

## Scope

M3 includes:

- `zinder-source` mempool adapters for Zebra streaming and JSON-RPC polling.
- `zinder-ingest` ownership of the live `MempoolIndex`.
- A persistent `MempoolEventLog` with independent cursor sequence and retention.
- Native `WalletQuery.MempoolSnapshot` and `WalletQuery.MempoolEvents`.
- `zinder-client::ChainIndex` mempool methods for Zallet and future Rust clients.
- `zinder-compat-lightwalletd` mappings for `GetMempoolStream` and `GetMempoolTx`.
- Server capabilities, readiness, metrics, structured errors, and live tests.

M3 does not include:

- Server-side shielded address or viewing-key scanning.
- Full transparent address history or balance parity.
- Explorer-specific materialized views beyond the core mempool stream/snapshot.
- Any new lightwalletd proto extensions.

## Locked vocabulary

- `MempoolEntry`: a server-observed mempool transaction record. Use this for
  indexer state.
- `PendingTransaction`: forbidden for server-observed state. Pending is a
  wallet-local UX state and can include transactions the network never accepted.
- `MempoolSourceEvent`: a source observation before it is written into the
  live index or event log.
- `MempoolEventEnvelope`: the public retained stream item with cursor and event
  sequence.
- `MempoolStreamCursorV1`: the opaque cursor for M3 mempool events. It uses the
  reserved family code `0x2`, but it is not a chain-event cursor body.
- `TransparentMempoolOutput`: transparent output currently visible in the
  mempool index.
- `TransparentMempoolSpend`: transparent outpoint-spend relationship currently
  visible in the mempool index.
- `TransparentMempoolOutputsRequest`: bounded request for mempool outputs tied
  to one transparent address; follows the mined `TransparentAddressUtxosRequest`
  pattern rather than exposing raw source address parsing at call sites.

## Source contract

`zinder-source` exposes one internal source event enum:

```rust
pub enum MempoolSourceEvent {
    Added(MempoolEntry),
    Invalidated {
        txid: TransactionId,
        reason: MempoolEvictionReason,
    },
    Mined {
        txid: TransactionId,
        mined_height: BlockHeight,
    },
}

pub enum MempoolEvictionReason {
    Conflict,
    Expired,
    LowFee,
    NodeRejected,
    Unknown,
}
```

`MempoolEntry` contains at least:

- `txid`
- `auth_digest` when the source provides it
- raw transaction bytes
- compact transaction bytes or enough parsed material to build them
- first-seen timestamp
- first-seen `ChainEpoch`
- transparent outputs by address
- transparent spends by outpoint

Zebra streaming is preferred when the indexer gRPC surface is available. Local
Zebra evidence shows `MempoolChange` has `ADDED`, `INVALIDATED`, and `MINED`
variants, but the gRPC message carries only transaction hash and auth digest.
Therefore an `ADDED` observation must be hydrated by fetching the raw
transaction before it becomes a public `MempoolEntry`. The compatibility adapter
must never compensate by calling the upstream node directly.

Polling is the fallback. It diffs `getrawmempool` results, hydrates added txids
with raw transaction fetches, emits `Invalidated { reason: Unknown }` for txids
that disappear without a known block commit, and emits `Mined` from chain
commit correlation.

## Ownership and topology

`zinder-ingest` owns the live mempool because mempool state is source-observed
and non-canonical. It is not part of `commit_ingest_batch` and it is not
canonical chain state.

`zinder-query` owns the public wallet RPC surface. In production it must answer
mempool RPCs by using an ingest-owned private control surface, analogous to the
M2 `ChainEvents` proxy. A secondary RocksDB reader cannot observe an in-process
live `MempoolIndex`, so `zinder-query` must not pretend it can serve live
snapshots from `SecondaryChainStore` alone.

Persistent storage is limited to the retained event log and cursor floor:

- `mempool_event` column family: one event envelope per source event.
- `storage_control.oldest_retained_mempool_event_sequence`: retention floor.
- independent mempool event sequence space, never reused.

## Native public surface

`WalletQuery.MempoolSnapshot` returns a bounded point-in-time view of the live
index. It is for queries and initial state.

Required response fields:

- visible `ChainEpoch` at snapshot time
- mempool snapshot sequence
- snapshot age in milliseconds
- zero or more `MempoolEntry` records
- next-page cursor when the response is truncated

The request must include a server-enforced maximum entry count. An unbounded
"return the whole mempool" native method is forbidden. Lightwalletd compatibility
may preserve lightwalletd behavior by internally streaming bounded pages.

`WalletQuery.MempoolEvents` server-streams retained and live events. It is for
lifecycle changes and cursor resume.

Required semantics:

- empty cursor starts at the earliest retained mempool event
- stream replays retained events, then continues live
- stream end means disconnect or shutdown, never "new block arrived"
- expired cursor returns `MempoolCursorExpired`
- invalid cursor returns `MempoolCursorInvalid`
- no cross-stream ordering guarantee with `ChainEvents`

## Typed Rust client

`zinder-client::ChainIndex` gains:

```rust
async fn mempool_snapshot(&self, request: MempoolSnapshotRequest)
    -> Result<MempoolSnapshotView, IndexerError>;

async fn mempool_events(&self, from_cursor: Option<MempoolEventCursor>)
    -> Result<MempoolEventStream, IndexerError>;

async fn is_in_mempool(&self, txid: TransactionId)
    -> Result<bool, IndexerError>;

async fn transparent_mempool_outputs_by_address(
    &self,
    request: TransparentMempoolOutputsRequest,
) -> Result<Vec<TransparentMempoolOutput>, IndexerError>;

async fn transparent_mempool_spend_by_outpoint(
    &self,
    outpoint: TransparentOutPoint,
) -> Result<Option<TransparentMempoolSpend>, IndexerError>;
```

`transaction_by_id` may return `TxStatus::InMempool` only after the M3 live
index is wired. Before then, advertising or depending on that status is a
contract bug.

## Lightwalletd compatibility

`zinder-compat-lightwalletd` maps existing methods over the native M3 surface:

- `GetMempoolStream`: sends current raw mempool entries, then new `Added`
  raw transactions. It closes on best-chain tip change to preserve the
  lightwalletd contract, but native `MempoolEvents` never uses stream close as a
  chain-tip signal.
- `GetMempoolTx`: streams compact transactions from `MempoolSnapshot`, applies
  `exclude_txid_suffixes`, and respects `poolTypes` filtering. The suffix
  behavior belongs only to the compatibility adapter.

The adapter must not own a second mempool cache, call the upstream node, or invent
Zinder-only lightwalletd methods. `GetMempoolStream` and `GetMempoolTx` are
readiness-gated until the native source/index/event-log path exists.

## Retention, readiness, and capabilities

Default retention:

- `mempool_mined_retention_minutes = 60`
- `mempool_invalidated_retention_hours = 24`

Readiness and metrics:

- `MempoolSourceUnavailable`
- `MempoolHydrationLagging`
- `MempoolCursorAtRisk`
- `zinder_mempool_entries`
- `zinder_mempool_snapshot_age_seconds`
- `zinder_mempool_events_retained`
- `zinder_mempool_events_pruned_total`
- `zinder_mempool_hydration_failures_total`

Capabilities remain absent until fully implemented:

- `wallet.snapshot.mempool_v1`
- `wallet.events.mempool_v1`

Node capabilities are diagnostic inputs, not public wallet promises:

- `node.streaming_source`
- `node.json_rpc`

## Validation gates

M3 is not production-ready until these gates pass:

1. Regtest streaming backend: broadcast a transaction, observe hydrated `Added`,
   mine it, observe `Mined`, and verify `GetMempoolStream` closes on the block.
2. Regtest polling backend: disable streaming source, repeat the same lifecycle
   through `getrawmempool` diffing.
3. Reorg gate: invalidate a block containing a previously mined transaction and
   verify Zinder follows upstream node mempool observations instead of synthesizing
   mempool entries from reverted blocks.
4. Low-retention stale-cursor gate: configure tiny mempool retention, prune, and
   verify `MempoolCursorExpired` with structured details.
5. Zallet contract harness gate: exercise `ChainIndex` mempool queries for
   presence, `transparent_mempool_outputs_by_address`,
   `transparent_mempool_spend_by_outpoint`, and rebroadcast decision support in
   a deterministic harness that can fail fast in CI.
6. Zallet binary/app gate: run the real Zallet binary or app against the M3
   endpoint and confirm it observes mempool lifecycle state without relying on
   the lightwalletd compatibility adapter or on "mempool stream closed means tip
   changed" behavior.
7. lightwalletd gate: verify `GetMempoolTx` and `GetMempoolStream` against the
   vendored proto and an upstream lightwalletd-compatible client.
8. Android/Zashi gate: observe at least one mempool transaction through the SDK
   or Zashi/Zodl against `zinder-compat-lightwalletd`.
9. Mainnet/testnet soak: run with real mempool traffic, record source backend,
   hydration lag, snapshot age, event retention, and reconnect behavior.

## Build order

| # | Goal |
| - | ---- |
| W1 | Add source traits and fake/source tests for `MempoolSourceEvent`, including hydration failure classification. |
| W2 | Implement `MempoolIndex`, transparent mempool output/spend indexes, and compact transaction construction. |
| W3 | Add `mempool_event` storage, cursor encoding, retention pruning, readiness, and metrics. |
| W4 | Add private ingest-control mempool snapshot/events methods and production query proxying. |
| W5 | Add native `WalletQuery` methods and `zinder-client::ChainIndex` methods. |
| W6 | Map `GetMempoolStream` and `GetMempoolTx` in `zinder-compat-lightwalletd`. |
| W7 | Add regtest, low-retention, reorg, lightwalletd, Zallet contract-harness, Zallet binary/app, Android/Zashi, and mainnet/testnet validation gates. |

The order is intentional: compatibility methods land after the native source and
index path, not before.
