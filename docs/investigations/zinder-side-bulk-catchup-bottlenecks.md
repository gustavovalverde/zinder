# Zinder-side bulk catchup bottlenecks and implementation record

Status: Implemented architecture, deployed to mainnet stack, benchmark validated
Date: 2026-05-24
Updated: 2026-05-25
Scope: `zinder-ingest`, `zinder-source`, canonical storage pressure, derive replay pressure, deployment validation
Related:
[Fact-first indexer](../architecture/fact-first-indexer.md),
[Chain ingestion](../architecture/chain-ingestion.md),
[Node source boundary](../architecture/node-source-boundary.md),
[Service operations](../architecture/service-operations.md),
[ADR-0022](../adrs/0022-resource-budgeted-bulk-catchup.md)

This document is the implementation record for the Zinder-side bulk-catchup
architecture. It assumes no backward-compatibility requirement for unreleased
native APIs, config keys, metrics, module names, or docs. The goal is to remove
fixed-unit and coupled-loop baggage from initial sync, keep public boundaries
fact-first, and make the system able to explain its own throughput through
metrics.

The implemented architecture is a resource-budgeted staged pipeline:
`NodeSource::fetch_chain_segment`, JSON-RPC segment fetching behind
`NodeSource`, checkpoint-only tree state, source byte prefetch, adaptive segment
sizing, unordered source completion with ordered reassembly, bounded parallel
canonical block prepare, spent-transparent-output prefetch before commit,
overlapped commit under commit-reassembly watermarks, canonical batch artifact
byte limits, estimated canonical write-byte limits, runtime memory gauges, and
derive replay degrade-before-pause.

## 2026-05-25 clean-sync transparent-input validation

The clean-sync validation used the disposable Docker volume
`zinder-mainnet-clean-sync-20260525092628-data`; the original
`zinder-mainnet-data` volume was left untouched. The clean run first exposed the
May 2017 transparent-input range as the remaining bottleneck: around
`106000..107592`, batches were closing after only `4..27` blocks because the old
transparent-spend-reference budget fired before source fetch or block
derivation could amortize work. That range advanced at about `3.38 blocks/s`,
with transparent-output `multi_get` calls dominating commit time.

The first fix replaced the raw transparent-spend-reference close trigger with
`canonical_batch_max_estimated_write_bytes` plus
`canonical_batch_min_blocks_before_estimated_write_close`. That removed tiny
batch churn, but it did not meet the target by itself: the commit path still had
to resolve hundreds of thousands of spent transparent outputs before each
RocksDB write.

The final fix moved spent-transparent-output hydration into canonical block
prepare. Each prepared block carries any already-visible spent outputs into
commit reassembly; `commit_ingest_batch` still performs the authoritative
fallback lookup for same-batch outputs and outputs that became visible after an
overlapped previous commit. This keeps the commit boundary correct while moving
historical RocksDB reads out of the serial commit path.

Deployment evidence:

| Window | Height delta | Wall time | Rate | Notes |
| ------ | ------------ | --------- | ---- | ----- |
| First prefetch deploy | `112592 -> 166592` | 394.6s | 136.8 blocks/s | 54 block-count commits; `estimated_write_bytes` triggers stayed at 0. |
| Rolling sample | `129592 -> 165592` | 301s | 119.6 blocks/s | Transparent-output read time continued accumulating, but commit height kept advancing. |
| Final-name deploy | `186592 -> 199592` | 241s | 53.9 blocks/s | Validated final `block_prepare_*` config and metric namespace. |
| Slowest 60s slice observed | `191592 -> 193592` | 60s | 33.3 blocks/s | Still about 9.9x the `3.38 blocks/s` clean-sync pathological baseline. |

The clean-sync target of at least `2x..3x` was exceeded in the stressed
transparent-input range. The remaining optimization surface is no longer commit
serialization; it is total transparent-output lookup work. If this range becomes
the dominant production cost again, the next architectural step is a bounded
windowed prevout-hydration stage that deduplicates outpoints across adjacent
prepared blocks before issuing RocksDB reads. Do not reintroduce a raw
transparent-spend-reference batch budget; it optimizes the wrong unit and
creates tiny commits in exactly the historical range that needs amortization.

Reader deployment validation:

- Query and explorer secondaries OOM-restarted under the old `2g` mainnet
  cgroup while clean sync was advancing quickly. The failure was outside the
  writer critical path, but it made the end-to-end Compose stack unstable.
- Reader defaults now use a secondary-specific RocksDB posture and a `1000 ms`
  catchup cadence; mainnet Compose sets query and explorer to `4g` cgroups.
- After redeploying those reader settings against the same clean-sync volume,
  query and explorer stayed healthy with zero restarts beyond the prior failure
  window, while RSS sat around `1.6..1.8 GiB` per reader and ingest continued
  committing.

## 2026-05-25 finalization checkpoint

The final architecture was rebuilt, deployed, and validated against the local
mainnet stack. The final `zinder-ingest:latest` rebuild exported image manifest
`sha256:9d6e60453a0cc8605c43ab38e311574b70e2e8d92cc8ca8eeb764a626167393f`
and manifest list
`sha256:0272b37728ad78e23048d456c80420a84dbd1a05ed46233b4b460f54a383d808`.
The colocated reader image manifest lists are
`zinder-query:latest@sha256:915153732db53c9da2be2ec139321248e0ecb98428c1d1783a4226d8c600eb2f`
and
`zinder-explorer:latest@sha256:6b6a7c0bc6b1634d71cf2650985d4878e97e142532657ab15b5b9a3862450097`.
The compatibility image, rebuilt after the canonical-only storage cleanup, is
`zinder-compat-lightwalletd:latest@sha256:5eaafe8a45906e89d1767e779376aeec582d6936cc8a7c33e3ec4eb94ea1a540`.
`/readyz` is served on `127.0.0.1:9105` and reported after the final deploy:

