# Fact-First Canonical Runtime Cutover Evidence

Status: local append-only real-service certification; Railway mainnet construction in progress
Date: 2026-07-16
Local certified revision: `ced54ba4f6df1954b132b93ac93f2bc177be4976`
Local certified image: `sha256:e5db272e2c6ae80d21b620d99d6474e9bf6d0a8549275cbd41bbd23b33dfe093`
Railway canary revision: `45fc285fd13e5764648e323ccf38db5d52a16b1a`
Networks: Zcash testnet locally; Zcash mainnet in the Railway canary

The shipped `zinder-ingest` composition now opens or freshly constructs the
version-1 Wallet canonical store and follows atomic Zebra tip observations
through `RocksDbCanonicalStore::commit_live_append`. The default run path does
not open or write the legacy canonical store, start legacy projection work, or
fall back to a migration reader. Shallow and same-height reorg replacement is
not part of this slice; a parent mismatch fails closed and leaves reorg handling
as the next implementation boundary.

The local evidence certifies a checkpointed fresh construction, direct handoff
to continuous append-only following, source-outage recovery, and authenticated
restart in the real service container. The Railway evidence currently certifies
the real canary composition, version-1 admission, fresh mainnet construction
startup, resource enforcement, and active progress only. It does not yet
certify completed mainnet construction, the canary transition to following,
canary restart, reorg execution, wallet projection following, query serving,
checkpoint restore, client parity, or production.

## Local real-service gate

The dedicated Compose project joined the existing `z3-testnet` network and
mounted only the Zebra authentication cookie read-only. Its only writable
volume was the project-scoped Zinder volume
`zinder-canonical-runtime-exact-cert_canonical_state`; Zebra chain state was
not declared or mounted. Docker enforced 10 CPUs and 10 GiB for
`zinder-ingest`. The image exposed only the operations port and used
`/healthz` for process liveness while the certifier polled `/readyz` for the
authenticated serving fence.

| Observation | Result |
| --- | --- |
| Authenticated predecessor | Testnet height 4,175,759 |
| Fresh fixed build fence | Height 4,175,760, epoch 1, event 1 |
| First live append | Height 4,175,761, epoch 2, event 2, lag 0 |
| Fence after source recovery | Height 4,175,763, epoch 4, event 4, lag 0 |
| Restart admission | Reopened height 4,175,763, epoch 4, event 4 |
| Historical prevout reads | 0 |
| Cross-block wallet reads | 0 |
| Fresh-build memory spot sample | 16.7 MiB of 10 GiB |
| Post-restart memory spot sample | 14.82 MiB of 10 GiB |

The initial store path was absent. Construction used a sibling
`canonical.building` path, published and closed the complete fixed-fence store,
renamed it into place, and cold-opened it before following. Zebra then advanced
one block. The live commit log carried the new tip, epoch, event sequence, and
sequence digest in the same transition record, and readiness reported lag zero.

For the outage gate, only the ingest container was disconnected from the Zebra
network. `/readyz` returned HTTP 503 with `node_unavailable`, and the durable
fence stayed at height 4,175,761, epoch 2, event 2 while the source recovery
counter advanced. Both forbidden-read counters remained zero. After the
container rejoined the network, the same admitted writer committed heights
4,175,762 and 4,175,763. A process restart then cold-reopened epoch 4 and event
4 at height 4,175,763.

The store contract tests separately prove that a stale expected fence is
rejected, an invented next commitment-tree frontier is rejected, and forced
process termination immediately before or after the synced RocksDB write
reopens at exactly the old or new complete fence. The focused CI profile ran
677 store and ingest tests successfully; 23 live or explicitly gated tests
remained skipped. Formatting and strict Clippy for both crates also passed.

## Railway canary execution

The pre-mutation audit found the following state in the `zexplorer` project,
`production` environment. Despite the environment name, only the named canary
service and its Zinder state were authorized for this run.

