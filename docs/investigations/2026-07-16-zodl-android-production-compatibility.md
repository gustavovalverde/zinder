# ZODL Android Production Compatibility Boundary

Status: protocol inventory complete; client and public-operator compatibility not certified
Date: 2026-07-16
Network: Zcash testnet
Zinder integration base: `85d5c02094d8ca99163162e41a5b3fe35fd4f389`
ZODL revision: `05cb52e89dc20ccc272ca589691067ac6c64e333`
Zcash Android Wallet SDK revision: `f386369ee82b5aa470ff61a55d2bb40e0d75fae7`

## Decision

The pinned ZODL and SDK sources define a compatible lightwalletd protocol
shape, and the current compatibility adapter implements that shape against the
legacy runtime. The version-1 fact-first storage path is not yet connected to
`zinder-ingest`, `zinder-query`, or `zinder-compat-lightwalletd`. It also lacks
the production wallet projection follower and exact-fence readiness
composition. Zinder therefore cannot yet be called ZODL-client-compatible or
public-operator-compatible on the version-1 path.

| Claim | Decision | Evidence boundary |
| --- | --- | --- |
| Protocol-compatible | Scoped yes | Static ZODL and SDK call tracing matches implemented compatibility RPCs and repository contract tests. This does not prove a live version-1 service. |
| Reference-parity-compatible | No | No exact-fence ZODL comparison against a trusted lightwalletd was possible because the production version-1 reader stack is absent. |
| Client-compatible | No | The pinned APK built, installed, and launched, but no create, restore, scan, balance, history, or send flow reached a version-1 Zinder endpoint. |
| Public-operator-compatible | No | The tracked Compose topology does not include the compatibility service or TLS, and the production services still open legacy stores. |