```json
{"status":"ready","cause":"ready","current_height":3353078,"target_height":3353078}
```

The initial production baseline on 2026-05-24 was `6.94 blocks/s` over 1 hour,
with `1,535,466` blocks remaining and an ETA of about `61.4 hours`. After the
staged pipeline and bulk-catchup budgets were deployed, the writer advanced
from height `1,817,432` at `1779657600` to visible tip height `3,352,935` at
`1779668400`: `1,535,503` blocks in 10,800 seconds, about `142.2 blocks/s`.
That is roughly `20x` the original 1-hour baseline. The fastest sustained
catchup window in the same deployment advanced from `1,921,432` to `3,352,935`
between `1779663300` and `1779668400`: `1,431,503` blocks in 5,100 seconds,
about `280.7 blocks/s`, roughly `40x` the baseline.

Current 30-minute and 60-minute writer rates are intentionally near zero
because the stack is at tip. Treat those windows as tip-follow idleness, not as
bulk-catchup throughput evidence.

After the strict source-watermark audit, the source fetch stage now reserves
`node.max_response_bytes` before each active request and rejects
`source_fetch_max_in_flight_bytes < node.max_response_bytes`. That keeps the
declared byte watermark ahead of the allocation rather than shrinking only
after the response is already decoded.

The final isolated benchmark used a disposable Docker volume, the existing
mainnet Zebra, checkpoint height `3,300,000`, and target height `3,350,000`.
It committed `50,000` blocks in `42.447299533` seconds, about
`1,177.9 blocks/s`, which is roughly `169.7x` the original `6.94 blocks/s`
baseline. The benchmark volume was removed after validation. A mid-run sample
showed source fetch active reservations at `5`, source queue bytes at
`77,897,814`, working-set memory at `1,592,229,888` bytes, scheduler memory
pressure at `0.1059`, raw cgroup pressure at `0.1308`, WAL bytes at
`11,795,914 / 268,435,456`, and derive replay state `normal=1`,
`degraded=0`, `paused=0`.

Post-deploy steady-state validation showed all colocated services ready:
ingest and query at height `3,353,078`, explorer ready, scheduler memory
pressure `0.0041`, raw cgroup pressure `0.0221`, working-set memory
`60,985,344` bytes, WAL bytes `0`, and derive replay state `normal=1`,
`degraded=0`, `paused=0`.

Final contract cleanup removed two pieces of baggage found during the audit:
`zinder-compat-lightwalletd` now accepts and prints only canonical-secondary
storage config, and its build-time git commit injection no longer uses a
`ZINDER_*` runtime-config prefix.

### Source transport decision

Keep JSON-RPC for this architecture revision. During the high-throughput
catchup window at `1779667800`, `fetch_chain_segment` averaged `0.114 s` and
had p95 `0.297 s`; `fetch_tree_state_for_block` averaged `0.0036 s`. Source
transport was no longer the wall-time limiter once unordered source completion,
block-prepare concurrency, commit overlap, and byte watermarks were active.

The replacement path remains explicit: if future long-window evidence on a
healthy Zebra shows JSON-RPC p95 dominating wall time while ingest CPU,
block-prepare, commit, flush, and memory have headroom, replace the adapter behind
`NodeSource`. Do not leak `JsonRpcBatch`, `ZebraBatch`, or transport-specific
names into ingest, store, query, or derive.

### Derive replay decision

Post-catchup validation found a different end-to-end issue: canonical ingest
reached tip, but derive replay could remain paused under high cgroup pressure.
The deployed defaults now keep canonical-first behavior while avoiding
projection starvation:

| Knob | Value |
| --- | ---: |
| `memory_degrade_ratio` | `0.90` |
| `memory_pause_ratio` | `0.99` |
| `memory_resume_ratio` | `0.80` |
| `min_replay_batch_blocks` in mainnet deploy config | `50` |

After redeploy, derive replay was no longer paused:

| Signal | Value |
| --- | ---: |
| derive replay budget state | `normal=1`, `degraded=0`, `paused=0` |
| effective replay batch | `500 blocks` |
| derive replay rate | `152.4 blocks/s` over 2 minutes after the final rebuild deploy |
| derive replay lag | `1,157,553 blocks` and decreasing from the earlier `1,293,550` snapshot |
| memory pressure | `0.248` |

This validates the architectural goal: canonical catchup remains primary, but
rebuildable derive projections keep draining once the writer is at tip instead
of staying opaque and paused.

### Implementation state

