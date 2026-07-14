# zinder-bench

Fixed-range capture and replay harness for measuring Zinder ingest changes
against identical inputs. It is the validation vehicle for held optimizations:
the windowed prevout resolver, canonical block-cache sizing, background-job
counts, and allocator experiments.

The harness holds two things constant so chain-content variance cannot reorder
conclusions: the source bytes (a captured fixture) and the starting canonical
store (a clone the operator supplies). The only time-dependent measurement is
wall-clock duration.

`zinder-bench` is a standalone binary. It links `zinder-ingest` and drives its
real bulk-catchup pipeline (block prepare, reassembly, commit); it never ships
inside a production image.

## 1. Capture a fixture

Point the harness at a synced Zebra JSON-RPC endpoint and capture a dense
range. The default range is a 50K-block window.

```bash
zinder-bench capture \
  --network zcash-mainnet \
  --json-rpc-addr http://127.0.0.1:8232 \
  --from-height 150000 \
  --to-height 200000 \
  --out ./fixtures/mainnet-150k-200k
```

Optional flags: `--node-auth-cookie <path>` for cookie auth, `--segment-blocks`
(blocks per segment file, default 1000), `--fetch-concurrency` (default 16),
`--request-timeout-secs`, `--max-response-bytes`.

The fixture directory holds one `segment-NNNNNN.bin` file per segment plus a
`manifest.json` recording the network, range, consensus activations, artifact
schema version, replay tip hash, per-segment SHA-256, and any shielded subtree
roots that complete inside the range. It also records transaction, transparent
input/output, and raw-byte density totals, populated-block counts, and
per-block maxima so a benchmark range can be reviewed for burst dominance.

## 2. Snapshot the starting store

The replay needs a canonical store already populated up to `from_height - 1`, so
the ordered prevout resolver can resolve spends of outputs created before the
captured range against the cold store. The operator supplies this clone; the
harness does not snapshot a live store for you.

Either copy a stopped store directory:

```bash
cp -a /var/lib/zinder ./fixtures/store-149999
```

or take a consistent RocksDB checkpoint of a running store with the existing
ingest backup command (hard-links, so it is cheap and space-light):

```bash
zinder-ingest backup \
  --network zcash-mainnet \
  --storage-path /var/lib/zinder \
  --to ./fixtures/store-149999
```

The clone's canonical tip must equal `from_height - 1`. Replay writes into the
clone, so use a throwaway copy per run (or per configuration in a sweep).

When the harness runs inside Docker Desktop, place RocksDB stores on named Linux volumes rather than host bind mounts. The macOS virtiofs path can report direct-I/O support while producing padded SST file sizes that fail RocksDB manifest validation on reopen. Bind mounts remain appropriate for immutable fixture inputs and JSON reports; the writable canonical and derive stores must use the container VM's ext4-backed named volumes.

## 3. Replay and read the report

```bash
zinder-bench replay \
  --fixture ./fixtures/mainnet-150k-200k \
  --store ./fixtures/store-149999 \
  --block-prepare-concurrency 16 \
  --report ./report-c16.json
```

Sweep the knobs the backlog calls out by varying flags across otherwise
identical runs (each against a fresh store clone):

- `--block-prepare-concurrency 4|8|12|24` for the prepare-concurrency sweep.
- `--block-cache-bytes <N>` for the canonical block-cache-size sweep.
- `--projection-preset wallet|complete` to drive one explicit projection
  workload over the committed range. Omit it for a canonical-only control.
- `--projection-replay-scope fixed-range|retained-history` to compare only the
  captured range (the default) or a full rebuild from retained canonical event
  history. Fixed-range replay requires a fresh derive store in the clone.

Omit `--report` to print the JSON to stdout (progress logs go to stderr).

## Report fields

- `fixture.workload_density`: the immutable workload totals and per-block
  maxima copied from the captured fixture manifest.
- `replay.wall_clock_seconds`, `replay.blocks_committed`,
  `replay.blocks_per_second`: throughput over the range.
- `replay.projection_preset`: the derive workload replayed after canonical
  ingest, or `null` for a canonical-only run.
- `replay.projection_replay_scope`: whether the projection arm measured only
  the fixed range or rebuilt all retained history.
- `replay.projection_row_count`, `replay.derive_store_bytes`: selected
  projection rows and final derive-store disk use.
- `replay.derive_bytes_written`, `replay.derive_compaction_bytes`: derive-store
  serialized write-batch bytes and compaction I/O for the run.
- `replay.projection_lag_blocks`: selected projection lag after the fixed event
  history is exhausted.
- `replay.derive_wall_clock_seconds`, `replay.derive_reopen_seconds`: projection
  replay and populated-store reopen time.
- `replay.epochs_committed`: committed chain epochs.
- `replay.commit_fallback_reads`: commit-fallback read calls; near zero confirms
  ordered prevout resolution covered the range.
- `replay.peak_rss`: peak resident bytes (Linux `/proc/self/status` `VmHWM`;
  reported as unavailable off Linux).
- `store_reads`: per-caller canonical-store read call counts and cumulative
  histogram seconds, keyed by caller (`block_prefetch`, `commit_fallback`,
  `derive_hydration`, `retention_sweep`, `query`), table, and operation.
- `multi_get`: per-caller requested and resolved key totals.
- `stage_durations`: cumulative task seconds and call counts for block-prepare
  stages (`artifact_derive`, `transparent_prevout_resolve`) and block-derive
  stages (`block_parse`, `identity_validation`, `compact_artifacts`,
  `transparent_output_artifacts`, `transaction_artifacts`,
  `block_header_artifact`, and `block_blob_artifact`).
- `rocksdb_tickers`: exported `RocksDB` statistics tickers (bloom, block cache,
  bytes read/written, stall micros, compaction bytes) per store role.

## Scope and faithfulness

- Source transport differs from production (fixture files instead of JSON-RPC),
  so source-fetch timing is not representative. Fixture replay parses only the
  block header before handing the payload to canonical preparation, matching the
  production batch-source boundary. Everything downstream (prepare, prefetch,
  reassembly, commit, and derive) runs the real pipeline against the real
  canonical store.
- Shielded subtree roots that complete inside the range are captured verbatim
  and served during replay, so post-Sapling ranges commit correctly.
- Sparse tree-state checkpoints are not captured; the fixture source does not
  advertise the tree-state capability, so the pipeline skips them. They are not
  on the transparent hot path the backlog targets.
- The bulk-catchup configuration uses production-representative defaults from
  `zinder_ingest::bench_support`; only the swept knobs vary between runs.
