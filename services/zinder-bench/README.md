# zinder-bench

Fixed-range capture and storage benchmark harness for measuring Zinder against
identical source bytes. It drives the current projection-coupled replay oracle
and two backend-neutral canonical-fact round trips: one sorted external SST
ingestion for `RocksDB` and binary `COPY` with deferred index construction for
PostgreSQL.

The captured fixture is the common input for all three arms. Current-schema
replay also requires a matching starting canonical store supplied by the
operator. Fact-first arms create fresh candidate storage and compare every
persisted semantic replay envelope against the fixture's ordered digest oracle.

`zinder-bench` is a standalone binary. It links `zinder-ingest` and drives its
real bulk-catchup pipeline (block prepare, reassembly, commit); it never ships
inside a production image.

Use the [storage benchmark environment](../../deploy/storage-benchmark.md) for
the resource-bounded Docker Compose topology and repeatable storage-candidate
runs.

## 1. Capture a fixture

Point the harness at a synced Zebra JSON-RPC endpoint and capture a dense
range. The default range is an inclusive 50,001-block window.

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
`manifest.json` recording the network, range, consensus activations,
current-schema oracle artifact version, replay tip hash, per-segment SHA-256,
and any shielded subtree roots that complete inside the range. Fixture format
version 1 records the versioned `CanonicalBlockFacts` block-digest and
ordered sequence-digest evidence used by both fact-first arms. Workload totals
and per-block maxima let reviewers detect burst-dominated ranges.

## 2. Snapshot the starting store

The replay needs a canonical store already populated up to `from_height - 1`, so
the ordered prevout resolver can resolve spends of outputs created before the
captured range against the cold store. The operator supplies this clone; the
harness does not snapshot a live store for you.

For an unthresholded exploratory run, copy a stopped store directory while
excluding its projection subdirectory:

```bash
rsync -a --exclude '/derive/' /var/lib/zinder/ ./fixtures/store-149999/
```

For a threshold-bearing run, take a consistent RocksDB checkpoint with the
existing ingest backup command (hard-links, so it is cheap and space-light):

```bash
zinder-ingest backup \
  --network zcash-mainnet \
  --storage-path /var/lib/zinder \
  --to ./fixtures/checkpoint-149999
rsync -a --exclude '/derive/' \
  ./fixtures/checkpoint-149999/ \
  ./fixtures/store-149999/
```

The backup command creates a canonical-plus-projection bundle. Projection
benchmarks require a fresh projection store for both fixed-range and
retained-history construction, so every throwaway replay clone must exclude
the bundle's `derive/` subdirectory. The harness rejects a pre-existing
projection path instead of silently timing an incremental catch-up. The
clone's canonical tip must equal `from_height - 1`. Replay writes into the
clone, so use one throwaway copy per run (or per configuration in a sweep).

Threshold-bearing non-genesis ranges require the backup's raw
`zinder-backup-manifest.json`. The harness hashes it before opening the store
and verifies its network, `ChainEpochId`, tip height, tip hash, and artifact
schema against the opened canonical store. A thresholded fixture beginning at
height 1 instead requires a genuinely empty store and does not require a
manifest. The report distinguishes `empty`, `checkpoint`, and
`unverified-clone` starting states, so a missing manifest cannot masquerade as
checkpoint provenance. The manifest proves the backup identity and logical
starting position; it does not prove byte-identical SST layout or compaction
state. Formal sweeps must create every throwaway arm from the same backup
directory. Unthresholded manual clones may omit the manifest.

When the harness runs inside Docker Desktop, place RocksDB stores on named
Linux volumes rather than host bind mounts. The macOS virtiofs path can report
direct-I/O support while producing padded SST file sizes that fail RocksDB
manifest validation on reopen. Bind mounts remain appropriate for immutable
fixture inputs and JSON reports; the writable canonical and projection stores
must use the container VM's ext4-backed named volumes.

## 3. Run the current-schema replay oracle

```bash
zinder-bench current-schema-replay \
  --fixture ./fixtures/mainnet-150k-200k \
  --store ./fixtures/store-149999 \
  --block-prepare-concurrency 16 \
  --software-revision "$(git rev-parse HEAD)" \
  --trial-id trial-01 \
  --fixture-cache-policy warm \
  --runner-id linux-amd64-c8-m16-nvme-01 \
  --cpu-limit-cores 8 \
  --memory-limit-bytes 17179869184 \
  --storage-class local-nvme \
  --image-reference "zinder-bench@sha256:<64-hex-digest>" \
  --report ./report-c16.json
```

Sweep the knobs the backlog calls out by varying flags across otherwise
identical runs (each against a fresh store clone):

