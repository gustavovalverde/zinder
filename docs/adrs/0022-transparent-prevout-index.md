# ADR-0022: Transparent prevout rows remove commit-time transaction re-reads

Status: Accepted
Date: 2026-05-20
Related: [ADR-0001](0001-rocksdb-canonical-store.md),
[ADR-0003](0003-canonical-storage-access-boundary.md),
[ADR-0021](0021-parallel-block-derivation.md)

## Context

`zinder-ingest`'s commit path runs `append_transparent_spend_tx_index_artifacts` once per batch to attach the spending-side row for each transparent input. To do that, it needs the **address that owned the spent output**. The original implementation found that address by:

1. Calling `current_chain_reader.transaction_by_id(prevout.transaction_id)` against the writer's own snapshot.
2. Loading the entire prevout transaction artifact through `multi_get` on the `transaction` CF (~10-20 KB per artifact).
3. Inside the store, reading the **entire canonical block** at the prevout's height through `read_block_artifact` to compare its `block_hash` against the transaction artifact's recorded `block_hash` (a reorg-safety check).
4. Deserializing the transaction with `zebra_chain::serialization::ZcashDeserialize`.
5. Extracting one output's `lock_script` by index.
6. Computing `TransparentAddressScriptHash::of_script_pub_key(lock_script)`.

On a 275 GB store at mainnet height ~328 K, instrumentation captured **54 GB of `finalized_block` reads and 11 GB of `transaction` reads** per ~5 commits, with a single 1000-block commit holding the writer for 251 to 541 seconds. The dominant cost was the canonical-block read inside the reorg-safety check, multiplied across ~134 K transparent inputs per batch.

Three architectural smells emerged from that measurement:

- The indexer was **re-deriving state it had already computed**. `address_script_hash` is computed once at UTXO creation and persisted in `TransparentAddressUtxoArtifact`. Recomputing it from raw transaction bytes at spend time is duplicated work.
- The writer was **paying for reorg safety it does not need**. The writer holds the control lock during commit and is itself producing the latest chain epoch; the chain cannot reorg underneath it. The block-hash verification inside `read_transaction_artifacts_batch` is defense-in-depth for query readers, not the writer.
- The indexer was **duplicating responsibilities that already live in Zebra**. Zebra owns the canonical chain and tells `zinder-ingest` about reorgs via `IndexerNotificationStream`. The indexer does not need a second canonical-chain re-verification on every internal read.

## Decision

Persist canonical transparent outputs as first-class transparent-prevout rows with two storage shapes:

- **`transparent_prevout`** is the current canonical projection, keyed by `(network, outpoint)`. It is the hot-path row family for current readers, writer commit lookups, spend indexing, and derive-context hydration.
- **`transparent_prevout_history`** is the epoch-suffixed history, keyed by `(network, outpoint, chain_epoch_id)`. It is used for pinned historical reads, reorg repair, and auditability.
- **`transparent_prevout_block_index`** is the block-local outpoint list, keyed by `(network, block_height, chain_epoch_id)`. It bounds reorg repair to the replaced height range instead of scanning the full current projection.

Each row carries the output value, raw `script_pub_key`, derived address script hash, and producing block identity. The commit pipeline resolves spent-output metadata from the exact current row instead of loading the producing transaction or stitching together UTXO and transaction artifacts.

Mechanics:

- Each transparent output emitted by `derive_block` produces a `TransparentPrevoutArtifact`. The artifact carries the outpoint, `value_zat`, `script_pub_key`, address script hash, block height, and block hash.
- `ChainEpochArtifacts::with_transparent_prevouts` and `push_transparent_prevout_artifact_puts` persist each artifact to the current projection and history column family, then persist one block-local outpoint index row per block in the same write batch.
- `ChainEpochReader::transparent_prevouts_by_outpoints(...)` uses exact `multi_get` reads against the current projection when the reader is pinned to the latest chain epoch. When the reader is explicitly pinned to an older epoch, it reverse-scans `transparent_prevout_history` under the outpoint prefix, ignores rows from later epochs, and returns the first row whose producing block is visible in that historical epoch.
- `ChainEpochReader::transparent_prevouts_by_outpoints_for_writer_commit(...)` uses the same exact current-projection read as latest readers. Only the primary writer's commit path may use it while deriving a node-validated batch against its own current epoch.
- `commit_chain_epoch` repairs the current projection during `ReorgWindowChange::Replace`: it reads the previous branch's block-local outpoint index for the replaced height range, then overwrites rows produced by reverted blocks with the newest older visible history row or deletes them when no visible row remains. The repair is part of the same atomic chain-epoch write batch and evaluates visibility against the post-reorg replacement block set.
- `append_transparent_spend_tx_index_artifacts` and derive-context assembly use the resolved `TransparentPrevoutArtifact` directly. The hot path no longer loads canonical blocks, loads prevout transactions, deserializes prevout transaction bytes, or reconstructs prevout payloads from separate row families.

For prevouts that are spent in the same batch they were created in, the writer first consults the batch's own `transparent_prevouts` vector; only out-of-batch prevouts hit the store.