| Field | Pre-mutation value |
| --- | --- |
| Canary service | `zinder-mainnet-canary` (`e1d46097-8bbc-4dc2-810b-04bc01843777`) |
| Active deployment | `558e8a7a-d580-4525-949b-e69ab5acb3a0`, successful and running |
| Active image | `sha256:6d2171b340f3f5b59cfbc43e4cfcdb3cb5da455c09935aec7320b9894a076622` |
| Deployment message | `fix(ingest): replan source prefetch and bound canonical batches` |
| Current config source | `/railway.toml` using `deploy/Dockerfile.railway-nocache` |
| Current resources | 24 vCPU and 24,576 MiB |
| Current Zinder volume | `zinder-mainnet-canary-volume` (`c3a28d0d-c841-4044-a335-11bfcacc434f`) |
| Zinder mount and usage | `/var/lib/zinder`, 169,645.50 MB used of 500,000 MB |
| Zebra endpoint | `http://zebra-mainnet.railway.internal:8232` |
| Current service port variable | `9099`, for the legacy bundled multiplexer |
| Current healthcheck path | No Railway healthcheck path configured |

The Zebra service and its volume remained outside the canary mutation scope.
The source volume is `zebra-mainnet-volume`
(`c74696c1-b456-4cc7-abfa-506b120188d0`) mounted at
`/home/zebra/.cache/zebra`; the canary cycle did not detach, replace, delete, or
write that volume. The pre-mutation deployment remains the executable rollback
reference, but the authorized wipe removed its legacy Zinder state, so it is
not a stateful rollback point.

### Deployment and storage admission

The canary retained volume `zinder-mainnet-canary-volume`
(`c3a28d0d-c841-4044-a335-11bfcacc434f`) at `/var/lib/zinder`. The volume was
wiped through the authenticated Railway browser after the version-1 runtime
first refused the legacy column-family set. No production service, production
traffic, Zebra state, or non-canary volume changed.

| Gate | Evidence |
| --- | --- |
| Legacy state refusal | Deployment `2c511453-3363-4281-8e6b-4d1611f99ac8` connected to Zebra, advertised the v1 runtime and zero forbidden reads, then failed closed on the legacy column-family set before mutation |
| Canary-only wipe | The Railway UI identified the exact canary volume, mount, capacity, and service; the confirmation phrase was entered only for that volume, and the volume API then reported 0 MB |
| Mount ownership diagnosis | Deployment `a14b2fbb-83aa-4c3e-a85c-3a3ade49c811` exposed `Permission denied (os error 13)` for `/var/lib/zinder/store.building` |
| Ownership repair | The canonical entrypoint recursively assigned `/var/lib/zinder` to UID/GID 1000, set `NoNewPrivs: 1`, and execed `zinder-ingest` as process 1 with no permitted or effective Linux capabilities |
| Active construction | Deployment `5c20998c-cda0-4e01-aea8-cec7babe66a3` started fresh mainnet construction at fixed tip 3,414,286 with direct RocksDB I/O |

The baseline deployment used
[`deploy/railway.canonical-runtime.toml`](../../deploy/railway.canonical-runtime.toml),
`RAILWAY_DOCKER_TARGET_STAGE=zinder-canonical-runtime`, and `PORT=9105`. Its
Railway image digest is
`sha256:8bbac648ee3165de20934c9fab67e5805fcf3cbe97ce5e481c28503d5b296ac7`;
the build exported OCI digest
`sha256:2c636676e2b7e2052b42ff6ce65a1caa92ec039d11e390082233b8fa904bbbee`.
The deployment has 10 CPUs, 10,000,000,000 bytes of memory, one EU West
replica, zero overlap, 30 seconds of draining, and `/healthz` with a 300-second
timeout. `/readyz` remains the exact canonical-fence gate.

### Baseline dense-range construction

The service started construction at `2026-07-16T11:59:11.729682Z`. Prometheus
reported `up=1`, `syncing=1`, and `ready=0`; `/readyz` returned HTTP 503 with
phase `bulk_catchup`, target height 3,414,286, and projection identity
`canonical-v1`. At 338,362 prepared blocks, cumulative source-segment wait was
2,058.40 seconds across 5,524 requests, block replay consumed 644.33 worker
seconds, block parsing consumed 319.91 worker seconds, and hex decoding consumed
144.33 worker seconds. Both historical prevout reads and cross-block wallet
reads remained zero.