| Phase | State | Notes |
| --- | --- | --- |
| Measurement contract | Implemented | `observability-smoke.sh snapshot` reports writer rates, source averages, bulk-stage p95, queues, memory, derive state, and commit/query counters. |
| Stage module extraction | Implemented | `bulk_catchup/mod.rs` is the facade; source fetch, block prepare, commit reassembly, flush, and watermarks have owned modules. |
| Byte-watermark foundation | Implemented | Source, block-prepare, and commit-reassembly memory are bounded and separately observable. Source fetch reserves `node.max_response_bytes` before each active request and keeps completed out-of-order bytes reserved until emission. |
| Unordered source fetch | Implemented | Source segments complete out of order and emit in canonical order through source reassembly. |
| Independent canonical block prepare | Implemented | Block prepare uses bounded concurrency and emits ordered blocks after out-of-order completion. |
| Commit reassembly and flush overlap | Implemented | One canonical commit can run while bounded source/fact work fills the next batch. |
| Derive replay degrade-before-pause | Implemented and tuned | Current deploy uses `0.90/0.99/0.80` memory ratios and validates active replay at tip. |
| Derive replay data-path cleanup | Not required for the 2x/3x target | Current replay rate is above the original writer baseline; revisit only if projection drain becomes the user-visible bottleneck. |
| JSON-RPC replacement | Deferred by evidence | Keep JSON-RPC behind `NodeSource`; replacement remains a future adapter decision, not an ingest contract change. |
| Allocator/RocksDB memory partitioning | Deferred by evidence | Tune only after derive backlog drains and memory pressure is stable across a long window. |

## One-hour production evidence

The following measurements were captured from the local mainnet deployment on
2026-05-24 against Prometheus at `127.0.0.1:9095`. Unless stated otherwise,
rates and histogram windows use the trailing 1 hour.

### Sync pace

| Signal | Value |
| --- | ---: |
| writer height | `1,817,432` |
| upstream target height from ingest readiness | `3,352,898` |
| remaining lag | `1,535,466 blocks` |
| 5-minute writer rate | `6.67 blocks/s` |
| 15-minute writer rate | `6.67 blocks/s` |
| 30-minute writer rate | `6.67 blocks/s` |
| 1-hour writer rate | `6.94 blocks/s` |
| 1-hour writer delta | `25,000 blocks` |
| ETA at 1-hour rate | `61.4 hours`, about `2.6 days` |

The 5-minute, 15-minute, and 30-minute windows now agree, which makes the
current rate more trustworthy than earlier mixed-era windows. The system is
not stalled, but it is still far below the single-digit-hour mainnet target.

### Source fetch and queue pressure

| Signal | Value |
| --- | ---: |
| `fetch_chain_segment` average | `9.34 s` |
| `fetch_chain_segment` p95 | `10.0 s` |
| Zebra `batch_getblock` average | `8.92 s` |
| Zebra `batch_getblock` p95 | `10.0 s` |
| source segment request rate | `0.372 req/s` |
| Zebra `batch_getblock` request rate | `0.379 req/s` |
| source payload throughput | `11.34 MB/s` |
| average segment blocks | `18.86 blocks` |
| average segment payload | `30.49 MB` |
| response-too-large splits | `13.05 / hour` |
| current next segment size | `10 blocks` |
| average source queue requests | `4.41` |
| max source queue requests | `5` |
| average source queue bytes | `222.09 MB` |
| max source queue bytes | `251.66 MB` |
| average active source fetches | `3.4` |
| max active source fetches | `5` |

Source fetch still explains most of the pace. At roughly 19 blocks per segment
and 9.3 seconds per source segment, the observed throughput lines up with a
source-limited pipeline even with several requests in flight. The queue is
bounded and active, so the problem is not that the writer is idle. The problem
is that the current ordered fetch and block-prepare stream cannot use later
completed work while the next required lower-height source segment is slow.

### Block prepare, commit, and flush pressure

| Signal | Value |
| --- | ---: |
| block-prepare average | `0.651 s/block` |
| block-prepare p95 | `2.73 s/block` |
| block-prepare completion rate | `7.01 blocks/s` |
| commit average | `3.54 s/epoch` |
| commit p95 | `9.35 s/epoch` |
| commit rate | `0.00725 commits/s`, about `26.1 commits/hour` |
| store commit average | `1.48 s` |
| flush average | `0.799 s` |
| flush p95 | `2.5 s` |
| `await_block_prepare` average | `0.139 s` |
| `await_block_prepare` p95 | `0.905 s` |
| tree-state checkpoint fetch average | `0.086 s` |
| subtree-root population average | `0.007 s` |
| block-count batch closes | about `24 / hour` |
| transparent-spend-reference batch closes | about `1 / hour` |

Commit and flush are visible but not the current ceiling. The writer is closing
mostly 1,000-block epochs, and store commit work is much smaller than source
fetch time. Block prepare is close to the writer rate, so the next pipeline must
let source fetch, block prepare, and commit proceed independently under byte
watermarks instead of hiding one stage behind the other.

### Memory and derive replay pressure

| Signal | Value |
| --- | ---: |
| memory pressure current | `0.991` |
| memory pressure average | `0.884` |
| memory pressure max | `0.9999` |
| cgroup `memory.current` current | `14.90 GB` |
| cgroup `memory.max` | `15.03 GB` |
| cgroup swap current | `0` |
| process RSS | `9.22 GB` |
| process anonymous RSS | `9.20 GB` |
| derive replay paused current | `1` |
| derive replay paused max | `1` |
| derive replay lag current | `0 blocks` |
| derive replay block rate | `5.30 blocks/s` |
| derive `read_transparent_spend_facts` average | `0.145 s` |
| derive `hydrate_blocks` average | `0.204 s` |
| derive `dispatch_event` average | `0.054 s` |

The derive tailer appears caught up to the local canonical height, but it is
still paused because memory pressure is near the hard cgroup limit. That means
derive lag is not the immediate throughput limiter in this snapshot, but the
pause model is still too coarse. `canonical-first` currently jumps to pause at
high pressure instead of shrinking replay work before it stops.

### Container and upstream-node state