The active artifact schema version is 4 and requires these rows for every mined transparent output in the canonical store. Stores created with the earlier epoch-suffixed-only shape are not migrated in place; operators wipe and re-sync because the project is pre-release and the clean schema is the contract.

## Why not the alternatives

- **Skip the canonical-block verification for finalized rows.** A targeted micro-fix would have helped, but it left intact the larger architectural smell: even with the verification removed, the writer still loads multi-KB transaction artifacts, deserializes them through zebra-chain, and extracts one output by index. The transparent prevout row eliminates the transaction read at the source.
- **Maintain a `(height) → block_hash` micro-index** so the reorg-safety check stays cheap. Same problem: it speeds up a check that should not run in the writer's hot path, and still leaves the transaction-by-id read on the critical path for every spend.
- **Trust Zebra at read time and call `getrawtransaction` to resolve prevout scripts.** Throws away the indexer's value: canonical wallet data is precisely what Zinder is supposed to materialize. Zebra does not maintain this wallet-plane row shape and would be the wrong place to ask. This option is rejected.

## Invariants preserved

- **One primary writer (ADR-0003).** Unchanged. The `transparent_prevout` family is written by the same writer through the same `WriteBatch` boundary.
- **Atomic per-chain-epoch commit (ADR-0001).** Unchanged. Transparent prevout puts are part of the same `commit_chain_epoch` `WriteBatch`.
- **Ordered writes and atomic flush (ADR-0020).** Unchanged.
- **Reorg safety.** `transparent_prevout_history` preserves ordered history for pinned readers. The current projection is repaired atomically during reorg replacement so latest readers and writer-commit lookups stay exact-key reads without carrying a per-request reverse scan.

## Consequences

Expected per-batch I/O drops dramatically:

| Per 1000-block commit batch | Before | After |
|---|---:|---:|
| `transaction` `multi_get` bytes | ~2-3 GB | 0 |
| `finalized_block` `get` bytes | ~7-11 GB | 0 |
| Transparent prevout `get` bytes | n/a | one exact current-projection row per out-of-batch spent outpoint, bounded by `ingest.bulk_catchup.max_transparent_prevout_store_lookups_per_batch` |
| Transparent prevout `put` bytes | n/a | one current row plus one history row per transparent output, plus one block-local index row per block |
| Wall-clock commit bottleneck (deployed mainnet, height ~328 K, instrumented) | 251-541 s writer-held commits | exact-row transparent-prevout reads with progress visible through `zinder_ingest_commit_stage_duration_seconds` and `zinder_store_read_duration_seconds{table="transparent_prevout"}` |

The writer hot path becomes proportional to the number of spent inputs only (not the size of the prevout transactions or canonical blocks). Bulk-catchup additionally caps each commit batch by unique transparent prevouts that must be read from the store, and commit-time lookups execute in bounded chunks with progress metrics. Combined with ADR-0021's parallel derive, the bulk-catchup ceiling shifts from "commit-bound at single-digit blocks/sec" to "fetch + derive bound at parallel-CPU blocks/sec." The same transparent prevout path also keeps in-process derive consumers from rehydrating transaction artifacts while the canonical writer is holding the commit boundary.

Storage overhead: one exact current row per currently canonical transparent output, one history row per committed transparent output, and one block-local outpoint-list row per block with transparent outputs.

Implementation surface:

- `derive_block` emits `TransparentPrevoutArtifact` values for every mined transparent output.
- `commit_chain_epoch` writes those artifacts into `transparent_prevout`, `transparent_prevout_history`, and `transparent_prevout_block_index` atomically with the rest of the epoch, and repairs the current projection during reorg replacement.
- Spend indexing and derive-context assembly resolve prevouts through `ChainEpochReader::transparent_prevouts_by_outpoints_for_writer_commit(...)` on the primary writer path.
- Query readers resolve latest wallet-plane `TransparentPrevouts` through exact current-projection reads. Pinned historical reads use the history table with block-visibility checks.

## References

- [`crates/zinder-core/src/transparent_prevout.rs`](../../crates/zinder-core/src/transparent_prevout.rs): `TransparentPrevoutArtifact` definition.
- [`crates/zinder-store/src/transparent_prevout.rs`](../../crates/zinder-store/src/transparent_prevout.rs): canonical transparent prevout reads.
- [`crates/zinder-store/src/chain_epoch_reader.rs`](../../crates/zinder-store/src/chain_epoch_reader.rs): `ChainEpochReader::transparent_prevouts_by_outpoints` and `ChainEpochReader::transparent_prevouts_by_outpoints_for_writer_commit`.
- [`services/zinder-ingest/src/artifact_builder.rs`](../../services/zinder-ingest/src/artifact_builder.rs): per-output emission of the new artifact.
- [`services/zinder-ingest/src/chain_ingest.rs`](../../services/zinder-ingest/src/chain_ingest.rs): `append_transparent_spend_tx_index_artifacts` using the new lookup.
- [`services/zinder-ingest/src/derive_consumers.rs`](../../services/zinder-ingest/src/derive_consumers.rs): derive-context prevout hydration using canonical transparent prevout rows.
- [The bulk-catchup throughput investigation](../investigations/bulk-catchup-throughput.md) and [ADR-0021](0021-parallel-block-derivation.md): the throughput context that motivated this change.