Railway's aggregate disk series retained an approximately 169.6 GB baseline
after the UI and volume API reported the wipe. A read-only filesystem probe is
therefore authoritative for live construction bytes: at 263,647 prepared
blocks, the external-SST staging directory used 20,663,516,630 bytes, the
container filesystem reported 26,493,362,176 bytes used, and 464,494,665,728
bytes remained available. External-file ingestion is configured to move SSTs
into RocksDB, so publication does not require a second full copy of the staged
families.

At 1,735,811 prepared blocks, the exact filesystem used 69,383,479,296
bytes and retained 421,604,548,608 bytes of available space. Railway reported a
3.58-vCPU peak within the 10-vCPU limit and a 1.18 GB memory peak within the
10 GB limit. Exact deployment traffic totaled 99,694,847,978 bytes, including
98,186,637,962 ingress bytes. Both forbidden-read counters remained zero, and
the service reported no writer, corruption, panic, or source-unavailability
failure.

Construction became source-admission-bound in the dense range around height
1,733,000. The derived source watermark was exactly 156,249,984 bytes, or the
10 GB cgroup limit divided by 64. Adaptive segments contracted from 33 blocks
to 13 blocks. In one sample, two admitted reservations consumed 122.2 MB, and
the next 44.9 MB reservation would have exceeded the watermark even though the
request-count ceiling was 12 and approximately 9 GB of container memory
remained free.

In the final five-minute sample, the source lane completed 0.453 responses per
second with a 4.53-second average Zebra wait and 10.37 MB/s of response payload.
Active source awaits averaged 2.15 and peaked at three; retained reservations
averaged 3.2 and peaked at four. Queue bytes averaged 124,820,928 and peaked at
154,945,232, immediately below the byte watermark, while admission was blocked
0.898 times per second. Hex decoding took approximately 8.5 milliseconds per
returned batch. Downstream preparation consumed approximately 2.47 CPU seconds
per wall second with a shallow reorder buffer, which shows that workers
processed delivered input without saturating the container.

The adaptive planner recorded 81 density-driven prefetch restarts and five
oversized-response restarts. It discarded 248 in-flight or completed segments
because each shrink cancels and refetches every later speculative range. The
five-minute construction rate fell to 5.85 blocks per second in this range. A
straight-line extrapolation would put the remaining construction near 80 hours
if that density persisted, but that is a diagnostic bound rather than a
completion forecast because block density changes by era. The baseline remains
preserved as deployment, log, and metrics evidence. Its canary-only Zinder
state was later wiped for the retention candidate.

## Authenticated dense mainnet replay

Status: single-run local diagnostic; canary experiment admitted
Date: 2026-07-16
Baseline revision: `d1d65ec`
Candidate revision: `08cd633`
Baseline image: `sha256:de74423adf14a777fe9596e37f31a5cb0fb9192e80f0583a02b2a7501d167133`
Candidate image: `sha256:fbbd4b2c3320d218e41e009a0190b0c6be58736aee3369e84d4c90f03aa6e68c`

The deterministic replay fixture contains mainnet heights 1,730,000 through
1,734,999: 5,000 blocks, 58,285 transactions, 61,870 transparent inputs,
119,167 transparent outputs, and 3,829,454,475 raw block bytes. Its manifest
digest is
`1bdc7b4b774e3a5d1e30ac68e33aed102ed2cb89ad769a3d6d29c32eaba64435`,
and its canonical sequence digest is
`91e3fb1e71ce4893fbdb425f45b64f8a8a1b0551b38fe3fe50634d170ff251b9`.
The fixture carries authenticated Zebra predecessor and source-tip
checkpoints. Every arm wrote the production version-1 canonical RocksDB store,
published `READY`, cold-reopened the event fence, and scanned all 5,000 blocks.

