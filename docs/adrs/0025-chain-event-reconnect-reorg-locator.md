# ADR-0025: Self-Healing Reorg On ChainEvents Reconnect With A Locator Cursor

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | Zinder |
| Domain | Chain-event subscription resume, stream cursor byte contract, reorg recovery |
| Related | [ADR-0002](0002-boundary-specific-serialization.md), [ADR-0003](0003-canonical-storage-access-boundary.md), [ADR-0007](0007-mempool-topology-and-retention.md), [ADR-0024](0024-wire-format-rpc-byte-order.md), [Chain events](../architecture/chain-events.md), [Wallet data plane](../architecture/wallet-data-plane.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

A `ChainEvents` consumer persists the opaque cursor bytes from the last envelope it durably applied and resumes strictly after that cursor on reconnect. Between disconnect and reconnect the consumer's last-seen block can be reorged out of the canonical chain. The consumer must learn that its branch changed; a silent resume from a divergent position would leave it applying canonical blocks on top of a stale branch.

A single `(last_height, last_hash)` position in the cursor detects divergence only at the cursor's own tip. When the reorg event that crossed that tip has already been pruned from the bounded event log, a tip-only position cannot tell whether the consumer's branch is still canonical, and it cannot locate where the two branches last agreed. The reorg-window event log is time-bounded by retention; the divergence base routinely falls outside it. The canonical block index, by contrast, retains the hash at every height for the visible chain and outlives the pruned event window.

## Decision

### The cursor carries a locator, not a single position

The chain-event body inside the HMAC-authenticated `StreamCursorTokenV1` envelope carries a bounded locator: an ordered, tip-first set of `(height, hash)` pairs exponentially back-spaced from the cursor's tip (`tip`, `tip-1`, `tip-2`, `tip-4`, `tip-8`, doubling the gap each step, clamped at genesis). The locator carries at least the tip entry and at most `CHAIN_EVENT_LOCATOR_MAX = 32` entries. The cap bounds both the recoverable reorg depth (32 exponentially spaced entries reach roughly 2^31 blocks of fork depth) and the on-the-wire cursor size.

The server builds the locator from the canonical block index at every height it bookmarks; the client treats the cursor as opaque and never parses it. The locator stays inside the existing `StreamCursorTokenV1` envelope and is covered by the same HMAC, so a tampered locator entry fails authentication.

### Envelope framing

The fixed 50-byte body and the family nibble at offset 49 are unchanged. The tip entry occupies the same offsets the prior single-position body used (height at 13, hash at 17). A one-byte count of the back-spaced ancestor entries follows the body, then the ancestor entries (4-byte height plus 32-byte hash each), then the 32-byte HMAC over the whole body. The chain-event cursor is therefore variable-length; the mempool, transparent-history, and address-output families keep the fixed-length body and are undisturbed. Hash bytes inside the cursor are storage-internal byte order (ADR-0024); the cursor is server-internal material the client never decodes.

### Resume algorithm

The server resolves the fork point as the most recent locator entry whose hash equals the canonical block hash at that height, then:

- **Top entry on-chain.** No reorg. Resume from `event_sequence + 1`. Expire only when the next event sits below the retention floor (events between the cursor and the floor were pruned).
- **A lower entry is the fork point, and the real reorg events are retained at or after `event_sequence`.** Resume from `event_sequence + 1`; the persisted `ChainReorged` replays. No synthesis.
- **A lower entry is the fork point, but the event log at `event_sequence` is pruned.** Synthesize a `ChainReorged` reverted from the fork point, deliver it ahead of the page, then resume from the retention floor so the retained events re-commit the post-fork canonical chain.
- **No locator entry on the canonical chain.** The divergence is deeper than the cap or the fork-point block is unresolvable. Return `CHAIN_EVENT_CURSOR_EXPIRED` with re-derive guidance.

`CHAIN_EVENT_CURSOR_INVALID` is reserved for genuine corruption or forgery: a failed HMAC, a malformed body, a zero sequence, or a sequence ahead of history. A reorg never produces `INVALID`.

### Synthetic reorg on a pruned gap

The synthesized `ChainReorged` reverts `(fork_point, reverted_tip]` and re-commits the canonical range above the fork point. It occupies a delivered envelope ahead of the page scan; the gRPC handlers and the ingest stream driver forward it like any other envelope. Recovery is idempotent: the synthetic envelope's cursor bookmarks the on-chain fork point one sequence below the retention floor, so a reconnect that has not yet applied the reorg resumes from the retained events that re-commit the canonical chain. Forward progress is guaranteed because the synthetic envelope's cursor advances past the synthesis rather than re-naming the reorged-out position.

A consumer therefore only ever handles `ChainCommitted` and `ChainReorged` and never reconciles hashes itself.

### The Settled family never receives a synthesized reorg

A `Settled` cursor delivers only non-reorg commits entirely at or below the settled tip. A `Settled` cursor cannot be reorged out below the settled tip by definition, so a locator miss is an expiry, never a synthesized reorg. The `Settled` family never carries `ChainReorged`.

## Consequences

### What this enables

- A wallet whose last-seen block was reorged away recovers on reconnect without a full re-derive, even when the divergence point has aged out of the event log.
- The reconnect-reorg guarantee in the chain-event and wallet-data-plane docs is enforced by the resume algorithm and covered by integration tests.
- The recovery path adopts the locator-based fork-point design that the read-only-secondary ecosystem (Zebra `ReadRequest::FindForkPoint`) is converging on, while keeping Zinder's push-model `ChainEvents` stream instead of adding a pull query.

### What this costs

- Each emitted chain-event envelope's cursor is enriched with back-spaced block-index reads, bounded by the 32-entry cap.
- A reorg deeper than the cap or with an unresolvable fork-point block degrades to `CHAIN_EVENT_CURSOR_EXPIRED`; such a consumer re-derives from canonical artifacts.
- The synthetic `ChainReorged` is recomputed deterministically on each reconnect rather than persisted, so a colocated-secondary reader and the primary writer produce the same recovery without a write on the read path (ADR-0003).

## Vocabulary

- `ChainEventLocator` (the tip-first bounded set of `(height, hash)` entries inside the cursor body).
- `CHAIN_EVENT_LOCATOR_MAX` (the 32-entry cap on locator depth and cursor size).
- `CHAIN_EVENT_CURSOR_EXPIRED` (degradation when no locator entry is canonical).
- `CHAIN_EVENT_CURSOR_INVALID` (reserved for corruption or forgery only).

See [Chain events §Resume Semantics](../architecture/chain-events.md#resume-semantics) and [Public interfaces §Cursor Conventions](../architecture/public-interfaces.md#cursor-conventions).