| Signal | Value |
| --- | ---: |
| ingest CPU snapshot | `899%` |
| ingest memory snapshot | `8.725 GiB / 14 GiB` from Docker stats |
| query state | ready at local height `1,817,432` |
| ingest state | `bulk_catchup`, lag `1,535,466` |
| Zebra CPU snapshot | `1.96%` |
| Zebra memory snapshot | `2.748 GiB / 23.43 GiB` |
| Zebra logs | repeated `Elapsed`, stalled chain updates, and exhausted prospective tip sets |

This 2026-05-24 snapshot was not sufficient for a final transport verdict on
its own because Zebra was not healthy near its live tip. The 2026-05-25
finalization checkpoint above supersedes that uncertainty: keep JSON-RPC for
this revision, and keep any replacement behind `NodeSource`.

## Final diagnosis

The original Zinder-side bottleneck was not one knob. It was the combination
of source segment latency, ordered source completion, ordered block-prepare output,
high anonymous-memory pressure, and a derive replay scheduler that could only
pause rather than degrade.

The implementation moved away from per-block tree-state fetching and fixed
32-block source requests first. The final architecture then removed the
remaining coupled-stream behavior. The old shape was:

```text
source segment fetch
  -> ordered block stream
  -> ordered block-prepare stream
  -> mutable tree-size finalization
  -> subtree/tree-state attachment
  -> commit
  -> optional flush
```

The implemented shape is a staged pipeline with explicit budgets:

```text
SourceFetchStage
  -> CanonicalBlockPrepareStage
  -> CanonicalFinalizeStage
  -> SubtreeRootAttachmentStage
  -> CanonicalCommitStage
  -> CanonicalFlushStage
  -> ChainEvent
  -> DeriveTailer
```

Each stage exposes active work, queue depth, queued bytes, duration, or the
nearest stage-specific metric. Ordering is a commit invariant, not a fetch
invariant.

## Design rules

1. Every budget must be expressed in the resource unit it bounds: response
   bytes, queued artifact bytes, CPU workers, write-batch bytes, replay batch
   blocks, and cgroup memory.
2. Source adapters stay behind `zinder-source`. Store, query, derive, and
   wallet APIs must not learn whether the source is JSON-RPC, future Zebra
   range gRPC, or another feed.
3. `canonical-*` names belong to the canonical hot path. `derive_replay_*`
   names belong only to the asynchronous projection replay plane.
4. Do not introduce `manager`, `processor`, `worker`, `helper`, `utils`, or
   `legacy` names. Names must describe domain responsibilities.
5. Do not keep compatibility aliases for old config, metric, or native API
   names. This code is unreleased and should converge on the final vocabulary.
6. Prefer vertical slices that cross real boundaries and are deployable with
   measurements over broad refactors that only rearrange files.

## Module ownership target

Keep `services/zinder-ingest/src/bulk_catchup/mod.rs` as the facade for
`BulkCatchupRunConfig`, entrypoints, readiness, recovery, and orchestration. Move
stage internals into cohesive modules under `services/zinder-ingest/src/bulk_catchup/`.

| Module | Ownership |
| --- | --- |
| `source_fetch.rs` | Adaptive source sizing, unordered segment completion, source byte reservations, continuity validation |
| `block_prepare.rs` | Bounded `SourceBlock` to `DerivedBlockArtifacts` dispatch and block-prepare metrics |
| `commit_reassembly.rs` | Ordered height reassembly, commitment-tree finalization, canonical batch close decisions, checkpoint tree state, subtree roots, commit calls |
| `watermark.rs` | Shared byte watermarks, reservations, queue accounting, release-on-drop behavior |
| `flush.rs` | Bulk-catchup flush cadence and blocking `PrimaryChainStore::flush` wrapper if the current helper grows |

Keep `services/zinder-ingest/src/chain_ingest.rs` focused on shared ingest
primitives: `IngestError`, retry helpers, `CanonicalBatch`,
`CanonicalBatchBudget`, subtree-root population, and `commit_ingest_batch`.
Do not let bulk-catchup-specific stage logic keep accreting there.

## Phase 0: Measurement contract and roadmap document

Goal: make every later performance claim reproducible.

Work:

- Keep this document as the roadmap and live evidence ledger.
- Extend `scripts/observability-smoke.sh` so `calibrate` and `snapshot` report
  source, block-prepare, commit, derive, memory, and Zebra health breakdowns.
- Add a `mainnet measurement` section to the script output with image digest,
  resolved compose config, writer height deltas, and 15-minute, 30-minute, and
  60-minute rates.
- Add the same metrics to `docs/architecture/service-operations.md`.

Tests and validation:

- `bash -n scripts/observability-smoke.sh`
- Run `scripts/observability-smoke.sh snapshot` against the deployed stack.
- Confirm the output includes the metrics listed in this document's evidence
  section.

Acceptance:

- A future agent can run one command and obtain the same categories of evidence
  before and after a deploy.
- The report distinguishes source latency, block-prepare CPU, commit/flush,
  memory pressure, and derive replay pressure.

## Phase 1: Extract bulk-catchup stage modules without behavior change

Goal: create the final file structure before changing concurrency semantics.

Work:

- Move source sizing, source queue state, prefetch reservation, and continuity
  validation into `bulk_catchup/source_fetch.rs`.
- Move block-prepare stream construction into `bulk_catchup/block_prepare.rs`.
- Move batch accumulation, tree-size finalization, subtree-root attachment,
  tree-state checkpoint fetch, commit, and batch close orchestration into
  `bulk_catchup/commit_reassembly.rs`.
