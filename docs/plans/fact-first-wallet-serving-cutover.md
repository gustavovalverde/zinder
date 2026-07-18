# Plan: Fact-First Wallet-Serving Cutover

Status: Core runtime cutover complete; coherent recovery implementation and production evidence in progress
Date: 2026-07-17
Architecture authority: [ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md)
Runtime evidence: [Fact-first canonical runtime cutover](../investigations/2026-07-16-fact-first-canonical-runtime-cutover.md)
Performance continuation: [Canonical-v1 mainnet sync performance handoff](../investigations/2026-07-17-canonical-v1-mainnet-sync-performance-handoff.md)
Transport evidence: [Zebra historical block transport](../investigations/2026-07-16-zebra-historical-block-transport.md)
Client evidence: [ZODL Android production compatibility](../investigations/2026-07-16-zodl-android-production-compatibility.md)

This plan completes the wallet-serving slice of the `rocksdb-single-host`
lifecycle already accepted by Zinder's ADRs. It converts the fixed-fence ZODL
proof into a continuously following, reorg-safe, restart-safe, and publicly
operable runtime without adding legacy fallback, dual writes, migration readers,
or an application-specific storage path.

## Outcome and Evidence Boundary

The target is an authenticated path from Zebra through version-1 canonical and
wallet stores to `WalletQuery` and `zinder-compat-lightwalletd`. ZODL must be
able to create or restore a wallet, follow an advancing chain, display correct
balances and history, submit a transaction, observe it pending and confirmed,
and resume after service restarts. Every public readiness claim must name a
wallet projection that covers the exact canonical event fence.

The current implementation proves less than that target. A pinned ZODL build
on a physical Pixel 10 Pro selected a TLS-terminated Zinder endpoint, scanned
165,600 historical testnet blocks, crossed NU6.3 activation, displayed its
existing balance and history, derived receive addresses, and completed a
Sapling transaction lifecycle. The exercised compatibility requests read
version-1 fact-first stores without legacy fallback, but those stores were
published at fixed fences through an exclusive primary-store handoff. Zinder
cannot yet ingest continuously while serving the same advancing wallet view.

## Production Priority

The fastest safe release is a one-way `rocksdb-single-host` wallet-serving
cutover. Functional correctness advances on one critical path, while canonical
construction performance advances in parallel against the same version-1 store
and evidence contracts. The performance lane advances to another canary only
when local evidence justifies the run, but it must not reorder the lifecycle
work or introduce a second runtime.

| Priority | Scope | Production decision |
| --- | --- | --- |
| Critical path | Canonical replacement, projection ownership and following, epoch-bound readers, mempool composition, coherent restore, operator topology, and client certification | Complete in dependency order and release only after every hard gate passes. |
| Immediate deployment admission | Fact-first single-host topology | Build and publish only ingest, projector, and compatibility images. The checked-in Compose/systemd shape may be used for local validation and controlled canaries, but production routing remains blocked on the hard evidence gates below. Railway remains ingest-only because it cannot provide the required shared host filesystem. |
| Parallel release blocker | Mainnet construction, publication, disk capacity, and restore performance | Work concurrently with the critical path; the accepted ADR lifecycle thresholds, not the sub-hour stretch goal, decide release readiness. |
| Post-wallet cutover | Explorer projection migration and deletion of its replaced legacy ownership | Begin after wallet-ready so explorer work cannot delay the first production wallet service. |
| Separate client track | Android SDK Orchard-to-Ironwood migration and the Fauzec Orchard-only send | Keep outside Zinder runtime scope, then rerun the resulting client transaction through Zinder certification. |
| Deferred storage architecture | `postgres-scale-out` and any generic database adapter | Do not start until the complete `rocksdb-single-host` topology is certified. |
| Deferred transport | Unary Zebra production reads, historical range streaming, and any generic source adapter | Do not start unless the captured-input path reaches the measured transport admission rule or live evidence isolates Zebra state reads as the remaining limit. |

The shipped compatibility surface remains deliberate: `WalletQueryApi` and the
claimed `CompactTxStreamer` protocol must retain their certified external
behavior. Internal backward compatibility is not a goal. Storage migration
readers, legacy config aliases, old composition roots, dual writes, general
source fallbacks, and deprecated schemas are removed when their version-1
replacement passes its phase gate. The narrowly specified ADR-0005
`TreeStateUpstream` cache-miss behavior remains a named consumer contract until
version-1 serving can satisfy the same coverage without it; it is not a general
fallback permission.

This plan does not replace the existing architecture or certification
documents:

- [ADR-0003](../adrs/0003-canonical-storage-access-boundary.md) owns
  epoch-bound reads, RocksDB secondaries, and replica-lag readiness.
- [ADR-0005](../adrs/0005-consumer-neutral-wallet-data-plane.md) owns
  wallet-serving coverage for creation, recovery, rescan, and transparent
  discovery.
- [ADR-0007](../adrs/0007-mempool-topology-and-retention.md) owns mempool
  snapshots, events, retention, and lightwalletd tip-change behavior.
- [ADR-0025](../adrs/0025-chain-event-reconnect-reorg-locator.md) owns event
  reconnect behavior across a reorg.
- [ADR-0035](../adrs/0035-fact-first-storage-selection-and-lifecycle.md) owns
  the version-1 canonical and projection lifecycle and the implementation
  order.
- [Lightwalletd compatibility certification](lightwalletd-compatibility-certification.md)
  owns the evidence levels and public replacement claims.

## Pinned Baseline

The baseline is evidence, not a moving compatibility promise. Every later
certification run must record its own revisions, artifacts, device, network,
and chain fence.

| Component | Pinned evidence |
| --- | --- |
| Zinder integration base | `d96f0c617192fc24d38a5cb57f6caa0c2604a049` |
| Zinder compatibility branch | `feat/zodl-production-compatibility` at `91409c459f9e083a7f2bf146595aa3b699c775e3` |
| ZODL | `05cb52e89dc20ccc272ca589691067ac6c64e333` |
| ZODL variant | `zcashtestnetInternalDebug`, application ID `co.electriccoin.zcash.testnet.internal.debug` |
| Android SDK | `ae884174523e3c25bb5fe9443f6807dd01f821dd` |
| librustzcash | `633f04f5b2343b455703ce542d272ff463ba5abe` |
| Device | Pixel 10 Pro, Android 16, API 36 |
| Network | Zcash testnet |
| Final authenticated fence | Height 4,176,052, epoch 590, event sequence 590 |
| Final wallet digest | `2a0eba1a8196a728af21f6a16d58c91ea825dc238a309680eb43f67c945c9800` |

The Android SDK patch supplies the testnet NU6.3 activation height 4,134,000
and branch ID `37a5165b`, pins the intended librustzcash revision, and builds the
native arm64 backend used by the application. That patch enables the tested
post-activation scan. It does not implement the separate Orchard-to-Ironwood
migration workflow needed for an Orchard-only Fauzec donation.