- `--block-prepare-concurrency 4|8|12|24` for the prepare-concurrency sweep.
- `--block-cache-bytes <N>` for the canonical block-cache-size sweep.
- `--projection-preset wallet|explorer` to drive one explicit projection
  diagnostic over the committed range. Neither preset builds ADR-0035's wallet
  plane: they do not own its live set, balances, address index, undo state, or
  readiness contract. `explorer` also does not drive projection-startup
  historical backfills or the coverage verifier. Neither preset can produce a
  wallet construction or wallet-ready acceptance result.
- `--projection-replay-scope fixed-range|retained-history` to compare only the
  captured range (the default) or a full rebuild from retained canonical event
  history. Fixed-range replay requires a fresh projection store in the clone.

`--software-revision` identifies the source revision. `--image-reference` must
be either a `sha256:<64-hex>` container image ID or a digest-pinned image
reference containing `@sha256:<64-hex>`; mutable tags are rejected.
`--runner-id` is only a stable operator label. The report separately records
`--cpu-limit-cores`, `--memory-limit-bytes`, and `--storage-class`, plus the
effective writer schema, full RocksDB budget, and durability posture. Every
threshold-bearing run requires all of this provenance and an installed metrics
recorder. Exploratory unthresholded runs may omit it.

The only acceptance boundary this command currently drives is canonical
fixture replay: the timed call replays exactly the captured range into the
supplied current-schema store. It does not open, build, validate, or promote a
fresh database, so it is not a fresh canonical construction lifecycle. Opt in
with the paired flags:

- `--canonical-fixture-replay-target-secs`;
- `--canonical-fixture-replay-hard-limit-secs`.

The target must be positive and no greater than the hard limit. Thresholded
canonical fixture replay is canonical-only and rejects `--projection-preset`,
which keeps projection caches and work outside the accepted measurement. The
report validates final height, fixture-order tip hash, committed block count,
and required telemetry before evaluating time. Production snapshot restore,
fact-first canonical construction, projection construction, following, and
wallet readiness gain report fields only when their real drivers own those
full boundaries.

The harness writes the JSON report before enforcing acceptance, but creates the
path exclusively and refuses to replace existing evidence. A completion,
telemetry, or hard-limit failure then makes the command exit non-zero. A
target-only miss remains successful so operators can compare performance inside
the accepted hard boundary. Choose a new report path for every run.

Omit `--report` to print the JSON to stdout (progress logs go to stderr).

## 4. Compare canonical-fact storage engines

The fact-first commands parse the same fixture into the same complete
`CanonicalBlockFacts` values. Each engine writes the same independently
versioned semantic replay format, reads every row back, reconstructs the full
aggregate, rejects non-canonical encodings, recomputes the independent
per-block and ordered sequence digests, and publishes a completion fence only
after validation.

The `RocksDB` arm requires a path that does not exist:

```bash
zinder-bench canonical-facts-round-trip rocksdb \
  --fixture ./fixtures/mainnet-150k-200k \
  --store ./candidates/rocksdb-fact-first \
  --block-prepare-concurrency 16 \
  --software-revision "$(git rev-parse HEAD)" \
  --trial-id trial-01 \
  --fixture-cache-policy warm \
  --runner-id linux-amd64-c8-m16-nvme-01 \
  --cpu-limit-cores 8 \
  --memory-limit-bytes 17179869184 \
  --storage-class local-nvme \
  --image-reference "zinder-bench@sha256:<64-hex-digest>" \
  --report ./rocksdb-fact-first-trial-01.json
```

The PostgreSQL arm reads its URL from an environment variable so credentials
never appear in command arguments or reports. It rejects a database where its
candidate schema already exists. The endpoint must be operator controlled; the
benchmark client does not treat a malicious PostgreSQL server as an input:

```bash
export ZINDER_BENCH_POSTGRES_DATABASE_URL='postgresql://...'
zinder-bench canonical-facts-round-trip postgres \
  --fixture ./fixtures/mainnet-150k-200k \
  --database-url-env ZINDER_BENCH_POSTGRES_DATABASE_URL \
  --block-prepare-concurrency 16 \
  --software-revision "$(git rev-parse HEAD)" \
  --trial-id trial-01 \
  --fixture-cache-policy warm \
  --runner-id linux-amd64-c8-m16-nvme-01 \
  --cpu-limit-cores 8 \
  --memory-limit-bytes 17179869184 \
  --client-cpu-limit-cores 2 \
  --client-memory-limit-bytes 8589934592 \
  --database-cpu-limit-cores 6 \
  --database-memory-limit-bytes 8589934592 \
  --database-image-reference "postgres@sha256:<64-hex-digest>" \
  --storage-class local-nvme \
  --image-reference "zinder-bench@sha256:<64-hex-digest>" \
  --report ./postgres-fact-first-trial-01.json
```