- Keep `bulk_catchup/mod.rs` as the public facade.
- Keep behavior ordered in this phase.

Tests and validation:

- Existing `zinder-ingest` unit and integration tests must pass unchanged.
- Add narrow tests around the extracted `SourceSegmentSizer` module if names or
  visibility change.

Acceptance:

- No runtime behavior change.
- Imports make ownership obvious.
- No generic wrapper modules or deprecated aliases are introduced.

## Phase 2: Shared byte-watermark foundation

Goal: make queue memory accounting explicit before increasing stage
independence.

Work:

- Add `bulk_catchup/watermark.rs` with:
  - `ByteWatermark`
  - `ByteReservation`
  - `WatermarkLimit`
  - `WatermarkSnapshot`
- Implement release-on-drop semantics for reservations.
- Route current `source_fetch_max_in_flight_bytes` accounting through the
  shared watermark.
- Add derived-artifact and commit-reassembly watermark configs, using names
  that state the bounded resource:
  - `source_fetch_max_in_flight_bytes`
  - `block_prepare_max_in_flight_artifact_bytes`
  - `commit_reassembly_max_queued_artifact_bytes`
- Emit:
  - `zinder_ingest_bulk_pipeline_queue_depth{stage}`
  - `zinder_ingest_bulk_pipeline_queue_bytes{stage}`
  - `zinder_ingest_bulk_pipeline_active{stage}`
  - `zinder_ingest_bulk_pipeline_watermark_blocked_total{stage}`

Tests and validation:

- Unit tests for reserve, release, over-limit refusal, first-reservation
  behavior, release-on-error, and drop release.
- Integration test with a delayed source proving byte reservations never exceed
  the configured hard watermark.

Acceptance:

- Source bytes, block-prepare backlog bytes, and commit-reassembly bytes are
  visible separately.
- No stage can grow an unbounded in-memory backlog.

## Phase 3: Unordered SourceFetchStage with ordered source reassembly

Goal: remove source-fetch head-of-line blocking without weakening canonical
ordering.

Work:

- Replace `FuturesOrdered` source segment completion with unordered completion.
- Reserve source response bytes when scheduling a segment and release them as
  soon as that segment completes or fails.
- Introduce a bounded source reorder buffer keyed by segment start height.
- Validate continuity when emitting blocks into the next stage, not when a
  lower-height future happens to finish.
- Record head-of-line wait:
  - `zinder_ingest_bulk_pipeline_head_of_line_wait_seconds{stage="source_fetch"}`
  - `zinder_ingest_bulk_pipeline_reorder_buffer_blocks`
  - `zinder_ingest_bulk_pipeline_reorder_buffer_bytes`

Tests and validation:

- Mock source where segment `N+1` completes before segment `N`.
- Assert later segment reservations release immediately.
- Assert emitted blocks remain contiguous and ordered.
- Assert source queue stays active while one lower-height segment is delayed.

Acceptance:

- A slow earlier segment no longer prevents later completed source work from
  freeing source capacity.
- Canonical commits remain strictly ordered.

## Phase 4: Independent CanonicalBlockPrepareStage

Goal: keep CPU-bound fact construction independent from source fetch and commit.

Work:

- Convert block prepare into an explicit bounded stage that accepts source blocks
  and emits `(height, DerivedBlockArtifacts, artifact_bytes)`.
- Keep `block_prepare_concurrency` as the worker count.
- Account queued derived artifact bytes against
  `block_prepare_max_in_flight_artifact_bytes`.
- Emit:
  - `zinder_ingest_bulk_pipeline_stage_duration_seconds{stage="canonical_block_prepare"}`
  - `zinder_ingest_bulk_pipeline_active{stage="canonical_block_prepare"}`
  - `zinder_ingest_bulk_pipeline_queue_bytes{stage="canonical_block_prepare"}`

Tests and validation:

- Slow block prepare at height `N` and fast block prepare at height `N+1`.
- Assert completion can be out of order.
- Assert finalization still waits for height `N`.
- Assert block-prepare workers remain active while source fetch has queued work.

Acceptance:

- Source fetch capacity is not consumed by blocks waiting for block-prepare
  permits.
- Fact-build capacity is not hidden behind commit or tree-state fetch latency.

## Phase 5: CanonicalFinalizeStage and commit reassembly

Goal: make ordering a narrow finalization concern.

Work:

- Add an ordered reassembly buffer keyed by `BlockHeight`.
- Fold commitment-tree sizes only when the next expected height is available.
- Keep `finalize_derived_block` single-threaded unless a future proof shows a
  safe associative fold.
- Accumulate `CanonicalBatch` by finalized height order.
- Preserve all current batch close triggers:
  - block count
  - artifact bytes
  - transaction count
  - transparent output count
  - transparent spend reference count
- Emit:
  - `zinder_ingest_bulk_pipeline_stage_duration_seconds{stage="canonical_finalize"}`
  - `zinder_ingest_bulk_pipeline_reorder_buffer_blocks`
  - `zinder_ingest_bulk_pipeline_reorder_buffer_bytes`

Tests and validation:

- Derived blocks complete out of order and commit in order.
- Batch close triggers fire at the same boundaries as before.
- Running tree sizes match current behavior on fixture blocks.
- A delayed commit cannot let reassembly exceed the configured watermark.

Acceptance:

- Ordered finalization is explicit and auditable.
- Commit order is preserved without forcing source and block-prepare completion
  order.

## Phase 6: SubtreeRootAttachmentStage, checkpoint tree state, commit, and flush

