# ADR-0021: Parallel block derivation and replay hydration

Status: Accepted
Date: 2026-05-20
Related: [ADR-0001](0001-rocksdb-canonical-store.md),
[ADR-0003](0003-canonical-storage-access-boundary.md),
[ADR-0015](0015-unified-phase-driven-ingest.md),
[ADR-0020](0020-bounded-rocksdb-resource-budget.md)

## Context

[The bulk-catchup throughput investigation](../investigations/bulk-catchup-throughput.md) documents a measured mainnet bulk-catchup rate of 8 to 14 blocks/second on a 10-core host, against a single-digit-hours design target of 90 to 350 blocks/second. The source Zebra serves data at 1 % CPU; nine of ten cores on the indexer container sit idle. The ingest container runs at 93 % of a single core.

`zinder-ingest` has two halves of the documented design target already in place. The pipelined fetch (`futures_util::stream::buffered(32)`) is at `services/zinder-ingest/src/backfill.rs`; the concurrent per-block RPCs (`tokio::join!`) are at `crates/zinder-source/src/zebra_json_rpc.rs:638-639`. Neither addresses the actual bottleneck.

Per-block derive work runs synchronously on the same Tokio task that pulls from the fetch stream. The hot path is `zebra_chain::serialization::ZcashDeserialize` on the raw block bytes, per-transaction `zcash_serialize_to_vec` re-emitting canonical bytes for `TransactionArtifact.payload_bytes`, `prost::Message::encode_to_vec` on the assembled `lightwalletd::CompactBlock`, and `SHA256(script_pub_key)` per transparent output. The investigation doc previously labeled this as Sapling/Orchard commitment-tree hashing; that claim is wrong. The code does no such hashing. The work is pure CPU on one thread.

The only cross-block state in the derive path is a two-`u32` running offset (Sapling output count, Orchard action count) inside `CompactBlockArtifactBuilder::current_tree_sizes`. Every other field of the per-block output is a pure function of the source block. Splitting derivation into a parallel-safe phase and a serial fold over those two counters is what makes parallelism correct rather than racy.

The storage layer carries hard invariants that the parallel pipeline must respect. ADR-0003 fixes one primary writer per `PrimaryChainStore`; readers see a chain epoch atomically or not at all. ADR-0020 requires ordered writes (`set_unordered_write` must remain unset), atomic flush across column families (`set_atomic_flush(true)`), and a bounded WAL ceiling. These constraints apply to the `commit_chain_epoch` boundary; nothing in them requires that artifact assembly upstream of the commit also be single-threaded.

## Decision

Split per-block derivation into two pure functions:

- `derive_block(source_block: &SourceBlock) -> Result<DerivedBlockArtifacts, ArtifactDeriveError>` runs the entire parsing, transparent-index, transaction-serialization, and proto-encoding pipeline. The output includes a `tree_size_additions: CommitmentTreeSizes` field (this block's delta) and an `observed_tree_sizes: Option<ObservedCommitmentTreeSizes>` extracted from the node's `z_gettreestate` payload. The lightwalletd compact block carried in the output has `chain_metadata = None` because its final value depends on the running position. The function is `Send + Sync` and has no shared state; multiple instances may run concurrently.
- `finalize_derived_block(derived, running_tree_sizes: &mut CommitmentTreeSizes) -> Result<BuiltArtifacts, ArtifactDeriveError>` applies the delta to a running offset, validates against any source-supplied observation, stamps the final `chain_metadata`, encodes the compact-block proto, and returns the finalized artifacts. This step is fast (no parsing or hashing); it can stay serial on the consumer thread.

Implement the bulk-catchup path as three named logical stages inside
`services/zinder-ingest/src/backfill.rs`:

- `FetchStage` produces an ordered `Stream<Item = Result<SourceBlock, IngestError>>` via the existing `fetch_block_with_retry` pipeline. Concurrency cap: `ingest.bulk_catchup.fetch_concurrency`.
- `DeriveStage` maps the fetch stream through `tokio::task::spawn_blocking(move || derive_block(source_block))`, wrapped in `Stream::try_buffered(ingest.derive.concurrency)`. The buffered slot count is the parallelism cap. Output order is preserved because `try_buffered` yields completed futures in submission order.
- `CommitStage` consumes the derive stream, folds `running_tree_sizes` through `finalize_derived_block`, absorbs `BuiltArtifacts` into the in-flight `IngestBatch`, fetches subtree roots inline, and commits when either the block budget or the transparent-prevout store-lookup budget is reached. Commits run through `tokio::task::spawn_blocking(move || commit_chain_epoch(...))`. The flush cadence runs from this stage after every `flush_interval_epochs` commits.

The stage implementation stays in `services/zinder-ingest/src/backfill.rs`
because fetch, derive, commit, batch accounting, subtree-root hydration, and
flush cadence share one loop-owned state machine. A separate `bulk_catchup/`
module boundary is not introduced until there is an independent owner or a
reusable API. Tip-follow keeps its single-block-per-poll shape and uses the
same `derive_block` + `finalize_derived_block` pair sequentially. The old
`ArtifactBuilder` trait, `IngestArtifactBuilder` struct,
`CompactBlockArtifactBuilder` struct, and `TestArtifactBuilder` test fixture
are removed; tests construct `DerivedBlockArtifacts` directly through a
`test_derived_block` helper.

The same derive concurrency cap also governs startup derive replay. Startup
must replay retained canonical chain events before the public surfaces open
when the canonical store committed an event that the ingest-owned derive store
did not finish before a crash. Replay keeps event dispatch and cursor
advancement serial, but hydrates each event's committed block range with
bounded `spawn_blocking` parsing and resolves transparent prevouts through one
batch `transactions_by_ids` call per event.

Configuration changes:

- `ingest.derive.concurrency` caps the number of in-flight CPU-bound derive and replay hydration tasks. Default: `clamp(available_parallelism() - 1, 4, 32)`. The ceiling at 32 follows Zebra's `full_verify_concurrency_limit` precedent, where wide parallel block processing was observed to thrash large hosts. The floor at 4 keeps small hosts pipelining I/O.
- `ingest.bulk_catchup.flush_interval_epochs` controls the explicit RocksDB flush cadence. No alias is supported because one canonical field name keeps operator docs and agent-generated configs unambiguous.
- `ingest.bulk_catchup.fetch_concurrency` keeps its name and semantic (in-flight fetches).
- `ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch` caps unique transparent prevouts that a bulk-catchup commit batch may read from the canonical store. The budget counts out-of-batch spent outpoints after deduplication; prevouts produced in the same batch do not count against it.

## Invariants preserved

- **One primary writer (ADR-0003).** All commits go through one `PrimaryChainStore` handle. `spawn_blocking` moves the actual RocksDB write off the Tokio reactor but does not introduce a second writer.
- **Ordered writes plus atomic flush (ADR-0020).** Stream output order is preserved by `try_buffered`'s slot accounting; the consumer folds in height order; the commit goes through one `WriteBatch` per epoch.
- **One commit per loop iteration (ADR-0015).** The unified ingest loop classifies after every batch. The parallel-derive pipeline still commits one batch per `run_one_batch` call.
- **Spend candidate ordering.** `transparent_address_tx_index_spend_candidates` are resolved at commit time against the in-batch transactions; they survive parallel derive because the serial fold sees blocks in height order.
- **WAL ceiling cadence (ADR-0020 Tier 3).** The flush trigger fires from the commit stage after every `flush_interval_epochs` commits, regardless of how many parallel workers fed the batch.

## Consequences

Throughput scales with `ingest.derive.concurrency` until source fetch, subtree-root hydration, transparent-prevout lookup, or RocksDB commit work becomes dominant. The parsed block produced by the parallel derive phase is carried into commit-time derive contexts so the writer does not reparse raw blocks serially. Startup replay exposes its own progress metrics (`zinder_ingest_derive_replay_blocks_total`, `zinder_ingest_derive_replay_lag_blocks`, and `zinder_ingest_derive_replay_stage_duration_seconds`) so operators can distinguish replay from the normal bulk-catchup loop and identify the active bottleneck.

Memory budget: each in-flight pipeline slot holds one `SourceBlock` plus one `DerivedBlockArtifacts`, roughly 100 KB on mainnet. With `ingest.derive.concurrency = 31` the in-flight derive contribution is approximately 3 MiB. Replay holds one retained event's parsed blocks and one shared prevout map at a time, then dispatches that event serially. The dominant memory term remains the commit accumulator, now bounded by both `commit_batch_blocks × per-block-built-artifact` and `max_transparent_prevout_store_lookups_per_batch × per-prevout-row`. Total peak stays within the [bounded RocksDB resource budget](../runbooks/bulk-catchup-oom-recovery.md).

Cancellation: in-flight `spawn_blocking` derive tasks are not cancellable. When the unified loop cancels mid-batch, dropping the `try_buffered` stream orphans up to `ingest.derive.concurrency` derives that run to completion on the blocking pool and discard their results. CPU cost only; no correctness or memory pressure.

Subtree-root fetch: kept inside `CommitStage` as a serial pre-commit step. Pipelining it with the next batch's derive would require running multiple batches per loop iteration, which conflicts with the unified loop's classifier check. Accept one RPC round-trip per batch (typically 100 to 500 ms against a local Zebra). If measured subtree-root latency becomes dominant, a separate ADR can introduce multi-batch pipelining.

Tip-follow: unchanged shape. Tip-follow processes one block per poll and runs an explicit parent-hash continuity check for reorg detection. Parallelizing it would buy nothing and complicate the reorg path. The same `derive_block` and `finalize_derived_block` functions are called sequentially.

`tokio::task::spawn_blocking` thread cap: the Tokio blocking pool defaults to 512 threads. The `try_buffered(N)` slot count bounds concurrent derives to `N`. With `ingest.derive.concurrency = 31`, peak transient usage during a loop iteration is approximately 33 blocking threads (31 derives plus the commit plus the flush). Well below the cap.

## Alternatives considered and rejected

- **Bounded `tokio::sync::mpsc` channels with `N` worker tasks plus a height-keyed reorder buffer.** More flexible (work-stealing across uneven block sizes) but adds an explicit reorder primitive, manual backpressure sizing, and three task-spawn boundaries. Not justified when `try_buffered` already provides bounded-queue semantics with ordered output for a strictly linear pipeline.
- **Rayon-based parallel batch derivation.** Bursts memory to one full batch of in-flight derived artifacts (roughly 700 MiB at `commit_batch_blocks = 1000`). Conflicts with the ADR-0020 budget. A future iteration may consider this for individual within-block work (per-transaction parallelism via `rayon::in_place_scope_fifo`) following Zebra's pattern in `non_finalized_state::validate_and_update_parallel`, but not as the per-batch parallelism boundary.
- **Dedicated `std::thread::spawn` writer thread (Zebra's pattern).** Stronger isolation but adds an OS-level thread and a channel between the commit stage and the writer. Zinder's writer is mutually exclusive with the rest of the ingest path (one phase at a time), so the Tokio-runtime starvation risk this pattern hedges against is smaller here. Reconsider only if profiling shows commit-time reactor contention.

## References

- [The bulk-catchup throughput investigation](../investigations/bulk-catchup-throughput.md): the measurements that motivated this ADR and the verified hot-path analysis.
- [`services/zinder-ingest/src/artifact_builder.rs`](../../services/zinder-ingest/src/artifact_builder.rs): home of `derive_block`, `finalize_derived_block`, `DerivedBlockArtifacts`, and `CommitmentTreeSizes`.
- [`services/zinder-ingest/src/chain_ingest.rs`](../../services/zinder-ingest/src/chain_ingest.rs): home of `BuiltArtifacts` and the `IngestBatch::absorb` helper.
- [Bulk-catchup OOM recovery runbook](../runbooks/bulk-catchup-oom-recovery.md): the related WAL-replay memory bound this parallel-derive change must preserve.
