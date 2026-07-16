# Fact-First Canonical Runtime Cutover Evidence

Status: local append-only real-service certification; Railway mainnet construction in progress
Date: 2026-07-16
Local certified revision: `ced54ba4f6df1954b132b93ac93f2bc177be4976`
Local certified image: `sha256:e5db272e2c6ae80d21b620d99d6474e9bf6d0a8549275cbd41bbd23b33dfe093`
Railway canary revision: `7f3d0eb25ace6e070a0de9958f6f2a1994af8649`
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

The active deployment uses
[`deploy/railway.canonical-runtime.toml`](../../deploy/railway.canonical-runtime.toml),
`RAILWAY_DOCKER_TARGET_STAGE=zinder-canonical-runtime`, and `PORT=9105`. Its
Railway image digest is
`sha256:8bbac648ee3165de20934c9fab67e5805fcf3cbe97ce5e481c28503d5b296ac7`;
the build exported OCI digest
`sha256:2c636676e2b7e2052b42ff6ce65a1caa92ec039d11e390082233b8fa904bbbee`.
The deployment has 10 CPUs, 10,000,000,000 bytes of memory, one EU West
replica, zero overlap, 30 seconds of draining, and `/healthz` with a 300-second
timeout. `/readyz` remains the exact canonical-fence gate.

### Construction in progress

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
completion forecast because block density changes by era. The active build
remains intact so later evidence can distinguish this range from the complete
mainnet lifecycle.

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

The source-attribution follow-up should replay a captured stream around height
1,733,000. It should compare the current adaptive planner and watermark with a
384 MiB source watermark, then separately test delayed segment-size application
that drains a valid contiguous prefix instead of discarding it on each density
shrink. The comparison must report blocks per second, CPU, Zebra latency,
refetched or discarded bytes, and peak memory. A production configuration
change requires that evidence. The harness must expose the source watermark and
inject deterministic per-segment delay; the current fixture source and
hard-coded 384 MiB benchmark watermark cannot represent this source-concurrency
question unchanged. The live certification build is not a tuning experiment.