Goal: keep source/fact stages productive while batch-tip attachments, commit,
and flush run under bounded backlog.

Work:

- Keep subtree-root and checkpoint tree-state fetches after batch finalization,
  because they attach to the ordered batch tip.
- Move subtree-root attachment into a named stage:
  `SubtreeRootAttachmentStage`.
- Keep checkpoint tree-state fetch as a named substep of commit reassembly.
- Isolate `CanonicalCommitStage` and `CanonicalFlushStage` metrics.
- Allow upstream source/fact stages to continue while commit and flush are in
  progress, bounded by reassembly and artifact-byte watermarks.
- Emit:
  - `zinder_ingest_bulk_pipeline_stage_duration_seconds{stage="subtree_root_attachment"}`
  - `zinder_ingest_bulk_pipeline_stage_duration_seconds{stage="checkpoint_tree_state"}`
  - `zinder_ingest_bulk_pipeline_stage_duration_seconds{stage="canonical_commit"}`
  - `zinder_ingest_bulk_pipeline_stage_duration_seconds{stage="canonical_flush"}`

Tests and validation:

- Delayed tree-state checkpoint fetch does not stop bounded source/fact work.
- Delayed flush does not let source/fact queues grow past watermarks.
- Stored tree state remains checkpoint-only at committed epoch tips.
- Lightwalletd `GetTreeState` still returns `NOT_FOUND` for non-checkpoints.

Acceptance:

- Commit remains serial and atomic.
- Commit and flush no longer hide source and block-prepare pressure.

## Phase 7: Derive replay degrade-before-pause

Goal: make `canonical-first` a controlled degradation policy instead of a
binary stop.

Work:

- Add:
  - `IngestMemoryBudget`
  - `DeriveReplayMemoryBudget`
  - `MemoryWatermarks`
  - `DeriveReplayBudgetState`
  - `EffectiveDeriveReplayLimits`
- Keep config under `[ingest.derive]`:
  - `memory_budget_bytes`
  - `memory_degrade_ratio`
  - `memory_pause_ratio`
  - `memory_resume_ratio`
  - `min_replay_batch_blocks`
- Check memory before every chain-event page and before each replay chunk.
- Shrink effective replay batch size from configured value, to half, to
  `min_replay_batch_blocks`, before pausing.
- Add hysteresis so replay does not flap near the pause threshold.
- Make `replay_concurrency` real if it is kept as a config knob. Otherwise
  remove it from the pressure model and docs.
- Emit:
  - `zinder_ingest_derive_replay_budget_state{state}`
  - `zinder_ingest_derive_replay_effective_batch_blocks`
  - `zinder_ingest_derive_replay_memory_budget_bytes`
  - derive replay watermark gauges

Tests and validation:

- Unit tests for budget-state transitions, invalid ratios, and resume
  hysteresis.
- Integration test proving high pressure shrinks batch size before pause.
- Regression test proving pressure rising mid-pass yields before the next page
  or chunk.
- `--print-config` tests for new config keys.

Acceptance:

- `derive_replay_paused=1` becomes the last rung, not the first response.
- Canonical catchup can keep memory headroom without making derive replay
  opaque.

## Phase 8: Derive replay data-path cleanup

Goal: reduce replay hydration cost after stage and memory metrics show where
derive still spends time.

Work:

- Audit `read_transparent_spend_facts` call sites and coalesce overlapping
  reads inside a replay chunk.
- Keep transparent spend facts as canonical facts. Do not reintroduce raw block
  parsing in derive replay.
- If derive consumers need different fact shapes, add typed canonical facts
  rather than consumer-local RocksDB lookups that rediscover the same data.
- Do not split bundled consumers by cost unless cursor semantics explicitly
  support partial consumer advancement.

Tests and validation:

- Replay tests prove no raw block parse is needed for transparent-address and
  fee projections.
- Reorg tests prove spend facts repair correctly across reverted epochs.
- Metrics show lower `read_transparent_spend_facts` average or lower replay
  memory without increasing canonical commit cost.

Acceptance:

- Derive replay can catch up without repeated high-cost hydration reads.
- Projection cursor advancement remains atomic.

## Phase 9: JSON-RPC transport decision and replacement path

Goal: decide from evidence whether JSON-RPC remains acceptable for the hot path.

Decision gate:

- Keep JSON-RPC if `batch_getblock` p95 is low on a healthy Zebra, source queue
  is not saturated, block-prepare CPU is the ceiling, and commit/flush/memory
  explain the remaining gap.
- Tune JSON-RPC if segment splits, response targets, or in-flight byte limits
  are the bottleneck while Zebra is healthy and CPU has headroom.
- Replace JSON-RPC if, on a healthy Zebra, `batch_getblock` p95 and max dominate
  wall time, ingest CPU has headroom, block prepare is not saturated, commit and
  flush are small, and raising safe concurrency only increases tail latency or
  Zebra instability.

Replacement constraints:

- The replacement stays behind `NodeSource`.
- The domain boundary remains `SourceChainSegment` and `SourceChainUpdate`.
- Do not add `JsonRpcBatch` or `ZebraBatch` names to ingest, store, query, or
  derive.
- Candidate implementations include a Zebra range streaming RPC, a local
  sidecar feed, or another source adapter that emits the same source segment
  values.

Tests and validation:

- Source adapter parity tests against existing JSON-RPC fixtures.
- Live source comparison on the same height regime.
- 30-minute and 60-minute windows for each transport.

Acceptance:

- The project either keeps JSON-RPC with evidence or replaces it behind the
  existing source boundary.
