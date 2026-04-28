# Chain Events

Chain events are the contract between source observations, canonical commits, query freshness, and derived-index replay. They are not a storage table and not a wallet API by themselves.

This document settles the event vocabulary that the rest of the architecture should reuse. The concrete gRPC schema for `ChainEpochReadApi` should be designed after this model, not before it.

## Boundaries

Zinder has three event boundaries with different jobs:

| Boundary | Producer | Consumer | Purpose |
| -------- | -------- | -------- | ------- |
| Source observations | `NodeSource` adapter | `zinder-ingest` | Report upstream node observations before canonical commit |
| `ChainEvent` | `zinder-ingest` | ingest-owned publisher and private subscription endpoint | Describe the committed canonical transition |
| `ChainEventEnvelope` | ingest subscription plane | `zinder-query`, `zinder-client`, `zinder-derive`, and external derived consumers | Carry replayable, cursor-bound chain events over the wire |

Keep these layers distinct. A source observation is not a committed chain event. A wire envelope is not the internal state-machine event.

## Source Boundary

`NodeSource` normalizes Zebra ReadState, zcashd JSON-RPC, and future streaming sources into source observations. It does not decide canonical state and does not build artifacts. Trait shape, capability model, and adapter rules live in [Node source boundary](node-source-boundary.md).

For the event model: source observations are not committed chain events. A streaming follower will introduce explicit `chain_source_events` plus resume cursors when that backend lands; until then ingest drives the source through async polling. Best-chain selection uses cumulative chainwork, not tip height.

## Ingest State Machine