The CPU and memory values on a fact report describe the complete benchmark
arm. PostgreSQL requires the client and database limits together, and their
exact sum must equal the aggregate arm limits. Its report also identifies the
database image independently from the benchmark-client image.

The concrete driver gate creates its own small fixture and requires only a
fresh disposable database URL:

```bash
ZINDER_TEST_POSTGRES_DATABASE_URL='postgresql://zinder_bench:zinder_bench_local_only@127.0.0.1:55432/zinder_bench' \
  cargo nextest run -p zinder-bench --profile=ci-postgres --run-ignored=all
```

Selection runs require a repeated, alternating-order campaign and an explicit
fixture page-cache policy. Fresh candidate volumes do not clear the host cache
for the bind-mounted fixture. The deployment runbook defines the minimum trial
count, unique trial IDs, warm/cold policy, and 5-artifact campaign ledger. Each
trial retains the RocksDB report and resource observation plus the PostgreSQL
report, client observation, and database observation. Run
`scripts/validate-storage-benchmark-campaign.sh` to reject inconsistent evidence
and compute candidate medians/minimums/maximums; a single report pair is
diagnostic only. The PostgreSQL database observation is complete only after the
server stops.

These commands prove a persisted `CanonicalBlockFacts` round trip only. They
do not persist compact blocks, tree state, subtree roots, `ChainEpoch`, or
`ChainEvent`; exercise reorgs; build wallet or explorer projections; serve
queries; measure fresh canonical construction; or certify either deployment
topology.

End-to-end throughput compares the complete deployment arms under their stated
resource allocations; it is not an isolated database-engine score. The
PostgreSQL client and server have a fixed resource partition, while the RocksDB
process owns its whole arm budget. Use phase timings to explain work within each
arm, not as interchangeable engine microbenchmarks; cross-arm conclusions need
end-to-end time, storage bytes, digest equality, and resource evidence together.
The campaign summary's comparable high-water metrics are
`sampled_whole_arm_memory_peak_bytes` and
`sampled_whole_arm_storage_peak_bytes`. PostgreSQL memory is the maximum of
time-aligned client-plus-database `memory.current` samples, never the sum of
independent component peaks. The exact cgroup `memory.peak` values remain
component diagnostics in the external resource artifacts.

## Report fields

- `report_format_version`: machine-readable report contract version. The
  closed measurement contract described here is version 1.
- `measurement_kind`: either `current-schema-fixture-replay` or
  `canonical-block-facts-round-trip`. The tagged shape prevents fact-only
  evidence from acquiring placeholder lifecycle or current-schema telemetry
  fields.
- `provenance`: benchmark version, software revision, immutable image identity,
  build target OS/architecture, structured runner identity and resources, plus
  `run.trial_id`, `run.fixture_cache_policy`, and binary-generated start and
  completion Unix-millisecond timestamps.
- `storage_candidate`: identifies `rocksdb-current-schema-oracle`,
  `rocksdb-fact-first`, or `postgres-fact-first`, including the logical model
  and the `rocksdb-single-host` or `postgres-scale-out` topology represented by
  that arm. This is candidate identity, not topology certification.
- `acceptance.canonical_fixture_replay`: `fixture-range` wall time and optional
  target/hard-limit evaluation. It is the only current acceptance boundary.
  There are no placeholder production lifecycle fields.
- `fixture.fixture_format_version`,
  `fixture.current_schema_oracle_artifact_schema_version`,
  `fixture.canonical_block_facts_digest_evidence`, `fixture.tip_hash_hex`, and
  `fixture.digest_sha256`: fixture provenance required to compare candidates
  against identical source bytes and digest contracts. Each driver verifies
  every segment SHA-256 before writing its rows.
- `fixture.workload_density`: the immutable workload totals and per-block
  maxima copied from the captured fixture manifest.
- `replay.wall_clock_seconds`, `replay.blocks_committed`,
  `replay.blocks_per_second`: throughput over the range.
- `replay.starting_canonical_state`: the opened store's `chain_epoch_id`, tip
  height, RPC-order tip hash, artifact schema version, checkpoint-manifest
  SHA-256, and `empty`, `checkpoint`, or `unverified-clone` provenance kind.
  The manifest digest identifies the manifest, not physical SST or compaction
  layout.
- `replay.tip_height_after`, `replay.tip_hash_after_hex`: final fixture range
  position. The hash uses the same internal-byte-order hex as
  `fixture.tip_hash_hex`; thresholded acceptance requires both to match.
- `replay.canonical_writer`: actual canonical store/artifact schema versions,
  sync-write and WAL/fsync durability mode, and the complete effective RocksDB
  resource budget, including the default cache size when no override is given.
- `replay.projection_preset`: the projection workload replayed after canonical
  ingest, or `null` for a canonical-only run.
