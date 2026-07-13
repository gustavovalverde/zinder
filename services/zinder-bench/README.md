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
roots that complete inside the range.

## 2. Snapshot the starting store

The replay needs a canonical store already populated up to `from_height - 1`, so
the prefetch stage can resolve prevouts spent inside the range that were created
before it. The operator supplies this clone; the harness does not snapshot a live
store for you.

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
- `--derive` to also drive derive replay over the committed range.

Omit `--report` to print the JSON to stdout (progress logs go to stderr).

## Report fields

- `replay.wall_clock_seconds`, `replay.blocks_committed`,
  `replay.blocks_per_second`: throughput over the range.
- `replay.epochs_committed`: committed chain epochs.
- `replay.commit_fallback_reads`: commit-fallback read calls; near zero confirms
  the prefetch stage resolved the range's prevouts.
- `replay.peak_rss`: peak resident bytes (Linux `/proc/self/status` `VmHWM`;
  reported as unavailable off Linux).
- `store_reads`: per-caller canonical-store read call counts and cumulative
  histogram seconds, keyed by caller (`block_prefetch`, `commit_fallback`,
  `derive_hydration`, `retention_sweep`, `query`), table, and operation.
- `multi_get`: per-caller requested and resolved key totals.
- `stage_durations`: cumulative task seconds and call counts for block-prepare
  stages (`artifact_derive`, `transparent_prevout_prefetch`) and block-derive
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