- No wallet, store, derive, or query contract changes are required for the
  transport swap.

## Phase 10: Allocator and RocksDB memory partitioning

Goal: tune memory only after stage metrics prove what remains.

Work:

- Use the stage watermarks to estimate process-level memory partitions.
- Done: canonical and derive stores now use separate
  `RocksDbResourceBudget` defaults through `[storage.canonical.rocksdb]` and
  `[storage.derive.rocksdb]`, including independent write-buffer budgets.
- Test `WriteBufferManager` only if memtable metrics show it is the remaining
  pressure source.
- Test allocator changes such as jemalloc only as an operations slice with
  before/after evidence.

Tests and validation:

- Compare current allocator and candidate allocator under the same height
  regime for at least 30 minutes.
- Record RSS, anonymous RSS, cgroup pressure, swap, writer rate, source p95,
  block-prepare p95, commit p95, and restart/OOM behavior.

Acceptance:

- Ingest does not run steadily at the cgroup hard limit.
- Memory improvements do not hide leaks or increase source/block-prepare tail
  latency.

## Phase 11: Final documentation and cleanup

Goal: leave the repo in the final vocabulary, with no transitional baggage.

Work:

- Update:
  - `docs/adrs/0022-resource-budgeted-bulk-catchup.md`
  - `docs/architecture/chain-ingestion.md`
  - `docs/architecture/service-operations.md`
  - `docs/architecture/node-source-boundary.md`
  - `docs/architecture/fact-first-indexer.md`
  - `docs/runbooks/initial-sync.md`
  - `docs/runbooks/bulk-catchup-oom-recovery.md`
- Remove obsolete metric names and old config examples.
- Remove any wrapper modules that only forward to the final modules.
- Keep `tree_state_checkpoint_*`, `canonical_*`, `block_prepare_*`, and
  `derive_replay_*` names consistent across code, config, docs, and metrics.

Tests and validation:

- `cargo fmt --all --check`
- `cargo check --workspace --all-targets --all-features`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- Targeted ingest, source, store, query, proto, and compatibility tests.
- `bash -n scripts/observability-smoke.sh`
- Mainnet rebuild and 30-minute validation.

Acceptance:

- A developer can find each pipeline concern by grepping its domain term.
- A user sees faster, more predictable sync and query/explorer availability at
  the local height.
- An agent can infer the architecture from file names, config names, metrics,
  and docs without reconstructing history from old names.

## Global validation loop

Every phase that changes runtime behavior follows the same loop:

1. Capture a 15-minute or 30-minute pre-change baseline in the same height
   regime.
2. Implement one bounded architectural change.
3. Run targeted tests for the changed boundary.
4. Run workspace format, check, and clippy gates before deployment.
5. Build images and redeploy the mainnet stack.
6. Capture 15-minute, 30-minute, and when needed 60-minute windows.
7. Record the result in this document before starting the next phase.

Minimum live checks:

```bash
curl -sG http://127.0.0.1:9095/api/v1/query \
  --data-urlencode 'query=(zinder_ingest_writer_tip_height - zinder_ingest_writer_tip_height offset 1h) / 3600'

curl -sG http://127.0.0.1:9095/api/v1/query \
  --data-urlencode 'query=sum by (operation) (rate(zinder_ingest_source_request_duration_seconds_sum[1h])) / sum by (operation) (rate(zinder_ingest_source_request_duration_seconds_count[1h]))'

curl -sG http://127.0.0.1:9095/api/v1/query \
  --data-urlencode 'query=histogram_quantile(0.95, sum by (le, method) (rate(zinder_node_request_duration_seconds_bucket[1h])))'

curl -sG http://127.0.0.1:9095/api/v1/query \
  --data-urlencode 'query=sum(rate(zinder_ingest_block_prepare_total[1h]))'

curl -sG http://127.0.0.1:9095/api/v1/query \
  --data-urlencode 'query=max_over_time(zinder_ingest_memory_pressure_ratio[1h])'
```

Container and upstream checks:

```bash
docker stats --no-stream \
  --format 'table {{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}'

docker exec zinder-mainnet-zinder-ingest-1 sh -lc '
  printf "memory.current="; cat /sys/fs/cgroup/memory.current
  printf "memory.max="; cat /sys/fs/cgroup/memory.max
  printf "memory.swap.current="; cat /sys/fs/cgroup/memory.swap.current 2>/dev/null || true
  awk "/^(VmPeak|VmSize|VmHWM|VmRSS|RssAnon|RssFile|Threads):/ {print}" /proc/1/status
'

docker logs --since 1h z3-mainnet-zebra-1 2>&1 \
  | grep -E 'stalled|Elapsed|DownloadFailed|exhausted prospective tip set|state_tip|current_height' \
  | tail -80
```

## Completion criteria

The performance architecture is complete for this revision:

- Source, block-prepare, finalize, attachment, commit, flush, and derive replay
  pressures are independently visible.
- Source fetch completion is unordered, while canonical commit remains ordered.
- Each stage has hard byte or work limits expressed in the unit that bounds the
  resource.
- Derive replay degrades before pausing and exposes its effective budget.
- Mainnet catchup exceeded the requested 2x/3x target by reaching about `20x`
  over the full tuned catchup window and about `40x` over the fastest sustained
  catchup window.
- Query and explorer remained available at the local canonical height during
  catchup, and the stack reported ready at tip after deployment.
- JSON-RPC is kept for this revision based on catchup-window evidence; future
  transport work must stay behind `NodeSource`.
- The module structure now matches the stage ownership model without generic
  wrapper modules.