`zinder-ingest` consumes source events through one deterministic state machine; the pipeline and substep decomposition are in [Chain ingestion §Operation Shape](chain-ingestion.md#operation-shape). The contract this document owns:

- The event is persisted in the same `commit_chain_epoch` `WriteBatch` that advances the visible epoch pointer. Publication and commit are atomic.
- The state machine produces three outputs: a `ChainEpochArtifacts` value passed to `commit_chain_epoch`, a durable epoch pointer after the batch succeeds, and a `ChainEvent` published only after the epoch is durable.

## Canonical Event Boundary

`ChainEvent` is the in-process event emitted after canonical storage changes. It follows the Reth-style committed, reorged, reverted shape while preserving ADR-0003's post-commit event names.

```rust
pub enum ChainEvent {
    ChainCommitted {
        committed: ChainEpochCommitted,
    },
    ChainReorged {
        reverted: ChainRangeReverted,
        committed: ChainEpochCommitted,
    },
}
```

Use `ChainReorged` when one durable transition both invalidates a visible non-finalized range and commits the replacement range. Use `ChainCommitted` for a pure append or finalized-boundary advance. Zinder does not expose an explicit rollback transition without a replacement range.

Do not publish source observations as `ChainEvent`. Do not publish `ChainEvent` before `commit_chain_epoch` succeeds.

## Reorg Replacement Contract

`ChainEpochArtifacts.reorg_window_change` carries the storage mutation that makes the event true:

| Chain transition | `ReorgWindowChange` | Published event |
| ---------------- | ------------------- | --------------- |
| Append inside the current best chain | `Extend { blocks }` | `ChainCommitted` |
| Reorg inside the configured window | `Replace { from_height }` | `ChainReorged` |
| Finalized prefix advances | `FinalizeThrough { height }` | `ChainCommitted` |
| No reorg-window mutation | `Unchanged` | `ChainCommitted` only when artifacts changed |

The replacement range must start at the first height where the old visible branch and the new selected branch differ. It must not replace finalized data. If the replacement starts below the supported window or below the finalized boundary, `zinder-ingest` returns `ReorgWindowExceeded`, fails readiness with `reorg_window_exceeded`, and requires operator action.

Reject the name `ReorgTooDeep`. It describes a symptom. `ReorgWindowExceeded` names the configured boundary that was violated.

`ChainReverted` was considered and removed in favor of `ChainReorged`, which
binds the reverted and replacement ranges in one durable transition.

`zinder-ingest tip-follow` is the first live producer of
`ReorgWindowChange::Replace`; historical backfill only appends and finalizes
already-stable ranges.

## Wire Event Envelope

The ingest subscription plane exposes chain events as a resumable stream of `ChainEventEnvelope` messages:

```text
ChainEventEnvelope
  cursor: StreamCursorTokenV1
  event_sequence: u64
  chain_epoch: ChainEpoch
  finalized_height: BlockHeight
  event: ChainCommitted | ChainReorged
```

The Substreams last-irreversible-block pattern maps to Zinder's `finalized_height`. Every envelope carries the finalized height that was true for that event. Consumers may discard undo state at or below that height.

`StreamCursorTokenV1` uses the storage-authenticated cursor shape from [ADR-0002](../adrs/0002-boundary-specific-serialization.md) and [ADR-0005](../adrs/0005-chain-event-cursor-sequence.md). It carries the event sequence, last height, and last hash for one chain-event stream. Adding a second cursor format for chain events requires a new ADR.

The wallet-facing exposure of this envelope is settled by [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription): the same `ChainEventEnvelope` shape is published as a `zinder.v1.wallet` proto message and streamed by `WalletQuery.ChainEvents`. The cursor crosses the wire as opaque bytes so wallet clients persist the exact bytes they received and replay strictly after them on reconnect.

The cursor body is not decorative state. `event_sequence` is the resume key, and `last_height` plus `last_hash` are position checks for the event that produced the cursor. If future stream families add cursor fields that are not immediately consumed, the field must be documented as reserved in the stream-specific contract before it is serialized.

## Resume Semantics

Derived consumers resume through
`chain_event_history(ChainEventHistoryRequest { from_cursor, max_events })` or
the equivalent private ingest subscription RPC.

Wallet consumers resume through `WalletQuery.ChainEvents` per [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription). The cursor protocol below is the same for both consumer paths.

Rules:

- With no cursor, the stream starts at the earliest retained canonical event.
- With a valid cursor, the stream returns events strictly after that cursor.
- Every history read has a non-zero `max_events` page bound. Consumers that need
  more events resume with the last cursor returned by the previous page.
- Consumers persist the cursor only after their sink has durably applied the event.
- If the cursor is older than retained history, the API returns `EventCursorExpired`. It must not silently start from the current tip.
- If the cursor belongs to another network, store identity, or stream family, the API returns a typed cursor error.
- If a cursor names an event sequence that does not exist yet, the API returns a typed cursor error. A pre-history cursor must not be treated as an empty stream.
- If the cursor's `last_hash` no longer matches the canonical block hash at `last_height`, the cursor's branch was reorged out. The server emits a synthetic `ChainReorged` envelope before resuming, per [Chain events §Cursor varieties](chain-events.md#cursor-varieties). The synthetic envelope occupies a real `event_sequence` slot and is persisted for idempotent recovery.
- If a consumer falls behind retention, it must recover from a checkpoint or rebuild from canonical artifacts.

### Cursor varieties

`StreamCursorTokenV1`'s `flags` byte carries a family code in the lower nibble (per [Chain events §Cursor varieties](chain-events.md#cursor-varieties)). Two `ChainEvents` family codes are active:

- **`0x0` `ChainEventTip`** — receives every `ChainCommitted` and `ChainReorged` envelope. Default for wallet consumers; clients must handle reorgs.
- **`0x1` `ChainEventFinalized`** — receives only envelopes whose `chain_epoch.tip_height <= finalized_height`. Never receives `ChainReorged`. Default for explorer and analytics consumers; trades latency for absence of reorg events. Bootstrap uses `WalletQuery.ChainEvents` with `family = Finalized` and an empty `from_cursor`.

Future stream families (`Mempool`, `Derive`) are reserved in the family-code table but use parallel cursor body types under their own contracts.

Do not use `epoch_history(from)` as the durable API name. It hides reorgs. Use `chain_event_history` or an explicitly equivalent gRPC name because consumers are replaying events, not only listing epochs.

Chain-event rows store event data and deterministic cursor inputs, not opaque
cursor bytes. The cursor is reconstructed when the envelope is returned, so the
durable row is not tied to one serialized token.

## Error Classification

Use these errors at the event and source boundaries:

| Error | Boundary | Meaning |
| ----- | -------- | ------- |
| `NodeUnavailable` | `NodeSource` | The configured upstream node cannot answer requests |
| `NodeCapabilityMissing` | `NodeSource` | A capability required by the caller is not advertised by the source |
| `TransactionBroadcastDisabled` | `TransactionBroadcaster` | The wired broadcaster is the no-op `()` impl, signaling a deliberately read-only deployment |
| `SourceProtocolMismatch` | `NodeSource` | The source response does not match the expected network or protocol version |
| `BlockUnavailable` | `NodeSource` | A block needed for ancestor reconstruction cannot be fetched |
| `ReorgWindowExceeded` | ingest and API | The selected branch requires replacing data outside the supported window |
| `EventCursorExpired` | `ChainEpochReadApi` | The event cursor is older than retained event history |
| `EventCursorInvalid` | `ChainEpochReadApi` | The cursor fails authentication, network, store, or stream-family validation |

The streaming follower will reintroduce typed source-cursor and source-gap errors when the streaming method lands. Until then, those failure modes do not exist at the source boundary.

Internal storage errors still map to API errors at service boundaries: `ChainEpochMissing` maps to `EpochNotFound`, and `ArtifactMissing` maps to `ArtifactUnavailable`.

## Retention And Backpressure

`zinder-ingest` must retain enough event history for expected `zinder-derive` outages and query catch-up windows. The retention policy is an operational setting, not a protocol shortcut.

Event streams must be bounded:

- The future streaming source method applies backpressure to source adapters.
- `ChainEventEnvelope` streams apply backpressure to `zinder-query` and `zinder-derive`.
- A slow consumer must not block `commit_chain_epoch`.
- If a consumer exceeds retention, the system returns `EventCursorExpired` and requires replay from checkpoint or canonical artifacts.

Chain-event retention is governed by [Chain events §Retention And Backpressure](chain-events.md#retention-and-backpressure). The default policy is time-windowed, operator-tunable through `[ingest.retention] chain_event_retention_hours` (default 168). A background pruning task in `zinder-ingest` deletes events whose `created_at` falls below the cutoff; pruning preserves `event_sequence` monotonicity by leaving gaps rather than rewriting the sequence space. The `oldest_retained_sequence` is surfaced through the `cursor_at_risk` readiness cause when the retention window approaches exhaustion under load. Operators tune retention based on consumer characteristics; setting `chain_event_retention_hours = 0` disables pruning and is reserved for local development.

## Module Naming Guidance

Future implementation modules should be named after the boundary they own:

- `node_source`
- `chain_source_event`
- `chain_event`
- `chain_event_publisher`
- `chain_event_stream`
- `chain_reorg`
- `event_cursor`

Avoid `event_service`, `reorg_manager`, `notification_handler`, `source_processor`, `stream_utils`, and public `push` modules. Those names hide ownership and do not tell a reader which boundary they are reading.

## Cross-References

- [Wallet data plane §Chain-Event Subscription](wallet-data-plane.md#chain-event-subscription) defines the public wire surface (`WalletQuery.ChainEvents`, `ChainEventEnvelope`, `ChainCommitted`, `ChainReorged`).
- [M3 Mempool](../specs/m3-mempool.md) defines the parallel mempool event stream and its `MempoolStreamCursorV1` cursor body.
