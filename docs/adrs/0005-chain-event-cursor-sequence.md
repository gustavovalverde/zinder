# ADR-0005: Use Event Sequence in Chain Event Cursors

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Storage and event replay |
| Related | [Chain events](../architecture/chain-events.md), [Storage backend](../architecture/storage-backend.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

`chain_event_history` is keyed by monotonic event sequence, and Zinder has one canonical chain-event view per store. A cursor format with a separate `view_id` field would carry an ordering concept that no producer or consumer can validate, and the store would still need the event sequence to resume strictly after a previously applied event.

## Decision

The chain-event cursor body in `StreamCursorTokenV1` carries `event_sequence: u64` directly, with no `view_id` slot:

```rust
struct StreamCursorTokenV1 {
    cursor_schema_version: u8,
    network_id: u32,
    event_sequence: u64,
    last_height: u32,
    last_hash: [u8; 32],
    flags: u8,
    auth_tag: [u8; 32],
}
```

The authentication tag covers every preceding byte. Stream-family flags are part of the body so a cursor minted for one stream family cannot be replayed against another. The `flags` byte uses its lower nibble for the family code (`Tip = 0x0`, `Finalized = 0x1`, `Mempool = 0x2` reserved for M3) and its upper nibble for per-family reserved bits.

`chain_event_history` accepts a bounded request:

```rust
struct ChainEventHistoryRequest<'cursor> {
    from_cursor: Option<&'cursor StreamCursorTokenV1>,
    max_events: NonZeroU32,
}
```

The store does not materialize unbounded retained history in one call. A consumer that needs more events resumes with the cursor from the last envelope in the previous page.

Persisted chain-event rows store the deterministic cursor inputs (`event_sequence`, `chain_epoch.tip_height`, `chain_epoch.tip_hash`), not the opaque cursor bytes. `ChainEpochReadApi` reconstructs the cursor when it returns a `ChainEventEnvelope`.

## Consequences

Positive:

- Resuming `chain_event_history` is keyed directly by the value already stored in the event log.
- Cursor decoding has no unused view concept.
- Event-history reads have an explicit page bound at the read API boundary.
- Retained event rows do not duplicate cursor bytes and can be re-emitted with the current store cursor key.
- Future stream families can define their own flag values and cursor body interpretation under the same fixed token envelope.

Negative:

- Rotating the store cursor key invalidates outstanding client-held cursors unless a future multi-key verification window is added.
- Adding multiple retained views per store would require a new cursor version or a stream-specific body shape.