The matrix fixed the response cap at 64 MiB, response target at 32 MiB,
segment ceiling at 64 blocks, request ceiling at 12, preparation concurrency at
10, and preparation watermark at 156,249,984 bytes. Each Docker container had
10 CPUs, a 10 GiB memory limit, and a private cgroup namespace. The injected
4.53-second delay is applied once per outer fixture request, so delayed arms
measure coarse transport sensitivity rather than reproducing Zebra's recursive
split latency or response ordering exactly.

| Planner | Source watermark | Delay | Rate | Returned blocks | Response payload | Density restarts | Peak memory |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Restart baseline | 156,249,984 B | 0 ms | 14.14 blocks/s | 5,440 | 8.33 GB | 15 | 4.37 GB |
| Restart baseline | 402,653,184 B | 0 ms | 10.76 blocks/s | 6,970 | 10.94 GB | 13 | 4.58 GB |
| Restart baseline | 156,249,984 B | 4,530 ms | 6.76 blocks/s | 5,056 | 7.74 GB | 13 | 4.31 GB |
| Restart baseline | 402,653,184 B | 4,530 ms | 11.89 blocks/s | 5,807 | 8.94 GB | 11 | 4.52 GB |
| Retention candidate | 156,249,984 B | 4,530 ms | 7.79 blocks/s | 5,000 | 7.66 GB | 0 | 4.31 GB |
| Retention candidate | 402,653,184 B | 0 ms | 13.45 blocks/s | 5,000 | 7.66 GB | 0 | 4.57 GB |
| Retention candidate | 402,653,184 B | 4,530 ms | 13.82 blocks/s | 5,000 | 7.66 GB | 0 | 4.37 GB |

The baseline demonstrates why the watermark cannot be widened independently.
At 402,653,184 bytes with no delay, discard and refetch amplification returned
39.4% more blocks and 42.8% more payload than the fixture, making the run 24%
lower in throughput and 31.4% longer than the 156,249,984-byte baseline.
Density feedback had changed the preferred size of future segments, but the
implementation cancelled completed and in-flight non-overlapping future ranges
and fetched them again.

The candidate keeps those bounded ranges under their existing reservations and
applies the smaller segment size at the unscheduled frontier. Ordered parent
validation is unchanged. A response that exceeds the hard cap still cancels
future work and replans. The delayed 402,653,184-byte arm retained 3 completed
and 132 in-flight segments across 15 density adjustments, while the only
discard was the six in-flight segments associated with one oversized response.
It returned exactly the fixture's 5,000 blocks and 7,658,908,950 response bytes.

Retention improved the delayed 156,249,984-byte arm by 15.3%, from 6.76 to
7.79 blocks per second. The retained 402,653,184-byte arm reached 13.82 blocks
per second with the same delay, 77.5% faster than retained work under the
smaller budget. The zero-delay retained arm reached 13.45 blocks per second.
The two wide-watermark results are effectively tied for this single-run
diagnostic, which means that preserving source concurrency moves the active
limit into block-local CPU. Peak memory remained between 4.31 and 4.58 GB,
well inside the 10 GiB limit.

The sub-hour target requires more than these changes. A fixed mainnet height of
3,414,286 requires an average 948.4 blocks per second to reconstruct in one
hour. The Sandblasting band from height 1,702,296 through 2,175,692 contains
473,397 blocks and alone requires 131.5 blocks per second for a full hour, or
263.0 blocks per second if it may consume half of the one-hour budget. At the
observed 12.9 blocks per response and 4.53-second delay, the 12-request ceiling
can deliver at most 34.2 blocks per second before parsing or persistence. A
402,653,184-byte watermark that admits eight 44.9 MB reservations has a lower
22.8-block-per-second reservation ceiling.

The most favorable measured parse, transaction-fact, and compact-artifact work
was approximately 1,448 cumulative task-seconds for 5,000 blocks, or 0.290
task-seconds per block. These timers use elapsed time inside concurrent blocking
tasks and can include scheduler delay; they are not cgroup CPU time. Their total
is four times the 361.71-second wall interval, which shows approximately four
of the ten available preparation slots active on average. A dense-band
30-minute budget allows only 0.038 ten-core-seconds per block, so the current
task-duration evidence is far outside the target even though it cannot yet
quantify the exact CPU reduction. The next campaign must capture `cpu.stat`
deltas before making a CPU-seconds-per-block claim.