The accepted full-tip version-1 storage run remains storage-construction
evidence only. It built 4,175,463 testnet blocks in 868.942 seconds, matched the
canonical and wallet fences and digests, performed zero historical prevout and
wallet validation random reads, and peaked at 5,960,953,856 bytes of container
memory. It did not exercise continuous following, query serving, the
lightwalletd adapter, or ZODL. See
[Fact-First Live Validation Evidence](2026-07-15-fact-first-live-validation.md#version-1-rocksdb-storage-construction).

## Revisions and isolation

### Zinder

The isolated Zinder worktree started at
`85d5c02094d8ca99163162e41a5b3fe35fd4f389` with uncommitted changes in:

- `crates/zinder-store/src/canonical_store/block_load.rs`
- `crates/zinder-store/src/canonical_store/builder.rs`
- `crates/zinder-store/src/canonical_store/live_commit.rs`
- `crates/zinder-store/src/canonical_store/mod.rs`
- `crates/zinder-store/src/canonical_store/publication.rs`
- `crates/zinder-store/src/lib.rs`

The binary starting diff contained 349 insertions and 79 deletions and had
SHA-256
`cf2836c21645c5f96366251fd9d36dea6c7897be90e4a1605067bf3662c21c18`.
It was reviewed and validated before being committed. The compatibility work is
on `feat/zodl-production-compatibility` with these reviewable commits:

| Commit | Effect |
| --- | --- |
| `793cfd7` | Authenticates a live append's commitment-tree transition from the persisted frontier and rejects an unrelated checkpoint. |
| `d1f648f` | Makes `GetLightdInfo.taddrSupport` fail closed when no wallet projection readiness reader is present. |
| `e47ec81` | Makes the transparent UTXO acceptance fixture establish both required wallet projection fences before expecting `taddrSupport`. |

### ZODL and SDK

The source ZODL checkout remained read-only at
`05cb52e89dc20ccc272ca589691067ac6c64e333`. Its user-owned state was one
modified `app/src/zcashtestnetInternalDebug/AndroidManifest.xml` and one
untracked `docs/Local lightwalletd backend.md`; the binary diff SHA-256 was
`93ef6294ca5e8add1fb5a5b0a11b4ce59d679805c856a20fb52fa176eb3c6f59`.
No file in that checkout was staged, overwritten, cleaned, or committed.

The build used separate detached worktrees. The ZODL worktree was based on the
same commit and contained only the Internal Debug TLS manifest override plus
the ignored network security configuration and public Caddy root. The Caddy
root SHA-256 was
`3a9987d99610fdaabf52cc2a6ad771e2689542b922a0c1da2f0ee013bfd414f9`.

ZODL declares `cash.z.ecc.android` SDK artifacts at `2.6.5-SNAPSHOT`. Resolving
that declaration without a composite build failed for all three SDK artifacts,
so that label is not a reproducible SDK pin. The accepted APK instead used a
clean SDK worktree at
`f386369ee82b5aa470ff61a55d2bb40e0d75fae7`, whose declared library version is
2.6.4. Gradle dependency insight confirmed that the requested
`2.6.5-SNAPSHOT` modules were substituted by that exact composite build.

The SDK build refreshed its Rust lock to the versions actually compiled. The
resolved `backend-lib/Cargo.lock` SHA-256 was
`e852c2522f931a40ef8739a92368df85affe5417962f9c4b7c8b9bc5997ec308`.
The relevant changes were `imt-tree` 0.2.0, `pir-client` 0.3.0, `pir-types`
0.2.0, `vote-commitment-tree` 0.3.2,
`vote-commitment-tree-client` 0.5.2, `voting-circuits` 0.8.0, and
`zcash_voting` 0.11.0. The root Rust package version resolved to 2.6.4.

### APK and device

| Field | Pinned value |
| --- | --- |
| Build variant | `zcashtestnetInternalDebug` |
| Application ID | `co.electriccoin.zcash.testnet.internal.debug` |
| Version | 3.7.2, version code 2076 |
| APK | `app-zcashtestnet-internal-debug.apk`, 235 MiB |
| APK SHA-256 | `bfd2a0d7d4fad73a7d027ce63abf0981706da784d95fc436274567be9a2f476b` |
| Signature | Android Debug certificate, SHA-256 `3209a2a4fae88261a7bfb7d6b60e0c0446eadbab2c9ecf49cbebb890a69ed3e3` |
| Build | Successful in 14m35s with JDK 21 and Android build tools 36.0.0 |
| Device | AVD `zodl-validation`, `sdk_gphone64_arm64`, API 34, `arm64-v8a` |
| Device fingerprint | `google/sdk_gphone64_arm64/emu64a:14/UE1A.230829.050/12077443:userdebug/dev-keys` |
| Fresh install | 5.21s |
| Cold application launch | 5.328s to `MainActivity` |

The application reached the create-or-restore onboarding screen without a
fatal exception. No wallet seed was generated, entered, displayed, or recorded
because the version-1 backend could not pass endpoint validation.

## ZODL and SDK RPC inventory

Receive addresses, balances, and transaction history are not fetched as
precomputed wallet objects. The SDK derives addresses locally and constructs
balances and history in its local database from compact-block scanning,
transparent address RPCs, and full transaction retrieval.

| RPC used by the pinned SDK | ZODL or SDK purpose | Consumed response contract | Version-1 fact-first status |
| --- | --- | --- | --- |
| `GetLightdInfo` | Endpoint validation, network selection, and sync height discovery | `chainName`, `consensusBranchId`, `blockHeight`, `saplingActivationHeight`, and `estimatedHeight` | Compatibility implementation exists. Production composition is legacy-only. The SDK does not enforce `taddrSupport`, so Zinder must advertise it truthfully and fail closed. |
| `GetLatestBlock` | Initial tip discovery and periodic tip polling | Height and hash | Canonical facts and cold reads exist. No production version-1 query or compatibility adapter is composed. |
| `GetTreeState` | New-wallet checkpoint selection near tip and scan rewind checkpoints | Height, hash, time, and final commitment-tree states | Version-1 tree checkpoints are constructed and cold-readable. Live service and current Ironwood client behavior remain unproven. |
| `GetSubtreeRoots` | Sapling and Orchard scan support | Ordered roots and completing block heights | Version-1 subtree roots are constructed and cold-readable. Live adapter serving is unproven. |
| `GetBlockRange` | Compact-block download and shielded scan | Ordered compact blocks, transaction actions, and chain metadata | Version-1 compact blocks are constructed and cold-readable. Continuous serving and tip changes are unproven. |
| `GetAddressUtxosStream` | Transparent UTXO discovery, requested from height zero on refresh | Address, txid, output index, script, value, and mined height | Version-1 wallet storage construction exists. Catch-up, following, reorg, restart, and production read adapters are missing. |
| `GetTaddressTxids` | Transparent transaction history discovery | Raw transaction stream with mined heights | Implemented on legacy projection state. No version-1 production wallet reader is composed. |
| `GetTransaction` | Full transaction retrieval after discovery and during send tracking | Raw transaction plus height where `-1` is orphaned, `0` is mempool, and positive is mined | Raw transaction retention and cold reads exist. Version-1 runtime serving and status transitions are unproven. |
| `SendTransaction` | Broadcast a signed transaction | lightwalletd error code and message semantics | Existing adapter forwards to the node broadcaster. It has not been exercised with the version-1 topology or pinned ZODL. |
| `GetMempoolStream` | Pending transaction discovery | Raw transactions reported at height zero | Existing adapter streams snapshot and live mempool events and closes on tip change. Version-1 topology and pinned-client behavior are unproven. |

The pinned client does not call unary `GetBlock`, either nullifier range RPC,
`GetTaddressBalance`, `GetMempoolTx`, `GetLatestTreeState`, unary
`GetAddressUtxos`, or `Ping` in the required flows. Those methods remain part
of the public compatibility surface but are not evidence for ZODL client
compatibility.

For a new wallet, the SDK asks for the latest block and then requests a tree
state near `tip - 100`; a five-second failure can fall back to a bundled
checkpoint. Restore starts from the supplied birthday checkpoint and then
discovers the latest height. The SDK polls the tip at roughly 20-second
intervals and does not consume a Zinder tip stream. Its mempool stream is
closed and reopened across block changes. Manual custom-server mode sends to
the selected endpoint, while automatic mode may broadcast to multiple
servers.

## Fact-first runtime coverage

| Requirement | Classification | Current evidence or gap |
| --- | --- | --- |
| Fresh canonical construction | Implemented on version 1 | Fixed-fence full-tip construction, validation, publication, and cold reopen are accepted. |
| Fresh wallet projection construction | Implemented on version 1 | Fixed-fence wallet build, validation, publication, and cold reopen are accepted. |
| Live canonical append | Implemented as a store operation, not as a production service | `commit_live_append` now authenticates the transition from the persisted frontier. `zinder-ingest` does not use it in production composition. |
| Live canonical reorg | Missing | No version-1 production reorg operation and service lifecycle are available. |
| Canonical restart and resume | Unproven | Cold reopen is tested; a long-lived source follower does not reopen and resume version-1 state. |
| Wallet catch-up and following | Missing | The fixed builder publishes a wallet store. No independent authenticated event-cursor follower exists. |
| Wallet reorg and restart | Missing | No production wallet follower can apply replacement epochs or resume from its exact fence. |
| Query readers | Implemented as cold store reads, missing production adapters | `zinder-query` still opens `PrimaryChainStore`, `SecondaryChainStore`, and legacy derive state. |
| Compatibility reader | Implemented only on legacy production state | The adapter RPC logic exists, but the binary does not open version-1 canonical and wallet stores. |
| Exact-fence readiness | Missing in the version-1 topology | Legacy readiness can check both wallet projection cursors. The compatibility adapter now refuses `taddrSupport` when the readiness reader is absent. |
| Compose service topology | Missing | `deploy/docker-compose.yml` has ingest, query, explorer, Prometheus, and Grafana. It has no compatibility service or TLS termination and does not select version-1 service composition. |

No request in this investigation was served from the version-1 store. This
avoids turning a legacy fallback demonstration into a fact-first compatibility
claim.

## Safe runtime observations

The existing Zebra state was inspected read-only and was neither restarted nor
recreated. At `2026-07-16T10:52:46Z` it reported:

| Field | Value |
| --- | --- |
| Container | `z3-testnet-zebra-1`, healthy |
| Image digest | `sha256:998178a61a67b4776ea7104d05c481d86f069a688595e99fcff7f090ae4b7e2b` |
| Image revision label | `15d578362448fb8c4a5d29a00dcfe8adb5184082` |
| Docker network | `z3-testnet` |
| Chain volume | `z3-testnet-chain`, mounted at `/home/zebra/.cache/zebra` |
| Authentication volume | `z3-testnet-cookie`, mounted at `/var/run/auth` |
| Canonical height | 4,175,745 |
| Canonical hash | `002997405879debd53bd981aaa4bdef692852884cf9c8c36581c0af6bbafee68` |
| Estimated height | 4,175,746 |
| Active branch | NU6.3, consensus branch `37a5165b` |
| Zebra-reported chain bytes | 10,195,252,981 |

The local TLS boundary was also exercised without changing Zebra. Caddy 2.11.2
accepted the bundled local root at `https://localhost:19443`; certificate
verification returned zero. `adb reverse tcp:19443 tcp:19443` preserved the
`localhost` certificate identity for the emulator. The request returned HTTP
502 because the h2c upstream at `127.0.0.1:19067` was absent. A five-second
`GetLightdInfo` gRPC probe returned `Unavailable` with the same 502 response.
That is an honest fail-closed endpoint-selection result, not a successful
compatibility smoke.

The first runnable vertical slice is therefore:

1. retain the existing Zebra container, chain volume, and cookie volume;
2. start version-1 `zinder-ingest` from an empty Zinder-only volume and follow
   the captured Zebra fence;
3. build and follow the independent version-1 wallet projection to that exact
   authenticated event fence;
4. open production version-1 query and compatibility secondaries, with
   `GetLightdInfo` refusing readiness until every dependency covers that fence;
5. terminate TLS through the verified Caddy path; and
6. validate `localhost:19443` from the already pinned and installable ZODL APK.

Steps 2 through 4 do not exist in production composition, so a real Compose
stack was not started and no Zinder data volume was deleted. Starting the
legacy stack would violate the version-1 acceptance boundary.

## Clock definitions and instrumentation

The APK build, installation, and application launch measurements above are
environment setup evidence. They are not wallet-sync timings.

### A. Indexer cold-start clock

Start at admission of an empty, identity-scoped Zinder version-1 canonical and
wallet store after capturing the Zebra fence. Stop only when canonical is
following, the wallet projection covers the same authenticated event fence,
the compatibility secondary has caught up, and wallet readiness is true.

Record source RPC wait, payload transfer, decode, parse, prepare, canonical
writes, cold validation, publication, projection scan and merge, projection
catch-up and following, secondary catch-up, readiness evaluation, CPU, peak
memory, disk high-water, network bytes, and every lag gauge. Existing ingest
source-request, canonical-construction, store-write, projection-height, and
projection-lag metrics provide the base. The version-1 production follower and
compatibility-secondary metrics remain to be connected.

### B. Wallet clock

Start with fresh ZODL application data immediately before first endpoint
validation. For restore, record the non-secret birthday and use the same
secret source without logging it. Stop only after endpoint validation, first
compact-block receipt, complete SDK scan, exact-fence balance and history
comparison, and ready-to-send state.

Record endpoint-validation latency, first compact-block latency, range serving
latency and bytes, SDK scan progress, local database work, final network height,
balance and history comparison fence, and time to ready-to-send. Separate
fresh-create and known-seed restore observations.

### C. Total zero-to-wallet clock

Start at the earlier of empty Zinder-store admission and fresh ZODL state.
Stop when both clocks A and B have reached their acceptance boundaries. Report
the overlapping stages as a timeline, not as the sum of the two totals. Zebra
was already at tip in this environment and its initial chain synchronization
must not be included in this clock.

### Warm restart and resume clock

Start immediately before stopping the Zinder writer, wallet follower, query,
and compatibility processes over a populated durable store. Stop when the same
ZODL installation resumes at the same or later authenticated fence, with the
correct balance and history, without historical reconstruction. Record each
process-open stage, secondary catch-up, projection lag, SDK reconnect, and
first successful compact-block or tip response.

None of these four acceptance clocks has a result yet. The dominant blocker is
missing production composition, not an observed performance stage.

## Acceptance gate ledger

| Gate | Result |
| --- | --- |
| Exact Zinder, ZODL, SDK source, resolved lock, variant, APK, device, network, and Zebra fence | Partial pass. All available pins are recorded; no seed birthday or service Compose revision exists because no wallet flow or version-1 stack ran. |
| Truthful `GetLightdInfo` network, heights, activations, transparent support, and readiness | Partial. Network and activation mapping have contract tests. Missing projection readiness now forces `taddrSupport=false`. No live version-1 response exists. |
| Compact block, tree state, subtree root, and range behavior for create, restore, and resync | Unproven in the pinned application flow. |
| Transparent history, UTXO, transaction fetch, submission, mempool, and tip changes | Protocol tests exist on legacy fixtures; version-1 application flow is unproven. |
| Exact-fence ZODL balance and history parity with a trusted reference | Not run. |
| Projection lag fails closed and compat never leads the wallet fence | Unit and integration behavior improved; production version-1 composition missing. |
| Live append, shallow reorg, and restart resume without historical replay | Not run; required version-1 runtime slices are missing. |
| No legacy tables, historical prevout work, obsolete derive consumers, or fallback serving | Not exercised. No legacy stack was started as substitute evidence. |
| Reproducible stage timings identify the next bottleneck | Blocked before the first runtime stage. The next bottleneck is implementation availability. |

## Smallest unblocker

The next implementation should remain inside the ADR-0035 RocksDB
single-host boundary:

1. compose version-1 `RocksDbCanonicalStore` into `zinder-ingest` for fixed
   construction, durable reopen, continuous append, and shallow reorg;
2. add the independent wallet projection follower over authenticated chain
   events, including catch-up, follow, reorg, restart, and exact-fence
   readiness;
3. adapt `zinder-query` and `zinder-compat-lightwalletd` to version-1 canonical
   and wallet readers, including secondary catch-up and fail-closed readiness;
4. add the compatibility and Caddy services to the local Compose topology with
   Zinder-only durable volumes and explicit resource limits; and
5. run fresh-create, known-seed restore, exact-fence reference comparison,
   send, mempool, confirmation, append, restart, lag, and reorg gates with the
   pinned APK.

These slices should replace legacy ownership directly. They must not introduce
dual writes, migration readers, fallback serving, backend abstractions, or
legacy wallet-table preservation.

## Repository validation

The store changes passed package checks, targeted ready-store regressions, and
strict Clippy for `zinder-store`. The compatibility changes passed all 69
selected package tests, with 29 tests skipped by their existing gates, plus
strict all-target and all-feature Clippy. An earlier combined run under four-ABI
Android native compilation produced one timeout and one deterministic fixture
failure. The timed-out RocksDB test passed in isolation, and the fixture was
corrected to establish wallet readiness before advertising transparent
support. Its isolated regression and the full compatibility package suite then
passed.