PR [#17](https://github.com/gustavovalverde/zinder/pull/17) merged the following
compatibility implementation commits into `main`. They are evidence-backed
fixed-fence work, not proof that the complete cutover is implemented.

| Commit | Merged behavior |
| --- | --- |
| `8d2a311` | Fail compatibility readiness closed when projection readiness is unavailable. |
| `55db749` | Require wallet readiness in the transparent UTXO compatibility fixture. |
| `65fc02d` | Read version-1 wallet artifacts from the fact-first store. |
| `d645757` | Serve the fixed-fence fact-first wallet state through query and compatibility paths. |
| `620ba7b` | Rebuild and cold-validate a wallet store from a ready canonical store. |

## Current Fact-First Evidence

The evidence separates implemented behavior from production lifecycle claims.
A passing request at one fixed fence does not prove continuous serving, and a
green readiness endpoint does not prove that a wallet projection is current.

| Requirement | Current evidence | Remaining gap |
| --- | --- | --- |
| Testnet canonical construction | A fresh version-1 construction published and cold-opened a ready store at height 4,175,463 in 868.942 seconds, with a 5,960,953,856-byte peak container memory sample and zero historical-prevout reads. | This does not establish mainnet lifecycle time, restore time, or complete topology capacity. |
| Mainnet canonical construction | The Railway canary built canonical-v1 to height 3,414,760, entered following, and returned to zero lag. Preparation took about 6 hours 14 minutes, cold validation took about 1 hour 32 minutes, and the complete lifecycle took about 7 hours 47 minutes. | The result fails ADR-0035's 3-hour hard gate. Canonical data alone occupied roughly 320 to 338 GB of a 500 GB volume, so wallet, checkpoint, compaction, and restore headroom remain unproven. |
| Canonical following | Real `zinder-ingest` append-only following, outage recovery, and authenticated restart passed. Appending 53 blocks for the final phone fence took about 2.9 seconds. The current public follower also passes deterministic same-height replacement, bounded source discovery, archive, cancellation, reopen, subsequent append, and over-window no-mutation fixtures. | Run the replacement path against a live deterministic node, including explicit source movement and explorer artifacts, before a production recovery claim. |
| Wallet construction | The production projector owns a durable fixed-fence build lease, renews a full configured retention window before every transition, uses bounded external sorters, verifies guarded READY publication, and commits schema-v1 settled-tip identity. Projector unit and CLI suites pass, including nested and lexical-alias storage-root refusal, and the complete local CI, parity, performance, lint, and documentation gates are green. | Measure the production mainnet build against the 2-hour hard gate and prove its worst transition remains inside the four-hour lease. |
| Wallet following | Schema-v1 transitions persist the canonical source position and settled tip, prune settled undo rows atomically, and reconcile append, cancellation, collapsed replacement, restart, and bounded suffix reorgs. Wallet library, projection, acceptance, settlement/reorg, restart, and incremental-cancellation regressions match cold reconstruction. | Prove sustained live following and independent restart while the writer advances. |
| Query and compat | Compatibility serving opens only canonical and wallet RocksDB secondaries, catches an inactive pair to one authenticated fence, and publishes it through an immutable request-scoped generation. Superseded primary-reader constructors and the standalone query binary are deleted. Synthetic exact/mutated-fence, mismatch, old-generation drain, readiness, and CLI tests pass. A real RocksDB cold-lifecycle test covers empty-root bootstrap, primary advance plus wallet reconciliation, refresh, old-generation drain, safe path reuse, and fail-closed writer-fence mutation. | Run live reader-lag and old-request drain evidence while the production writer advances. |
| Readiness | Projector readiness is fence-bound. Compatibility readiness is dynamically bound to its current exact pair and distinguishes projection lag from replica lag. | Run the complete topology while the writer advances and preserve readiness samples across replacement and restart. |
| Broadcast | Zinder forwarded a physical Sapling self-transfer to Zebra, which accepted it. | The composed production topology still lacks its complete live control plane. |
| Mempool | The canonical writer owns the durable schema-v4 mempool event log, serves IngestControl from the same authenticated listener as CanonicalControl, and hides every generation until its source emits an explicit complete-snapshot marker. Focused nonempty-to-empty reconnect, abandoned partial generation, readiness withdrawal/restamp, durable restart/cursor expiry, and acceptance restart-resume tests pass, as do the strict workspace lint and complete CI profiles. | Certify the external non-empty stream through an independent client. |
| Deployment topology | Compose/systemd now run only ingest, projector, and compatibility on one shared host filesystem; release and PR image workflows build only those runtimes. The legacy mixed image, configs, supervision tree, and deploy smoke harness are deleted. Both Dockerfiles pass BuildKit checks, all three Compose network files resolve, and deployment admission tests pass. | Supply operator TLS and close performance, capacity, restore, live replacement, and client evidence before admitting production traffic. |
| Restart | Compat returned ready at the unchanged populated fence in 1.108 seconds, and the phone retained its completed activity. | Writer, projector, and readers have not restarted together while preserving continuous following. |
| Client behavior | Existing-wallet scan, balance, history, receive addresses, Sapling send, pending state, confirmation, and compat restart passed on the physical phone. | Fresh create, known-seed restore, non-empty transparent funds, projection lag, reorg, and Orchard-to-Ironwood migration remain unproven. |

Exact-fence parity also passed for the tested reference surfaces. At height
4,175,463, Zinder and the trusted lightwalletd produced the same tree-state
digest, `be9c6152d1b413dcaab10a05e29110b838e4815800110296a3f5793e6649f5f1`.
A normalized compact range spanning heights 4,133,999 through 4,134,001
matched across NU6.3 activation. The physical transaction
`bf9fecb237ed3ba41570ecdc3258e974c422ee5d2dcd6eca9b86dc4891f9d0b9`
was mined at height 4,176,040, and Zinder later returned the same 2,379 raw
bytes as the trusted reference.

## Production Gates

The hard gates in ADR-0035 define the first production release. The sub-hour
fresh-sync goal remains a performance objective, but it does not delay release
after the accepted hard gates and operational recovery contract pass.

| Lifecycle | Target | Hard gate | Current result |
| --- | ---: | ---: | --- |
| Fresh canonical construction | 2 hours | 3 hours | Fails: about 7 hours 47 minutes on the measured mainnet canary. |
| Wallet projection after canonical | 1 hour | 2 hours | Uncertified for the production projector. |
| Fresh wallet-ready lifecycle | 3 hours | 4 hours | Uncertified. |
| Verified snapshot restore and 10,000-block tail | 15 minutes | 15 minutes | Unimplemented for a coherent canonical and wallet bundle. |
| Healthy canonical lag | At most 2 blocks | At most 2 blocks | Append-only following reached zero lag; bounded reorg recovery passes local public-boundary fixtures but has not run in a live canary. |
| Healthy wallet projection lag | At most 2 canonical epochs | At most 2 canonical epochs | Implemented and readiness-gated locally; no sustained production-envelope measurement yet. |

Production capacity must cover the canonical store, wallet store, one coherent
checkpoint bundle, worst-case compaction and restore workspace, and a documented
chain-growth reserve on the selected volume class. The current 500 GB canary
cannot be certified until per-family physical bytes and those amplification
factors are measured together.

## Dependency Order

The implementation order follows state ownership rather than client RPC order:

```text
critical path
  canonical reorg replacement
    -> projection lease and event-retention floor
    -> wallet construction, catch-up, following, reorg, and restart
    -> canonical and wallet secondary serving
      -> mempool and tip-change composition ---------+
      -> coherent checkpoint and restore -----------+--> operator topology
                                                        -> independent-client certification
                                                        -> wallet production cutover
                                                        -> explorer fact-first cutover
                                                        -> complete topology certification

parallel performance lane
  closed phase attribution
    +-> oversized-response retention -> bounded source concurrency -> block-local CPU
    +-> bounded cold scans -> construction manifest -> fail-closed consumer reads
    +-> per-family physical sizing -> complete topology capacity envelope
```

Canonical reorg replacement precedes projection reorg handling because a
projector cannot reverse a transition the canonical writer cannot publish.
The projection lifecycle precedes secondary certification because readers need
a live primary whose authenticated position can advance. Mempool composition
and coherent checkpoint work may proceed in parallel after secondary ownership
is stable, and both converge before operator packaging. Performance work begins
immediately, but it does not change this functional order.

The checked-in deployment now has the Phase 7 process ownership shape, but it
remains non-production until its hard evidence gates pass. Release admission
rejects the deleted mixed target, superseded reader images, an omitted
projector, and ingest-only production claims. This keeps process topology
completion separate from performance, capacity, restore, and client
certification.

## First Implementation Batch

Start with 4 independently reviewable slices that touch separate boundaries:

1. **Canonical correctness**: complete Phase 1 through atomic shallow and
   same-height replacement, maximum-depth reorg, crash recovery, and public
   boundary tests.
2. **Source efficiency**: add closed phase attribution, then retain valid future
   work when one response is too large and run the authenticated local A/B.
3. **Publication efficiency**: attribute every cold scan, add coverage-aware
   failure tests for transaction locations and subtree roots, and define the
   versioned construction-manifest contract before changing READY semantics.
4. **Deployment admission**: keep the release set limited to ingest, projector,
   and compatibility; validate the single-host Compose contract; and reject
   every legacy, mixed, or ingest-only production claim.

Phase 2 starts as soon as canonical replacement passes, regardless of whether
the sub-hour stretch goal has been reached. Another long Railway canary waits
until both construction and publication have local evidence that justifies the
run; a scheduler-only improvement cannot compensate for the measured 92-minute
publication scan.

### Status at 2026-07-17

| Slice | Implemented boundary | Remaining gate |
| --- | --- | --- |
| Local release gates | The final authenticated-capture artifact passes strict workspace Clippy, warning-denied Rustdoc, deployment admission, all three Compose resolutions, formatting, shell lint, and diff integrity. The complete CI profile exercised 2,253 tests: 2,249 passed in the shared run, the generated environment-table drift was corrected and passed exactly, and the three process-abort tests that exceeded the parallel 20-second timeout passed serially in 1.28, 2.65, and 1.61 seconds. Consumer parity passes 11 of 11 tests and the local performance profile passes 3 of 3. The managed-sandbox socket failures were reproduced as OS `PermissionDenied` and disappeared in the unrestricted rerun. | This is sufficient local evidence for packaging an explicitly non-serving controlled canary. Production traffic remains blocked on mainnet lifecycle and capacity, construction-manifest-bound coherent restore, live replacement, 24-hour soak, operator-edge, TLS/proxy, and independent-client evidence. The parity harness also reports one leaky child descriptor, and macOS nextest discovery is materially slower than test execution; fix both as release-engineering debt. |
| Deployment admission | The release and PR image sets contain only ingest, projector, and compatibility. The checked-in Compose/systemd topology shares one data volume but gives checkpoint staging a distinct named volume mounted only into state-init, ingest, and projector. Separate read-only secret mounts keep the checkpoint capability out of compatibility. Control stays on loopback, compatibility is exposed on host loopback for operator TLS, and every runtime has an exact config and readiness probe. Admission rejects data/checkpoint volume aliasing, missing owner mounts or initialization, any compatibility staging mount, public host binds, private-control publication, liveness-only dependencies, split namespaces, or implicit public-bind opt-ins. The mixed image, legacy configs/supervision, and obsolete deploy test tier are deleted. Railway remains an explicitly ingest-only canary. | Certify the complete topology's performance, capacity, coherent restore, live replacement, and independent-client behavior before enabling production routing. |
| Canonical replacement foundation | Version-1 reorg event bytes are pinned. Physical canonical schema 5 persists the typed nonzero reorg policy, authenticated settled-sequence checkpoint, retention and lease controls, exact resulting fence on every retained event, and the immutable construction-manifest identity. Earlier schemas are rebuild-only, and exact-policy mismatch fails admission without mutation. Cold construction retains only the last `reorg_window + 1` prefixes. Bounded fork discovery handles source rewind, same-height mismatch, and next-parent mismatch without reading at or below settlement; it authenticates every connected replacement block and checkpoint before one consuming atomic commit. Maximum-depth, same-height, stale/settled and over-window rejection, shorter-suffix stale-row deletion, cancellation before commit, reopen, subsequent append, and before/after-write process-abort proofs pass. Reopen binds every retained reorg to permanent event context and cumulative archive state, then validates only the latest event's at-most-window rows and newest-hash pointers. | Certify explicit source-movement cases and explorer artifact/subtree replacements through the public runtime path. The canonical writer and follower foundation itself is complete. |
| Source efficiency | Oversized source responses retain disjoint completed and in-flight future work. Metrics separate zero disjoint-prefetch churn from offending-range retry requests and blocks. The release replay of the authenticated direct 128-block fixture reproduced its exact digest and READY/reopen fence with one source request, no watermark block or restart, and report-schema-2 counters proving zero historical-prevout and cross-block-wallet reads. | Run the delayed JSON-RPC 128-block split check and matched 5,000-block A/B before claiming a throughput gain. |
| Publication attribution | Every completed cold family scan reports duration, rows, and logical bytes without changing validation order or trust semantics. The report-schema-2 128-block replay carried 13 family summaries, including 1,090 transaction-blob rows and 78,821,497 logical transaction-blob bytes. | Add per-family physical bytes and read-I/O, then run the bounded-scan and construction-manifest experiments. |
| Benchmark fidelity | The active fixed-range replay resolves pipeline watermarks from its declared CPU, memory, and response envelope before named overrides. The 10-core, 10 GiB release replay resolved source and preparation watermarks to 160 MiB, preserved exact fixture/load/READY sequence-digest equality, completed in 4.422 seconds at 28.95 blocks/s, and wrote `.tmp/canonical-128-v2-TCmYyQ/report.json` with SHA-256 `d55abfcf0402c7d98c0306bdc3d9d362fd92492b4964457e735fb97fac866e32`. | Peak RSS and runner identity remain unavailable in this macOS run. Add those fields plus closed physical-I/O timings, then rebaseline the 5,000-block dense fixture. Do not compare new default geometry with historical runs as if only one variable changed. |
| Wallet projector lifecycle | The production projector now owns fixed-fence construction and continuous following, renews a full configured retention lease before every transition, rejects nested and lexical-alias store roots, and passes 21 unit plus 6 CLI tests. Wallet RocksDB and projection suites cover build, append, settlement, cancellation, reorg, restart, and lease behavior. | Run the production-envelope transition and mainnet projection timing gates. |
| Exact-pair serving | Compatibility uses generation-specific canonical and wallet secondaries, validates one authenticated pair, atomically swaps it through `ArcSwap`, and retains old generations for in-flight requests. New `CompactTxStreamer` and reflection requests pass one shared readiness interceptor; warning-ready states remain available, while blocking lag returns gRPC `Unavailable` and established streams keep their immutable pair. The real RocksDB cold-lifecycle harness proves empty-root bootstrap, primary advance plus wallet reconciliation, exact-pair refresh, old-generation path exclusion until the final request handle drains, path reuse after drain, and fail-closed rejection of a mutated writer fence without replacing the active pair. Focused frozen-pair and CLI tests pass. | Run the live advancing-writer reader-lag gate and the production transition envelope. |
| Mempool ownership | Snapshot generations remain private until the complete marker. Empty replacement snapshots durably invalidate the previous live set, abandoned partial snapshots never enter durable history, and restart reconciles durable history before serving. The complete CI and strict lint gates pass. | Complete independent-client non-empty mempool certification. |
| Runtime deletion | The mixed single-container tree, query and explorer release images/configs, standalone query binary and CLI tests, native gRPC smoke script, legacy backup command and symbols, obsolete Zallet live placeholder, backup observability surface, and deleted query-secondary metrics are removed. | Delete explorer's replaced ownership only after the post-wallet-cutover explorer projection is ready; no compatibility shim is permitted. |
| Coherent checkpoint capture | The canonical and wallet following primaries expose owner-only checkpoint operations that refuse every existing entry, including dangling symlinks. A loopback-only, bearer-authenticated and supervised `ProjectorControl` pre-binds before the projector can become ready, queues capture only through the running wallet owner, marks readiness syncing, renews the retained canonical lease around capture, requires the live authenticated wallet fence, and publishes the format-1 manifest last. `CanonicalControl.CreateOwnerCheckpoint` and `ReadmitOwnerCheckpoint` require both ordinary control authentication and a distinct method-level capability, an opaque candidate ID, a SHA-256 real-root binding, and an exact expected writer fence. The owner resolves only the fixed canonical child below its configured staging root, cold-re-admits it against the returned physical database identity and complete fence immediately before wallet capture, and never accepts caller-supplied filesystem authority. Compose isolates the staging volume and capability to ingest and projector, never compatibility. Both cold-admitted physical database identities and the canonical construction-manifest version and digest are bound into the manifest. The projector can copy a complete bundle into a fixed-layout sealed recovery directory and admit it only after rehashing every flat regular payload file, the inner manifest, and the construction-manifest sidecar. | Publish sealed artifacts to operator-provided immutable storage, implement `RestorePending`, rekey cursor authentication and clear process-specific mempool/lease state, implement safe inactive-lane restore, and certify the exact 10,000-block tail plus 15-minute restore/capacity gates. A locally sealed directory is byte-verifiable evidence, not physical WORM storage or a restore-safe production artifact. |

## Cutover and Deletion Policy

This is a breaking internal cutover, not a compatibility migration. Freeze
legacy runtime paths now, add no features to them, and make deletion part of
each replacement's definition of done. A phase is incomplete while production
can still select both the old and new owner for the same state.

| Replacement gate | Delete in the same phase |
| --- | --- |
| Production wallet construction and following | Legacy wallet ownership, historical-prevout commit work, obsolete derive consumers, and any production dependency on the diagnostic rebuild command. |
| Epoch-bound version-1 serving | Legacy canonical or wallet query openings, primary-store reader ownership, migration readers, storage fallbacks, and obsolete config aliases. |
| Version-1 ingest control and mempool | The unreachable legacy ingest composition is already deleted. Implement the remaining control-plane and mempool ownership directly on the version-1 runtime, then delete any second process that creates independent mempool truth. |
| Coherent checkpoint restore | Ad hoc store-copy and fixed-fence handoff procedures that cannot prove an exact cross-store fence. |
| Explorer fact-first cutover | Replaced explorer tables, writers, readers, and configuration; this follows wallet production and does not block it. |

Reference implementations may remain only as test or certification oracles
outside the production dependency graph. Diagnostic source experiments remain
bench-only. Do not expose unary Zebra `GetBlock` as a production option, add a
general fallback, or begin range-streaming work during this cutover. Preserve
the ADR-0005 `TreeStateUpstream` cache-miss carve-out as a named, measured
consumer behavior until its replacement passes the same coverage tests.
Reconsider a range stream only after the identical captured-input path reaches
at least 131.5 blocks/s or live measurements isolate Zebra state reads as the
remaining limit. Retire plans and other durable documentation only after their
replacement authority is complete and no active runbook, ADR, or certification
surface references them; documentation cleanup is not on the critical path.

## Parallel Performance Plan

Performance is one release-blocking lane with short local loops. It addresses
source admission, construction CPU, publication, and disk capacity as separate
causes so one improvement is never credited for another phase's time.

### P1: Close Measurement and Byte Accounting

- Record duration, rows, logical bytes, physical bytes, and read I/O for source
  load, external-SST ingestion, settlement, flush, BUILDING reopen, every cold
  family scan, READY publication, independent READY reopen, and semantic replay.
- Separately record disjoint-prefetch retention, discard, and refetch counts and
  bytes, plus offending-range split and retry counts and blocks. Disjoint future
  discard and refetch are expected to remain zero; retries of the one offending
  range are expected work. Also record reservations, active requests, response
  latency, worker utilization, and both prohibited-read counters.
- Attribute physical storage by family and record construction, compaction,
  checkpoint, and restore high-water marks.

P1 exits when the 128-block and 5,000-block reports close phase time and byte
accounting, reproduce the authenticated digests, and report exactly zero
historical-prevout and cross-block-wallet reads.

Admission reports must resolve source-fetch and block-prepare limits from the
declared CPU, memory, and response-size envelope through the production
`CanonicalPipelineLimits` resolver before applying named benchmark overrides.
A harness with independent hard-coded watermarks may remain diagnostic, but it
cannot support a production-representative performance claim.

### P2: Retain Oversized-Response Work

- Split and retry only the offending range, retain disjoint completed and
  in-flight future ranges under their reservations, and shrink only the
  unscheduled frontier.
- Fail closed on a gap, overlap, reservation overflow, duplicate emission, or
  parent mismatch.
- Run the handoff's delayed 5,000-block source matrix after the 128-block
  correctness loop passes.

The correctness fix may merge when disjoint future-prefetch discard and refetch
reach zero, offending-range retries are fully attributed, all digest and READY
gates pass, and wall time regresses by no more than 3%.
A wider watermark warrants canary consideration only at a matched throughput
gain of at least 15%, with headroom under 10 GiB and no byte amplification.

### P3: Replace Routine Full-Scan Publication

- Rebaseline the historical dense fixture on the current optimized revision
  before attributing any remaining stage split. Treat earlier fixture rates and
  proposed multiplicative gains as historical evidence, not as a forecast for
  the current canary.
- Instrument every cold family scan, then A/B larger readahead, scan
  deduplication, and bounded independent scans under identical source input,
  storage class, and corruption coverage. Evaluate asynchronous I/O separately
  from readahead and concurrency; no unmeasured speedup is an admission claim.
- First test bounded independent scans, deduplicate semantic and generic family
  scans, and replace empty-family scans with bounded probes without changing the
  trust model.
- Define a versioned `CanonicalConstructionManifest` that binds the build plan,
  source fence, checkpoint, family coverage, row and byte counts, key bounds,
  domain-separated digests, replay and subtree digests, and ingested SST
  evidence to the atomic READY record.
- Make admitted-range reads fail closed on missing compact blocks, subtree roots,
  transaction locations, or cross-family associations before routine full scans
  are removed.
- Retain full semantic scans for untrusted restore, migration, explicit
  certification, periodic scrubbing, and corruption recovery.

Bounded scans advance only if the 5,000-block publication phase becomes at least
2 times faster with the corruption matrix intact. The manifest path must make a
10-minute publication allocation plausible before a sub-hour canary is useful.

### P4: Reduce Block-Local CPU and Prove Capacity

- Profile transaction facts, block parsing, and compact artifacts after source
  admission keeps workers fed; optimize one family at a time under byte-for-byte
  fact and sequence-digest equality.
- Revisit request geometry only when bounded source admission, rather than
  speculative transport work, is the measured limit.
- Reject a deployment size until canonical, wallet, checkpoint, compaction,
  restore, and growth-reserve bytes fit its volume class together.

No sub-hour canary starts until the delayed dense fixture reaches at least 131.5
blocks/s and publication has a credible 10-minute path. A candidate that improves
efficiency without meeting those admission thresholds can merge, but it cannot
support a sub-hour claim. The production release instead uses ADR-0035's 3-hour
canonical and 4-hour wallet-ready hard gates.

## Phase 0: Preserve the Fixed-Fence Tracer Bullet

Status: Implemented on `main` by PR [#17](https://github.com/gustavovalverde/zinder/pull/17)

The existing tracer bullet is the regression floor for later lifecycle work.
It must remain runnable while ownership moves into production services, but its
diagnostic rebuild and fixed-fence process choreography must not become the
production topology.

### Required baseline

- Version-1 canonical readers reproduce compact blocks, tree state, subtree
  roots, raw transactions, transparent history, and transparent UTXOs.
- `WalletQueryApi` and the compatibility adapter serve the pinned request and
  error shapes without legacy fallback.
- Readiness fails closed when canonical and wallet identities or fences differ.
- A wallet store constructed from a ready canonical store cold-opens with the
  expected fence and digest.
- The pinned query, compatibility, and wallet-rebuild tests remain green.

### Review gate

- Keep the merged fixed-fence compatibility coverage passing on current
  `main` while later lifecycle phases land.
- Retain the diagnostic rebuild only as an oracle and recovery tool; production
  code must not shell out to it or coordinate serving through store renames.
- Preserve the rule that no request reads legacy canonical wallet tables,
  historical-prevout commit work, obsolete wallet derive consumers, migration
  readers, or fallback storage.

## Phase 1: Make Canonical Replacements Atomic

The append-only canonical writer already establishes ordered event fences.
This phase completes the writer contract so the projection follower has a
durable replacement event to consume.

### Implementation

- Define and version the `REORG_EVENT` wire contract before emitting it. The
  event record binds kind and version, previous and resulting epochs, and exact
  reverted and committed ranges. The same atomic transition binds the new
  canonical identity, event sequence, sequence digest, cursor behavior, and
  restart/readback semantics; a decoder accepting a shape is not proof that the
  writer can publish it.
- Persist and admit a typed, nonzero `CanonicalReorgPolicy` so replacement
  depth is part of the store identity rather than an unverified process-local
  setting. Both initial and reopen admission fail closed on exact-policy
  mismatch; incomplete pre-production version-1 control bytes are rebuilt
  rather than decoded through a compatibility path.
- Store displaced facts as append-only version-1 order rows keyed by event,
  height, and hash, with a newest-observation hash index. Reuse the authenticated
  canonical replay envelope as the semantic core and add only displacement
  epoch and time, optional raw block bytes, and optional final roots. Derive
  transaction identifiers and coinbase outputs on readback rather
  than copying legacy materializations.
- Keep that contract in the existing `displaced_block_facts` family with four
  versioned key spaces: `[0x00]` for archive activation, latest reorg sequence,
  and cumulative block-count state;
  `[0x01] || event_be_u64 || height_be_u32 || hash` for permanent order rows;
  `[0x02] || hash` for the newest event/height pointer; and
  `[0x03] || event_be_u64` for permanent event context binding its epoch, time,
  exact reverted range, row count, and cumulative archive count. The order value
  canonically embeds the replay plus only the thin fields above. Archive reads
  must remain fail-closed after the corresponding canonical event and epoch
  leave their shorter retention window, so retained event rows may be
  cross-checked but are not a permanent read dependency. Validate key identity
  against the decoded replay, validate optional raw bytes against its
  serialized-block digest, and expose bounded event, newest-first page, and hash
  reads that derive the existing product-neutral `DisplacedBlock` result. Keep
  the existing 4,096-row public page ceiling and use one look-ahead row for
  pagination. READY admission must bind retained reorg events to their exact
  permanent contexts and cumulative state, then validate the latest event's
  at-most-window order rows and newest-hash pointers. It must not rescan the
  unbounded permanent archive on every reopen. Add no root, transaction, or
  copied-value index.
- Delete old hash and transaction-identifier index rows, height and position
  rows in the reverted suffix, tree-state checkpoints, optional block artifacts,
  and subtree indices completed after the fork. Write replacement rows only
  after all displaced deletions so a re-mined transaction identifier resolves
  to the replacement. Keep the displaced archive permanently append-only from
  its explicit activation coverage, independently of later `ChainEvent`
  pruning. The reorg window bounds work for one accepted replacement; it does
  not bound repeated replacement history. Retain epoch and event records
  according to their own lifecycle, and leave mempool and currently empty
  daily-balance families untouched.
- Persist one authenticated `CanonicalSequenceCheckpoint` at the settled tip,
  containing its block identity, block count, sequence digest, and logical
  replay bytes. Require every admitted epoch to keep visible-tip distance from
  the settled tip within the persisted reorg window, and advance finality when
  needed to preserve that invariant.
- Recompute a replacement prefix by resuming from the settled sequence
  checkpoint and replaying no more than the persisted reorg window through
  `from_height - 1`. Do not infer a prefix digest by subtracting from the
  existing aggregate, and do not add a rolling checkpoint column family unless
  a future product contract permits the settled tip to lag beyond the reorg
  window.
- During baseline validation, retain only the last `reorg_window + 1`
  authenticated prefix states and select the state matching the published
  settled tip. Append and replacement update the selected checkpoint in the
  same synced batch as canonical rows, the event, epoch, and READY record.
- Add shallow and same-height replacement to the version-1 canonical store.
- Enforce the configured reorg window and reject a replacement whose fork point
  is unavailable or too deep.
- Commit displaced facts, replacement facts, the new canonical identity, epoch,
  event sequence, and sequence digest atomically.
- Keep `commit_live_replacement` as a parallel consuming public operation with
  an explicit expected fence and replacement range. Do not generalize the
  proven append operation into a compatibility enum or shared mutation adapter.
- Resume from the authenticated event fence after restart without replaying
  already committed events.
- Keep historical-prevout and cross-block wallet reads at zero.

### Acceptance criteria

- [x] A deterministic regtest reorg replaces a shallow suffix and publishes one
      ordered, versioned `ChainReorged` transition with the exact reverted and
      committed ranges.
- [x] A same-height replacement produces the expected new hash, epoch, event
      sequence, and digest.
- [ ] Replaced canonical hash, transaction, optional artifact, checkpoint, and
      subtree rows are absent or overwritten according to their family
      contract, while displaced facts remain available through their retained
      archive boundary.
- [ ] The post-replacement sequence digest equals a serial reconstruction from
      the retained prefix plus committed replacement, including after reopen.
- [x] A replacement beyond the configured window fails without modifying the
      visible fence.
- [x] A replacement exactly at the configured maximum depth commits and
      publishes the expected ordered transition.
- [x] Crash injection immediately before and after the RocksDB write reopens at
      exactly the old or new complete fence.
- [x] Append, replacement, reopen, and corruption tests exercise the public
      canonical boundary rather than private column-family mutations.

## Phase 2: Give Projection Construction Production Ownership

Projection construction and continuous following share one ownership model:
an inactive generation builds at a pinned canonical anchor, catches up through
retained events, validates its digest, and becomes eligible for promotion. This
phase establishes that ownership before exposing a long-running follower.

### Implementation

- Add the durable projection-build lease and anchor-aware event-pruning floor
  required by ADR-0035.
- Give `zinder-projector` ownership of wallet-store primary access, projection
  construction, validation, promotion, and recovery.
- Give the version-1 event service ownership of retained-event read, cursor
  validation, reconnect, and expired-cursor behavior. A construction generation
  must bootstrap from a named anchor, consume retained events in order, and
  rebuild from that anchor when its cursor cannot be resumed.
- Reuse the proven fact-first construction algorithm without preserving
  `zinder-bench` as a production service dependency.
- Record projection identity, schema, generation, canonical construction
  anchor, source position, canonical settled tip, coverage, and digest in the
  wallet store. The settled tip is part of READY admission, not a transient
  projector observation.
- Decide and version the wallet digest contract before incremental following.
  It must support the required insert, delete, and reorg-undo transitions, or
  define a bounded recomputation path that still meets the lifecycle gates; it
  must remain comparable with the construction oracle at an exact fence.
- Decide the supported wallet coverage floor as a product contract before
  production following. The current activation-derived coverage remains the
  default. Any narrower birthday floor requires an explicit architecture and
  client contract, clear below-floor behavior, and must not silently turn a
  wallet-serving store into a recent-checkpoint fixture.
- Reject competing, expired, foreign-network, stale-schema, and stale-anchor
  builders before promotion.

### Acceptance criteria

- [ ] Competing builders cannot both hold a valid lease or promote the same
      projection generation.
- [ ] Event pruning never advances past the earliest live construction anchor.
- [ ] A killed builder resumes or restarts safely without publishing a partial
      wallet store.
- [ ] Promotion requires publication verification appropriate to the trusted
      construction path, the pinned canonical identity, and complete event
      catch-up.
- [ ] The constructed store matches the diagnostic rebuild's rows, fence, and
      digest at the same canonical anchor.
- [ ] A retained cursor reconnects from the exact next event, while an expired
      cursor triggers the documented anchor rebuild without skipped history.
- [ ] The event reader resumes from a persisted cursor across append and reorg,
      rejects an unknown event version or malformed range, and never invents a
      replacement from a committed-only event.
- [ ] The selected digest construction matches the oracle after append,
      deletion, reorg undo, restart, and construction at the same fence.
- [ ] Coverage tests fail closed below the supported floor and return complete
      results in range; an explicit birthday-floor policy, if adopted, passes
      the same tests without weakening the activation-derived default.

## Phase 3: Follow Wallet Events Incrementally

The wallet follower replaces the current full reconstruction after every
canonical append. It owns ordered state transitions, while canonical ingestion
remains independent of wallet progress.

### Implementation

- Catch up from the construction anchor by consuming versioned retained
  canonical events in order.
- Implement the Phase 2 digest decision rather than incrementally mutating the
  current append-only digest shape by assumption.
- Commit wallet rows, reorg undo, source position, projection fence, and digest
  atomically for every applied transition.
- Retain reorg undo for exactly the current canonical range strictly above the
  authenticated settled tip. Cold construction obtains that floor from its
  pinned `ChainEpoch`; incremental application authenticates the resulting
  epoch, removes newly settled undo, and persists the new floor in the same
  batch. Do not retain below-settlement undo merely to fill the configured
  maximum window.
- Reverse applied transitions when the canonical writer publishes a reorg.
- Resume from the persisted projection position after a clean restart or crash.
- Authenticate `wallet-ready` against canonical network, store identity, epoch,
  height, hash, event sequence, and replay digest; height equality alone must
  never admit serving.
- Expose projection lag, last applied event, transition duration, row and byte
  counts, restart recovery, and reorg reversal metrics.

### Acceptance criteria

- [ ] A 1-block and a multi-block append advance the wallet projection without
      scanning historical canonical rows.
- [ ] The 53-block phone-test delta does not trigger a 4.17-million-block wallet
      rebuild, and the measured cost is attributable to the delta.
- [ ] Projection output matches a clean reconstruction oracle at the same
      fence after append, restart, and shallow reorg.
- [ ] Killing the projector before and after a transition commit resumes from
      exactly the old or new projection position.
- [ ] Canonical readiness remains independent, while wallet-serving readiness
      reports `ProjectionBehind` until the exact event fence is covered.
- [ ] A projector that observes an expired cursor uses the documented rebuild
      path instead of skipping missing events.

## Phase 4: Serve Through Epoch-Bound Secondaries

Once writer and projector primaries can advance independently, readers can use
the production topology from ADR-0003. The client-facing contract is an
epoch-bound view, not direct primary ownership.

### Implementation

- Open canonical and wallet stores through process-unique RocksDB secondary
  paths in the compatibility service that owns production wallet reads.
- Maintain two reader generations. The published generation is immutable and
  request-owned through reference-counted handles; no catch-up operation may
  mutate it. Catch up the inactive canonical and wallet secondaries on a
  bounded interval, alternating the lagging side until both authenticate one
  exact network, epoch, event sequence, height, hash, and sequence digest.
- Atomically publish only that exact inactive pair. Requests that began before
  publication retain the previous immutable generation until they finish, and
  its secondary paths become reusable only after every reference drains. A
  failed or mismatched candidate leaves the currently published pair unchanged
  and readiness reports the measured lag. Secondary freshness by itself never
  admits a composed read.
- Bound candidate convergence attempts and secondary-path generations so a
  continuously moving writer cannot create an unbounded catch-up loop or
  metadata-directory leak. When no exact candidate is available within the
  configured lag threshold, fail readiness closed while the last coherent pair
  remains available for explicitly stale-tolerant operational inspection only.
- Return typed `ReplicaBehind`, `ProjectionBehind`, schema, identity, and
  writer-status failures instead of empty data or invented readiness.
- Keep `zinder-compat-lightwalletd` as protocol translation over the
  `WalletQueryApi` and the live control surfaces authorized by ADR-0007; it must
  not become another storage owner.
- Preserve request-level epoch pinning across every composed canonical and
  wallet read.

### Acceptance criteria

- [ ] Ingest, projector, and compat run concurrently against the same
      host volumes without any reader opening a production store as primary.
- [ ] Deliberately paused secondary catch-up makes readiness fail closed and
      never advertises a wallet height ahead of the projection fence.
- [ ] Resuming secondary catch-up restores readiness without a process restart
      or historical rebuild.
- [ ] A request started at one epoch never mixes artifacts from a later epoch.
- [ ] A forced canonical or wallet catch-up race either returns one coherent
      authenticated fence or fails with a typed behind status; it never joins
      individually fresh secondaries at different positions.
- [ ] Compat restart reopens secondaries and resumes serving at the same or a
      later authenticated fence.
- [ ] A second canonical or wallet primary fails ownership admission without
      mutating either store.
- [ ] The post-parity serving artifact contains no legacy canonical or wallet
      reader, primary-reader mode, migration reader, storage fallback, or
      obsolete compatibility configuration.

## Phase 5: Compose Mempool and Tip Changes

Confirmed-chain reads and live transaction state have different ownership.
The writer owns mempool truth, and compatibility readers consume it through
`IngestControl` rather than opening a second Zebra connection.

### Implementation

- Compose `WriterStatus`, `ChainEvents`, `MempoolSnapshot`, and `MempoolEvents`
  on the private authenticated ingest-control endpoint.
- Rebuild the live mempool index from the source snapshot before the writer
  advertises ready after restart.
- Connect the compatibility adapter's `GetMempoolStream` to both mempool events
  and canonical tip changes, closing the lightwalletd stream when the tip
  changes.
- Preserve typed broadcast rejection and the configured Zebra broadcaster.
- Retain mined and invalidated events according to ADR-0007, including cursor
  expiration and resnapshot behavior.

### Acceptance criteria

- [ ] A submitted transaction appears through the actual
      `GetMempoolStream`, not only through SDK-local pending state or a duplicate
      submission error.
- [ ] Mining the transaction closes the stream on the tip change, and the next
      compact scan reports the transaction at its confirmed height.
- [ ] Eviction and invalidation produce the documented event and status shapes.
- [ ] Writer restart reconstructs the current mempool before readiness and
      preserves resumable retained event history.
- [ ] Compat uses one writer-owned source view and never establishes
      independent mempool truth.
- [ ] The version-1 control plane runs after the superseded ingest composition
      and duplicate mempool ownership paths are deleted.

## Phase 6: Publish and Restore a Coherent Checkpoint Bundle

Recovery is a product boundary, not an operator copy command. This phase binds
canonical and wallet stores to one authenticated fence, restores them into
inactive paths, and catches the restored topology up before it can serve.

### Implementation

- Queue canonical checkpoint creation through the canonical owner and derive its
  fence before checkpoint creation. Require the owner queue to match that exact
  fence and the configured staging-root binding before it creates any files.
  Withdraw projector readiness, retain the canonical event lease, and hold the
  wallet at that fence until the complete capture publishes or fails.
- Coordinate the bundle inside the projector boundary, which already owns the
  wallet primary and consumes canonical control. Do not add a generic checkpoint
  abstraction before another real consumer exists.
- Create RocksDB checkpoints only from the canonical and wallet primaries. Do
  not archive process-specific secondary metadata or infer coherence from
  matching directory timestamps.
- Publish a bundle manifest with network, topology and schema revisions, store
  identities, projection identity, epoch, event sequence, height and hash, and
  canonical and wallet digests. Treat this inner manifest as checkpoint
  evidence, not as a published recovery artifact.
- Copy the fixed-layout canonical and wallet checkpoint directories into a
  sealed recovery directory, then publish a separate outer recovery manifest
  last. The release format permits only flat regular files below the two fixed
  checkpoint roots plus the fixed inner and outer manifests. The outer manifest
  binds every payload path, byte length and SHA-256 digest, aggregate payload
  digest, the inner manifest digest, both checkpoint identities, and the
  canonical construction-manifest version and digest. Admission rehashes every
  payload byte and reads the construction sidecar through the narrow canonical
  descriptor before any restore boundary may use the candidate. It rejects
  absolute paths, traversal, links, duplicate entries, unexpected roots, nested
  checkpoint directories, and bounded-size/count violations. Only the outer
  manifest makes a candidate visible; failed candidates are eligible only for
  narrow layout-validated orphan cleanup. The configured local archive root
  supplies byte-verifiable sealing, while physical immutable publication remains
  an operator storage requirement.
- Keep the snapshot manifest distinct from the canonical construction manifest:
  one certifies an immutable recovery artifact and tail, while the other proves
  a fresh build reached atomic READY.
- Add a durable canonical `RestorePending` control state and a distinct
  non-serving restored-primary type. A checkpoint-target-bound capability may
  move only its admitted immutable copy into that state; no path-based API may
  downgrade an arbitrary READY primary. Reject `RestorePending` from normal
  primary, secondary, query, and compatibility admission.
- Restore into empty inactive lane-B paths with distinct volumes, storage
  roots, owner identities, secrets, ports, and secondary roots. Verify the
  archive and both manifests before opening either store. In one synced
  restored-primary batch, generate fresh cursor authentication, purge copied
  durable mempool generations/events/head/floor state, and clear copied
  projection-build leases while preserving canonical event history, retention
  state, the canonical fence, and the wallet fence.
- Tail the non-serving restored canonical and wallet owners through the exact
  pre-staged 10,000-block corpus, catch up fresh secondaries, and promote only
  after the final canonical-wallet-compatibility fence matches. Cut traffic
  atomically at the external proxy and drain lane A without shared fallback
  reads or storage.
- Leave Zebra state and every non-Zinder volume outside checkpoint ownership.
- The legacy `zinder-ingest backup` command is deleted. Implement this coherent
  bundle workflow before recovery can pass production admission; no
  canonical-only or legacy canonical-plus-derive fallback is supported.

### Acceptance criteria

- [ ] A checkpoint taken while writer and projector continue operating either
      publishes one exact bundle fence or leaves no published outer manifest.
- [ ] Crash injection around each checkpoint, manifest, archive, and promotion
      boundary exposes either the previous complete bundle or the new complete
      bundle, never a mixed pair.
- [ ] Wrong network, schema, identity, epoch, event sequence, digest, archive
      length, checksum, or 10,000-block tail fails before serving.
- [ ] A READY live primary cannot enter restore reset. Only a target-bound,
      cold-admitted checkpoint can become `RestorePending`, and no serving
      primary or secondary opens that state before explicit promotion.
- [ ] Reset invalidates copied cursor authentication, mempool history and build
      leases without changing canonical event history, retention state, chain
      data, or the canonical and wallet checkpoint fences.
- [ ] Restore, checksum verification, extraction, store admission, 10,000-block
      canonical and wallet tail, secondary catch-up, and readiness complete in
      at most 15 minutes on the certified storage class.
- [ ] Restored compatibility results match the pre-checkpoint exact
      fence, then advance to the current source tip without a full rebuild.
- [ ] The capacity report includes live stores, the checkpoint bundle, restore
      workspace, compaction amplification, and chain-growth reserve together.

## Phase 7: Publish One Reproducible Operator Topology

The durable topology packages the production boundaries only after those
boundaries work independently. Local proof reuses Zebra state, while every
Zinder store and reader path belongs to the isolated Compose project.

### Implementation

- Compose Zebra connectivity, `zinder-ingest`, `zinder-projector`, and
  `zinder-compat-lightwalletd`, then document the operator-controlled TLS
  boundary separately from the storage-owning services.
- Give canonical, wallet, and process-specific secondary metadata distinct
  durable paths with explicit ownership and permissions.
- Add resource limits, health checks, authenticated readiness, structured logs,
  metrics, restart policies, and redacted configuration inspection.
- Expose only the TLS compatibility endpoint through public DNS with a publicly
  trusted certificate and tested SNI. Keep plaintext gRPC, `IngestControl`,
  operations, and readiness surfaces private through explicit binds and firewall
  policy.
- Apply and test proxy access policy, connection and request limits, abuse
  controls, and restricted filesystem permissions.
- Support an empty-Zinder-state start while reusing an existing synchronized
  Zebra volume without declaring, deleting, or rebuilding that volume.
- Document cold construction, checkpoint restore, warm restart, projection lag,
  blue-green traffic switch, bounded rollback, and failure recovery as separate
  operator procedures.

### Acceptance criteria

- [ ] One checked-in Compose invocation reproduces the complete local topology
      without manual container choreography.
- [ ] The release admission probe proves that canonical-v1 ingest, projector,
      compatibility, and private ingest control belong to the same topology;
      an ingest-only image or a mixed legacy-reader bundle fails the probe.
- [ ] Deleting only isolated Zinder state produces a clean construction while
      leaving Zebra state unchanged.
- [ ] Service readiness identifies source wait, canonical construction,
      projection construction, projection lag, replica lag, mempool rebuild,
      and ready states distinctly.
- [ ] Restarting each service independently preserves state and does not trigger
      a full canonical or wallet reconstruction.
- [ ] A device outside the host network connects through public DNS and trusted
      TLS without ADB reverse, an application-installed local certificate
      authority, or access to a private service port.
- [ ] An external bind and firewall audit cannot reach plaintext gRPC,
      `IngestControl`, operations, readiness, or storage ports.
- [ ] Rate-limit and access-policy tests reject excess or unauthorized traffic
      without affecting authenticated readiness.
- [ ] `--print-config`, structured logs, and failure paths expose no seed,
      spending key, authorization material, full receive address, or unredacted
      secret, and service users cannot write another owner's store paths.
- [ ] A second writer or projector primary cannot acquire ownership while the
      active owner remains healthy.
- [ ] The checked-in topology exposes no legacy runtime mode, fixed-fence store
      handoff, or deprecated configuration path.

## Phase 8: Certify and Cut Over the Wallet Service

Certification measures the public service and client boundary after the
runtime lifecycle works. Fixture parity remains a prerequisite, but it cannot
substitute for a real application, a real Zebra source, or a same-fence trusted
reference.

### Client matrix

- Fresh ZODL install and wallet creation on a dedicated test profile or device.
- Known-seed restore from a recorded birthday without logging seed material.
- Existing-wallet rescan across NU6.3 activation.
- Non-empty transparent UTXO and transaction history discovery.
- Shielded balance, transaction history, receive-address display, and a
  corrected non-self receive that appears at the exact Zinder fence.
- Sapling send, actual mempool-stream observation, confirmation, and final
  balance reconciliation.
- Live append, projection lag, compat restart, projector restart, ingest
  restart, shallow reorg, and maximum-depth reorg.
- Exact-fence tree state, compact ranges, raw transactions, final balance, and
  transaction history against a trusted lightwalletd.

### Clock contract

Record 4 clocks without combining unrelated work:

1. **Indexer cold start**: empty Zinder state to canonical ready, projection
   ready, secondaries current, and compat wallet-ready. Zebra initial sync is
   excluded and labeled separately when it occurs.
2. **Wallet clock**: fresh create or known-seed restore through endpoint
   validation, first compact block, fully scanned, correct balance and history,
   and ready to send.
3. **Total zero to wallet**: empty Zinder state plus fresh ZODL state to a
   genuinely usable wallet.
4. **Warm restart and resume**: service interruption to authenticated readiness
   and client recovery without historical reconstruction.

Every run records source wait, payload transfer, decode, parse, prepare,
canonical writes, projection construction and following, secondary catch-up,
compact-block serving, Android SDK scanning, CPU, memory, disk, network, and
lag. A total without stage durations does not satisfy the gate.

Evaluate every clock and lag measurement against the Production Gates table. A
hard-gate failure stops the release, and reports show the stricter targets beside
the hard limits rather than collapsing them into one pass or fail result.

### Claim gates

| Claim | Required result |
| --- | --- |
| Protocol-compatible | Pinned proto and generated-client coverage pass for the claimed RPC set. |
| Reference-parity-compatible | Observable responses and status behavior match the pinned reference at the same fence. |
| ZODL client-compatible, Sapling flow | Fresh create, restore, transparent discovery, shielded discovery and receive, Sapling send, pending observation, confirmation, append, reorg, and restart pass in the pinned client. Orchard-to-Ironwood sending remains a separately named client scope until its wallet-local migration passes. |
| Public-operator-compatible | The reproducible public TLS deployment passes private-bind, access, rate-limit, readiness, lag, restart, resource, redaction, checkpoint, restore, and recovery gates without manual store handoff. This server-side claim does not depend on Android transaction construction. |

Every result names its network. Testnet client evidence cannot support an
unqualified mainnet claim. A mainnet release additionally reruns the complete
lifecycle at the current tip across representative dense-transparent and
Sandblast anchors, proves the hard clocks and capacity envelope, and performs an
advancing-tip soak against the intended mainnet Zebra source.

### Production cutover

- Build the final artifact only after superseded wallet-serving code and
  configuration have been deleted, then rerun parity and topology gates on that
  artifact.
- Deploy the new stack without traffic, restore or construct it, catch up every
  primary and secondary, and verify the exact authenticated fence through the
  public endpoint.
- Switch traffic only while canonical and wallet lag remain within their hard
  limits. Keep the previous stack isolated for a documented rollback window,
  with no shared writes or fallback reads.
- After the observation window passes, delete obsolete wallet-serving storage,
  deployment configuration, and compatibility baggage. A rollback redeploys an
  intact old stack; it never opens version-1 stores with a legacy binary.

### Acceptance criteria

- [ ] A 24-hour advancing-tip soak sustains active client requests and mempool
      streams with bounded lag, stable memory and disk, intact cursor retention,
      no unexplained readiness transition, and clean independent service
      restarts.
- [ ] The no-traffic deployment, catch-up, public-edge verification, traffic
      switch, and bounded rollback rehearsal complete without dual writes,
      fallback reads, or a mixed store fence.
- [ ] The post-deletion production artifact passes every claimed protocol,
      parity, client, operator, lifecycle, capacity, and recovery gate.

## Phase 9: Migrate Explorer and Complete `rocksdb-single-host`

The wallet service may enter production after Phase 8, but Zinder does not call
the whole `rocksdb-single-host` topology complete while an active explorer still
owns legacy state. Any legacy code retained at that boundary must have a current
explorer consumer; otherwise Phase 8 deletes it as dead code.

### Implementation

- Move explorer consumers, construction, backfills, and serving to
  explorer-owned version-1 modules and epoch-bound readers.
- Prove explorer construction, following, reorg, restart, checkpoint, restore,
  lag, query behavior, and parity without coupling `wallet-ready` to explorer
  progress.
- Delete the remaining legacy explorer tables, `zinder-derive` ownership,
  readers, writers, manifests, and configuration in the same cutover.
- Rerun the complete topology lifecycle across representative mainnet anchors
  on the final post-deletion artifact.
- Run native API, lightwalletd, Zallet, ZODL, Zally, Zpay, Zexplorer, and
  Cipherscan contract matrices at named revisions and network scopes.

### Acceptance criteria

- [ ] Explorer lag or rebuild never blocks wallet-serving readiness.
- [ ] Explorer parity passes at exact fences after append, reorg, restart, and
      restore.
- [ ] No legacy derive or explorer runtime path remains in the production
      artifact or deployment configuration.
- [ ] The complete canonical, wallet, explorer, snapshot, serving, and client
      matrix passes before `rocksdb-single-host` is declared certified.
- [ ] Production `postgres-scale-out` work does not begin before this phase
      completes.

## Validation Ladder

Each phase uses the narrowest reliable gate first, then expands only after the
local invariant passes.

1. Unit tests for fence comparison, lease state, event application, reorg undo,
   byte order, and readiness causes.
2. Store integration tests for crash boundaries, replacement, reopen,
   construction, promotion, following, and secondary catch-up.
3. Service integration tests through `WalletQuery`, `IngestControl`, and
   `CompactTxStreamer` public boundaries.
4. Fixture-backed consumer parity through `cargo nextest run --profile=ci-parity`.
5. Live regtest for deterministic mining, mempool transitions, and reorgs.
6. Live testnet for long-history construction, exact-fence reference parity,
   and physical ZODL behavior.
7. Performance-canary admission through closed phase attribution, the 128-block
   correctness loop, and an authenticated delayed 5,000-block A/B with exact
   bytes, order, digests, READY reopen, zero prohibited reads, bounded memory,
   and no unexplained discard or refetch.
8. A 24-hour advancing-tip local soak with active reads, mempool events,
   component restarts, bounded lag, stable resources, and no unexplained
   readiness transition.
9. Representative current-tip mainnet lifecycle evidence for any mainnet claim,
   followed by a separately authorized canary and the blue-green cutover gate.

The repository-wide completion gate remains the testing runbook's formatting,
check, strict Clippy, test, documentation, dependency, and policy suite. A
phase-specific report must name skipped or environment-blocked gates rather
than collapsing them into a generic pass.

## Stop Conditions

Stop the rollout and preserve evidence when any of these conditions occurs:

- Canonical and wallet stores have equal heights but different hashes, epochs,
  event sequences, identities, or digests.
- A reader advertises readiness while its secondary or wallet projection is
  behind the authenticated writer fence.
- A restart requires an undocumented historical replay or mutates an existing
  store before compatibility validation completes.
- Reorg recovery cannot prove whether the old or new transition committed.
- Compat serves a request through legacy tables, a migration reader, an
  unauthorized source fallback, or direct production primary access. The
  ADR-0005 `TreeStateUpstream` cache-miss carve-out must remain explicit,
  measured, and covered by its consumer contract.
- Mempool state is inferred independently by more than one process.
- A benchmark-only optimization changes the public lifecycle before the
  production boundary passes correctness tests.
- A lifecycle or capacity measurement exceeds a hard gate, or the result omits
  enough attribution to determine whether the gate passed.
- A production artifact still contains a superseded selectable runtime,
  migration reader, fallback, or obsolete configuration after replacement
  parity passes.
- A public deployment exposes a private control, plaintext data, operations,
  readiness, or storage endpoint.
- A test requires deleting or rebuilding an existing Zebra or non-Zinder
  volume.

## Separate Android Migration Track

The pinned ZODL build can scan post-NU6.3 blocks and complete a Sapling send,
but it cannot construct the Orchard-to-Ironwood migration needed before an
Orchard-only Fauzec donation. Two physical donation attempts failed locally
before Zinder's `SendTransaction` method with
`Cross-address transfers are disabled for this builder; use add_change_output for wallet-controlled change`.

That failure belongs to the Android SDK and librustzcash transaction lifecycle,
not the Zinder serving cutover. A separate plan must adopt a reviewed migration
API, persist migration progress, expose it in ZODL, and prevent generic sends
from entering the invalid builder path. Zinder certification should rerun the
resulting transaction through broadcast, mempool, confirmation, and exact-fence
history, but this plan must not absorb or simulate the wallet-local migration.

## Completion Record

The wallet-serving production cutover is complete after Phase 8 only when a
dated investigation records:

- exact Zinder, ZODL, Android SDK, librustzcash, Zebra, lightwalletd, Compose,
  device, build variant, network, birthday, and fence inputs without secrets;
- each phase's public-boundary test results and failure evidence;
- exact-fence parity and final wallet balances and history;
- all 4 clocks with stage durations, explicit hard-gate results, and resource
  observations;
- append, projection lag, mempool, reorg, restart, and restore behavior;
- construction-manifest version, snapshot bundle and manifest identities,
  restore and 10,000-block tail results, and disk high-water evidence;
- the exact production runtime, storage, and configuration surfaces deleted by
  each cutover, plus proof that no request used legacy or unauthorized fallback
  state;
- evidence from the final post-deletion artifact and the production cutover or
  rollback rehearsal; and
- the highest justified compatibility claim, with every higher claim marked
  unproven.

The complete `rocksdb-single-host` topology is certified only after Phase 9 adds
the explorer evidence and deletes the final legacy explorer plane. Until the
Phase 8 record exists, the current conclusion remains unchanged: ZODL is a
proven fixed-fence fact-first client for the tested existing-wallet and Sapling
flow, while continuous wallet serving and public operator compatibility remain
incomplete.