Two follow-up decisions remain deliberately evidence-gated, not part of this
completion bar: derive replay data-path cleanup and allocator/RocksDB memory
partitioning. Reopen them only if projection lag or steady-state memory pressure
becomes user-visible after the current derive backlog drains.

## 2026-05-24 implementation checkpoint

Baseline was captured from the live mainnet deployment before changes:

- Writer rate: 8.93 blocks/s over one hour.
- Source segment latency: 8.18s average, 10s p95.
- `batch_getblock` latency: 7.80s average, 10s p95.
- Source fetch queue: 4.38 active requests average, 5 max.
- Memory pressure: 0.92 average, 1.00 max.
- Derive replay paused: 51% of the one-hour window.

Two architectural slices were deployed and measured:

1. Source fetch uses unordered completion plus ordered source-segment
   reassembly. This made the new reassembly metrics visible but did not improve
   end-to-end writer throughput by itself: the five-minute writer rate was
   7.02 blocks/s.
2. Block prepare uses unordered completion plus ordered block-prepare reassembly.
   The five-minute writer rate reached 24.56 blocks/s, 2.75x the one-hour
   baseline. Source segment latency dropped to 2.26s average with 8.51s p95,
   `batch_getblock` dropped to 2.13s average, memory pressure averaged 0.38,
   and derive replay was not paused.

The deployed source/block-prepare follow-up bounded block-prepare reassembly and
changed cold-start source defaults to avoid 128-block split storms:

- `source_segment_max_blocks = 16`
- `source_segment_target_response_bytes = 33554432`
- `source_fetch_max_in_flight_requests = 12`
- `source_fetch_max_in_flight_bytes = 402653184`

The bounded+tuned deployment reached 28.07 blocks/s over five minutes, 3.14x
the one-hour baseline. The same window showed 31.02 block prepares/s, 2.93s
average source segment latency, 9.28s source p95, 2.77s average
`batch_getblock`, 7.70 active source requests on average, 5.55 completed
derived blocks waiting in block-prepare reassembly on average, 1.66s average
commit latency, 0.38 average memory pressure, and no derive replay pause.

This checkpoint was superseded by the 2026-05-24 follow-up implementation and
the 2026-05-25 finalization checkpoint.

## 2026-05-24 follow-up implementation checkpoint

The next implementation slice moved the remaining memory and coupling work
into the code path:

- `source_fetch` now uses the shared `ByteWatermark` reservation primitive.
  Active source requests reserve `node.max_response_bytes` before fetch, shrink
  to measured response bytes on completion, and keep completed out-of-order
  segment bytes reserved until ordered emission. Completed segments are also
  reported separately as source reorder-buffer bytes.
- `canonical_block_prepare` uses the same reservation primitive for active and
  completed derived artifacts and is bounded by
  `block_prepare_max_in_flight_artifact_bytes`.
- `commit_reassembly` can continue finalizing the next batch while one
  subtree-root/checkpoint/commit/flush future is in flight, bounded by
  `commit_reassembly_max_queued_artifact_bytes`.
- `canonical-first` derive replay now degrades before pausing through
  `memory_degrade_ratio`, `memory_pause_ratio`, `memory_resume_ratio`,
  `min_replay_batch_blocks`, and optional `memory_budget_bytes`.
  `replay_concurrency` was removed instead of kept as a false knob.
- `scripts/observability-smoke.sh snapshot` now prints writer-rate windows,
  source latency, bulk-stage latency, queue bytes, reorder-buffer bytes,
  watermark blocks, memory pressure, and derive replay budget state.

Local validation before deployment:

- `cargo check -p zinder-ingest --all-targets`
- `cargo test -p zinder-ingest bulk_catchup --lib`
- `cargo test -p zinder-ingest replay_budget --lib`
- `cargo test -p zinder-ingest --test acceptance print_config`
- `bash -n scripts/observability-smoke.sh`

Live validation was run on the local mainnet deployment through three rebuild
or restart cycles:

1. The initial deploy accepted the new config and reached 32.34 blocks/s over
   five minutes, 3.62x the one-hour baseline, but derive replay remained paused
   after pressure fell below `memory_pause_ratio`. That showed the pause
   hysteresis was too conservative for a catchup writer.
2. The derive budget state machine was changed so paused replay resumes as
   degraded work below `memory_pause_ratio` and returns to the normal replay
   batch below `memory_resume_ratio`. The focused replay-budget unit tests
   cover this transition.
3. The deploy profile was tuned to keep canonical fetch and derive replay
   balanced:

   - `source_fetch_max_in_flight_requests = 20`
   - `source_fetch_max_in_flight_bytes = 671088640`
   - `replay_batch_blocks = 500`
   - `min_replay_batch_blocks = 50`

The final tuned 2026-05-24 deployment reached 35.09 blocks/s over five minutes, 3.93x
the one-hour baseline. The same snapshot showed 30.51 blocks/s over the mixed
15-minute window, 37.53 blocks/s over the mixed 30-minute window, 35.09
blocks/s over the clean five-minute window, 242.06 derive replay blocks/s,
then-current derive replay lag at zero, derive replay state `normal`, effective replay
batch `500`, source queue bytes at the bounded 640 MiB watermark, no watermark
blocks, 0.63 memory pressure over 15 minutes, source segment latency at 4.48s
average, canonical block-prepare p95 at 0.13s, and canonical commit p95 at 8.70s.
The 2026-05-25 checkpoint supersedes this snapshot for post-catchup projection
backlog behavior.