The next optimization lane is block-local CPU attribution and removal of
duplicate parsing, decoding, allocation, or serialization work. The first
tracer bullet should compute each transaction identity once and reuse it across
transaction facts and compact artifacts; the current post-version-5 path
performs repeated full identity conversions. The next run should also capture
cgroup CPU deltas and finer transaction-identity and serialization-size stage
timings. Once zero-delay replay approaches the dense-band budget and refetch
amplification remains below 5%, the remaining delayed-source ceiling should be
tested against a captured Zebra response stream. If the existing JSON request
contract then saturates below the target, the evidence supports a pipelined or
binary historical-range source rather than another watermark increase.

## Railway retention candidate

Status: fresh construction in progress; sub-hour target not met
Revision: `45fc285fd13e5764648e323ccf38db5d52a16b1a`
Deployment: `cbe95970-fbcd-4a62-bbc9-fe74be3ea291`
Railway image: `sha256:96e60b9486eae2a85eb1181453603942d216efe92b17f071df97b32635c0260d`

The baseline deployment evidence was preserved before the canary cycle. The
first retention-candidate deployment,
`41371ae2-07c0-4893-ba15-e4131cfc6a9c`, found an orphaned
`/var/lib/zinder/store.building.block-load-staging` sibling left by the
interrupted unpublished build and failed closed. The exact canary-only Zinder
volume was then wiped. Railway restored the prior healthy image, which began
another unpublished construction. The runtime cleanup contract now removes
the store-owned sibling only after admission proves that the build is
unpublished. Deployment `cbe95970-fbcd-4a62-bbc9-fe74be3ea291` exercised the
recovery path, logged both the sibling discard and unpublished-build restart,
and began fresh construction at `2026-07-16T17:19:12.504685Z` with a fixed tip
of 3,414,542. Zebra state, production Zinder state, and production traffic
remained outside the mutation scope.

The deployment uses the retention planner and a 402,653,184-byte source
watermark under the same 12-request, 10-CPU, and 10 GB container limits. The
real service advertised `canonical-v1`, reported zero historical-prevout and
cross-block-wallet reads, passed `/healthz`, and remained correctly unavailable
on `/readyz` while its store was unpublished. The fixed readiness lag is not a
construction progress metric. The best available progress proxy is the
`block_replay` preparation-stage count, which advances once per block-local
artifact preparation but is not a durable canonical fence.

Five samples from `17:25:26Z` through `17:29:42Z` produced the following
construction window:

| Measurement | First sample | Final sample | Window result |
| --- | ---: | ---: | ---: |
| Prepared blocks | 255,237 | 401,567 | 571.60 blocks/s |
| Source-connected blocks | 256,680 | 402,936 | 571.31 blocks/s |
| Successful source requests | 4,205 | 6,545 | 9.14 requests/s |
| Response payload | 19.23 GB | 34.85 GB | 61.03 MB/s |
| Cgroup CPU | 905.67 CPU-s | 1,586.23 CPU-s | 2.66 vCPU average |
| Cgroup memory high-water | 659.05 MB | 815.43 MB | 815.43 MB maximum |
| Mounted-filesystem usage | 18.17 GB | 31.90 GB | 13.73 GB growth |

The CPU delta was 680.56 CPU-seconds for 146,330 prepared blocks, or 4.65
milliseconds per prepared block in this early range. The cgroup recorded no CPU
throttling. Queue depth stayed between nine and twelve requests, active fetches
stayed between three and nine, and memory remained below 8% of the container
limit. Density adjustments retained an additional 378.42 MB of completed
prefetch during the window. Oversized-response replanning still discarded
124.52 MB, so response-cap handling remains a smaller refetch source.

