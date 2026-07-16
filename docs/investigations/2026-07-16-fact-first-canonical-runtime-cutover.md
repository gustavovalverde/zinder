# Fact-First Canonical Runtime Cutover Evidence

Status: local append-only real-service certification; Railway canary not run
Date: 2026-07-16
Revision: `ced54ba4f6df1954b132b93ac93f2bc177be4976`
Local image: `sha256:e5db272e2c6ae80d21b620d99d6474e9bf6d0a8549275cbd41bbd23b33dfe093`
Network: Zcash testnet

The shipped `zinder-ingest` composition now opens or freshly constructs the
version-1 Wallet canonical store and follows atomic Zebra tip observations
through `RocksDbCanonicalStore::commit_live_append`. The default run path does
not open or write the legacy canonical store, start legacy projection work, or
fall back to a migration reader. Shallow and same-height reorg replacement is
not part of this slice; a parent mismatch fails closed and leaves reorg handling
as the next implementation boundary.

This evidence certifies a checkpointed fresh construction, direct handoff to
continuous append-only following, source-outage recovery, and authenticated
restart in the real service container. It does not certify a complete mainnet
construction, reorg execution, wallet projection following, query serving,
checkpoint restore, client parity, the Railway runtime, or production.

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
675 store and ingest tests successfully; 23 live or explicitly gated tests
remained skipped. Formatting and strict Clippy for both crates also passed.

## Railway canary inventory

The read-only audit found the following current state in the `zexplorer`
project, `production` environment. Despite the environment name, only the
named canary service and its Zinder state are in scope for a future run.

| Field | Current value |
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

The Zebra service and its volume are explicitly outside the canary mutation
scope. The source volume is `zebra-mainnet-volume`
(`c74696c1-b456-4cc7-abfa-506b120188d0`) mounted at
`/home/zebra/.cache/zebra`; no deployment step may detach, replace, delete, or
write that volume. The current canary deployment and its current Zinder volume
are the rollback reference and must also remain intact until acceptance
evidence is captured.

## Prepared canary boundary

[`deploy/railway.canonical-runtime.toml`](../../deploy/railway.canonical-runtime.toml)
is the isolated service config. Before deploying, the canary service must select
that config file, set `RAILWAY_DOCKER_TARGET_STAGE=zinder-canonical-runtime`,
change `PORT` from 9099 to 9105, retain the mainnet Zebra endpoint and existing
authentication variables, and attach a new empty canary-only Zinder volume at
`/var/lib/zinder`. The prepared runtime uses the Wallet workload, has no
checkpoint modifier for a full mainnet canonical construction, uses `/healthz`
for deployment liveness, polls `/readyz` for the exact canonical fence, and
sets deployment overlap to zero so two RocksDB writers cannot share a volume.

The measured canary job remains bounded to 10 CPUs and 10 GiB. The existing
24-CPU, 24-GiB legacy allocation is inventory, not authorization to expand the
new experiment. The deployment must use the reviewed revision and record the
resulting Railway image digest before construction begins.

No Railway mutation was made. The blocking operational question is whether the
platform procedure can detach the current canary Zinder volume, attach a fresh
one, and then reliably reattach the original volume to the rollback deployment
without modifying either volume. Credentials and project linkage are present,
but that reversible volume-swap procedure has not been proven. The canary must
stop at this prepared boundary until it is explicit; guessing would put the
only current rollback state at risk.

## Remaining acceptance boundaries

The next code slice is atomic shallow and same-height reorg replacement from the
authenticated event fence. Projection-build leases and the event-pruning floor
follow it. Wallet catch-up and ordered event following, exact wallet readiness,
query and lightwalletd cutover, checkpoint restore, mainnet dense-range evidence,
and real-client parity remain separate gates. This local result is not a claim
that `rocksdb-single-host`, the Railway canary, or production is certified.