- `replay.projection_replay_scope`: whether the projection arm measured only
  the fixed range or rebuilt all retained history.
- `replay.projection_row_count`, `replay.projection_store_bytes`: selected
  projection rows and final projection-store disk use.
- `replay.projection_logical_write_bytes`,
  `replay.projection_compaction_bytes`: serialized projection WriteBatch bytes
  and compaction I/O for the run. Logical write bytes are not WAL bytes;
  compaction bytes are `null` when both required RocksDB ticker families were
  not covered.
- `replay.projection_event_cursor_at_tip`: `true` only after every selected
  cursor in the reopened projection store equals canonical `LiveTail`.
- `replay.projection_build_wall_clock_seconds`,
  `replay.projection_store_reopen_seconds`: projection construction and
  populated-store reopen time.
- `replay.epochs_committed`: committed chain epochs, or `null` when its metric
  family was not covered.
- `replay.commit_fallback_reads`: commit-fallback read calls, or `null` without
  explicit store-read telemetry coverage. Thresholded acceptance requires both
  telemetry values.
- `replay.peak_rss`: peak resident bytes (Linux `/proc/self/status` `VmHWM`;
  reported as unavailable off Linux).
- `store_reads`: per-caller canonical-store read call counts and cumulative
  histogram seconds, keyed by caller (`block_prefetch`, `commit_fallback`,
  `derive_hydration`, `retention_sweep`, `query`), table, and operation.
- `multi_get`: per-caller requested and resolved key totals.
- `stage_durations`: cumulative task seconds and call counts for block-prepare
  stages (`canonical_block_prepare`, `transparent_prevout_resolve`) and
  canonical-construction stages (`block_parse`, `identity_validation`, `compact_artifacts`,
  `transaction_facts`, `block_header_artifact`, `block_blob`, and `block_replay`).
- `rocksdb_tickers`: exported `RocksDB` statistics tickers (bloom, block cache,
  bytes read/written, stall micros, compaction bytes) per store role.
- `round_trip`: fact-only wall time plus shared initialization, preparation,
  persistence, index-construction, storage-optimization, validation,
  publication, fresh-reader-validation, and storage-measurement phase times. Any
  framework overhead between those timers is explicit as
  `unattributed_wall_clock_seconds`. The report also records range identity,
  block rate, logical and physical bytes, persisted sequence digest,
  digest-match result, `replay_format_version`, full semantic-replay validation,
  and process peak RSS. These names provide a common diagnostic vocabulary,
  not interchangeable engine microbenchmarks. In the fresh-reader phase
  RocksDB closes and reopens its files, while PostgreSQL closes the client
  connection and reconnects to the running server; neither result certifies a
  database-server restart.
- `round_trip.storage`: engine-specific evidence. The `RocksDB` variant records
  its schema, external-SST bytes, explicit compression, bounded resource budget,
  durability mode, the resolved database I/O mode, and the separately recorded
  buffered external-SST construction mode. The PostgreSQL variant records
  schema, explicit `lz4` replay-encoding compression, the queried comparison
  and durability server settings, table/index/WAL bytes, both component resource
  shares, and the immutable database image identity. Retain the rendered Compose
  model for initdb, shared-memory, and temporary-filesystem evidence.

Container resource observations are separate, versioned evidence artifacts
rather than fields inside the process-generated report. This keeps the report
honest about what the benchmark process can observe while allowing the campaign
validator to prove private component-cgroup scope, exact candidate storage
roots, and complete report-window coverage for every component.

## Scope and faithfulness

- Source transport differs from production (fixture files instead of JSON-RPC),
  so source-fetch timing is not representative. Fixture replay parses only the
  block header before handing the payload to canonical preparation, matching the
  production batch-source boundary. Everything downstream (prepare, prefetch,
  reassembly, commit, and projection construction) runs the real pipeline
  against the real canonical store.
- Shielded subtree roots that complete inside the range are captured verbatim
  and served during replay, so post-Sapling ranges commit correctly.
- Sparse tree-state checkpoints are not captured; the fixture source does not
  advertise the tree-state capability, so the pipeline skips them. They are not
  on the transparent hot path the backlog targets.
- The bulk-catchup configuration uses production-representative defaults from
  `zinder_ingest::bench_support`; only the swept knobs vary between runs.
- Fact round trips use the production block parser and complete
  `CanonicalBlockFacts` semantic replay format, but their physical schemas are
  diagnostic vertical slices rather than production canonical stores.
- The PostgreSQL slice uses the exact production-intended `tokio-postgres`
  driver. The prescribed Compose and CI clusters require SCRAM-SHA-256 host
  authentication; an arbitrary operator-supplied database URL does not itself
  prove the session's authentication method. `NoTls` is limited to the isolated
  benchmark network; production remote transport still requires a
  certificate-validated TLS connector.