Railway's volume series was not usable for this window: one 30-second sample
jumped from 25.72 GB to 170.51 GB while the mounted filesystem reported 18.17
GB through 31.90 GB. The in-container filesystem is the construction high-water
authority until that backend series is reconciled. The observed 53.65 MB/s
staging growth must not be extrapolated linearly because external-SST loading
and RocksDB compaction are not linear.

At the final sample, 3,012,976 blocks remained in the inclusive fixed range.
Holding the 571.60-block-per-second preparation rate would require about 1 hour
28 minutes more before database loading, validation, publication, and cold
reopen. A zero-to-fence one-hour build requires 948.48 blocks per second. This
candidate therefore does not demonstrate sub-hour mainnet construction even
before the Sandblasting range. A defensible complete ETA must stratify stable
prepared-block and cgroup-CPU deltas by chain era, including a new window around
height 1.7 million, and then add the final loading and cold-validation phases.

### Dense candidate window

The canary reached the dense range without a restart. A 558.7-second window
from `17:42:15Z` through `17:51:33Z` covered prepared blocks 1,708,132 through
1,729,470.

| Measurement | Dense-window result |
| --- | ---: |
| Prepared throughput | 38.2 blocks/s |
| Source-connected throughput | 38.9 blocks/s |
| Successful source requests | 1.41 requests/s |
| Response payload | 26.6 MB/s |
| Average source request | 2.07 s |
| Average response width | 27.7 blocks |
| Source-watermark blocks | 0.95/s |
| Cgroup CPU | 3.41 cores average |
| CPU per prepared block | 89.2 ms |
| Memory | 1.30–1.38 GB; 1.46 GB peak |
| Mounted-filesystem growth | 55.98 GB to 63.71 GB |

The segment target contracted from 31 blocks to 10. The final 106-second
interval completed only 0.52 requests per second, delivered 13.1 MB/s, and
prepared 16.9 blocks per second while average request latency reached 5.82
seconds. CPU was 96.4% user time across the full window, but the container used
only 3.41 of 10 cores and recorded no throttling. Memory stayed below 15% of
the limit, and the filesystem retained approximately 427 GB of free space.

Retention behaved as designed: 40 density adjustments caused zero density
restarts and zero density-triggered discards. They retained 336 in-flight and
21 completed segment observations, including 588.84 MB of completed responses.
Oversized responses remained the explicit waste source, causing three
restarts, 25 in-flight discards, five completed-segment discards, and 180.47 MB
of discarded completed payload.

At 38.2 blocks per second, the remaining Sandblasting band would take about
3.25 hours, and the remaining fixed-fence range would take about 12.25 hours if
the same density persisted. The observed dense rate is 24.8 times below the
zero-to-tip one-hour target, 3.44 times below the rate needed to give
Sandblasting the entire hour, and 6.89 times below a 30-minute dense-band
budget. The candidate fixes discard-on-density amplification, but it does not
make this era sub-hour-capable. The active bottleneck is still the
source/admission lane, amplified by response-size variance and the remaining
oversized-response replans; CPU, memory, throttling, and disk capacity are not
the limiting resources in this window.

## Remaining acceptance boundaries

The current canary must finish construction, cold-open the authenticated READY
fence, follow an advancing Zebra tip, restart, reopen the same fence, and follow
again before this Railway slice is certified. The next code slice is atomic
shallow and same-height reorg replacement from the authenticated event fence.
Projection-build leases and the event-pruning floor follow it. Wallet catch-up
and ordered event following, exact wallet readiness, query and lightwalletd
cutover, checkpoint restore, dense-range completion evidence, and real-client
parity remain separate gates. This result is not a claim that
`rocksdb-single-host`, the Railway canary, or production is certified.

The authenticated dense replay admitted the retention candidate and
402,653,184-byte source budget to the isolated Railway canary. The fresh canary
confirmed retention behavior and the remaining source/admission limit at the
live dense boundary. It must still complete construction and cold validation
before the full end-to-end ETA and resource high-water marks are known. The
local single-run comparison and partial live construction are not production
configuration certification, and the canary is not a production traffic
experiment.
