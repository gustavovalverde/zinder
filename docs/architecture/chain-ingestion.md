# Chain ingestion

`zinder-ingest` is the only canonical writer. The release runtime either opens
an authenticated ready store or constructs one in a writer-owned staging path,
then continuously follows Zebra. It also owns live mempool state and the private
control APIs used by projectors and trusted readers.

## Runtime sequence

```text
load and validate configuration
  -> connect to Zebra and discover capabilities
  -> discover network-upgrade activations
  -> open a ready canonical store
     or recover a published staging store
     or construct and publish a fresh store
  -> start continuous canonical following
  -> serve authenticated control commands and mempool state
```

Store identity, network, workload, activation fingerprint, reorg policy, and
schema are validated before a ready store is admitted. An incompatible
non-empty path fails without mutation.

## Fresh construction

Fresh construction uses a sibling staging path. `CanonicalStoreBuildPlan`
fixes the source range, checkpoint predecessor, network, workload, activation
fingerprint, reorg policy, and construction manifest before data is loaded.

The source path fetches bounded connected segments. Block preparation validates
source identity, parses each block once, and constructs `CanonicalBlockFacts`,
retained raw blobs, compact-block material, tree commitments, and direct-index
inputs. CPU-heavy preparation may run in parallel; commitment-tree positioning
and publication remain ordered.

`CanonicalConstructionSettings` and `CanonicalPipelineLimits` bound:

- blocks, artifact bytes, and estimated write bytes per batch;
- source segment size and target response bytes;
- concurrent source requests and reserved response bytes;
- block-preparation concurrency; and
- queued artifact bytes while a prior batch is committed or flushed.

Construction loads an inactive `RocksDbCanonicalBuilder`, validates continuity,
manifest identity, per-block replay envelopes, and the ordered facts digest,
then publishes `CanonicalBaselinePublication`. The candidate is not readable as
a canonical store until publication succeeds.

On restart, a ready staging store is installed and cold-opened at the configured
path. An unpublished or invalid staging store is removed and reconstructed. A
ready configured store is never replaced by staging data.

## Continuous following

`CanonicalFollower` polls one atomic source tip and compares it with the
admitted `CanonicalEventFence`.

- A connected next block produces `CanonicalLiveAppend`.
- A changed suffix within `CanonicalReorgPolicy` produces
  `CanonicalLiveReplacement`.
- A replacement deeper than the persisted policy fails closed.
- An unchanged source tip waits for the configured poll interval.

Append and replacement commits advance block facts, direct indexes, chain
epoch, ordered sequence digest, and retained canonical event state atomically.
The writer does not perform historical wallet-output lookups or read wallet
projection state.

Readiness becomes ready only when canonical lag is within
`ingest.follow.lag_threshold_blocks` and the live mempool gate, when configured,
has completed its current source snapshot. Source failures move readiness to
`node_unavailable`, preserve committed state, back off by failure class, and
resume from the current fence. Storage corruption, schema mismatch, invalid
reorg depth, and internal invariant failures terminate the writer.

## Canonical events and retention

Every visible append or replacement publishes an authenticated retained event
with a monotonically increasing sequence. Projectors use event history to bind
their progress to canonical state. A writer-owned retention lease prevents
pruning below a projector's required anchor. Event retention and lease
operations are serialized through the canonical owner.

See [Chain events](chain-events.md) for event semantics and cursor behavior.

## Mempool ownership

The live mempool owner starts with the control surface and consumes either the
Zebra indexer stream or the JSON-RPC source selected by configuration. It
publishes a complete in-memory snapshot before satisfying the writer readiness
gate, then applies typed added, mined, and invalidated events. Retained mempool
events support cursor resume and bounded audit queries.

Mempool state is not part of a canonical block commit. The control API exposes
snapshot pages, transaction lookup, and event streaming without granting any
reader a canonical write handle.

## Configuration

The release writer uses these sections:

- `[network]` and `[node]` select and authenticate the upstream source;
- `[storage]` selects the canonical path and RocksDB resource budget;
- `[ingest.construction]` bounds fresh construction;
- `[ingest.follow]` controls polling and readiness lag;
- `[ingest.run_overrides]` supplies an optional checkpoint or target height;
- `[retention]` controls canonical and mempool event windows; and
- `[ingest_control]` configures the private authenticated control listener.

`ingest.phase_classification` remains available to the diagnostic `probe`
command and the artifact-oriented ingestion library. It does not choose between
two release binaries or require an operator handoff.

## Invariants

- Only `zinder-ingest` opens canonical storage for writes.
- Construction never publishes before complete validation.
- Following never commits a replacement beyond the fixed reorg policy.
- Canonical commits contain block-local facts and direct canonical indexes, not
  wallet or explorer query models.
- Source errors are recoverable readiness states; integrity errors fail closed.
- Secrets and raw authorization material never appear in logs or
  `--print-config` output.
