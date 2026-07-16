# ZODL Android Production Compatibility Evidence

Status: physical-client serving and Sapling transaction lifecycle proven at fixed fact-first fences; production following and post-NU6.3 Orchard sending not certified
Date: 2026-07-16
Network: Zcash testnet
Zinder integration base: `d96f0c617192fc24d38a5cb57f6caa0c2604a049`
ZODL revision: `05cb52e89dc20ccc272ca589691067ac6c64e333`
Zcash Android Wallet SDK revision: `ae884174523e3c25bb5fe9443f6807dd01f821dd`

## Decision

The pinned ZODL Internal Debug application can validate and select a TLS-terminated Zinder endpoint, scan real version-1 fact-first compact blocks, derive receive addresses, display its existing shielded balance and transaction history, and submit a signed Sapling transaction through Zinder to Zebra. The compatibility service served these requests directly from version-1 canonical and wallet stores, with no legacy fallback. Exact-fence probes also matched a trusted lightwalletd for tree state and compact-block ranges. The submitted transaction was observed pending by the SDK, confirmed by the trusted reference at height 4,176,040, then fetched byte-for-byte from Zinder at the same mined height and reflected in the physical wallet.

This is client evidence at fixed authenticated fences, not public-operator certification. The production wallet projection follower, RocksDB secondary-reader lifecycle, ingest-control service, mempool composition, shallow reorg path, and durable Compose topology are still absent. Updating the canonical store is fast, but every wallet-fence advance currently requires a full diagnostic projection rebuild and exclusive primary-store handoff. Zinder therefore cannot yet follow Zebra continuously while serving ZODL.

| Claim | Decision | Evidence boundary |
| --- | --- | --- |
| Protocol-compatible | Scoped yes | Every RPC used by the pinned client has been traced. All required fixed-fence RPCs except mempool streaming are served from version-1 stores or the configured Zebra broadcaster. |
| Reference-parity-compatible | Scoped yes | `GetTreeState` and normalized compact-block ranges matched a trusted lightwalletd at the same fences. The comparison does not cover every public lightwalletd method. |
| Client-compatible | Partial | The physical application selected Zinder, completed a historical scan, displayed balance and history, derived receive addresses, submitted a Sapling transaction, observed it pending, and resumed after restart. Fresh create, known-seed restore, transparent funds, and post-NU6.3 Orchard or Ironwood sending remain unproven. |
| Public-operator-compatible | No | Serving still requires a fixed-fence primary-store handoff. Continuous wallet following, secondary catch-up, mempool serving, reorg handling, and reproducible Compose orchestration are missing. |

## Revisions and isolation

The isolated Zinder worktree was seeded at `85d5c02094d8ca99163162e41a5b3fe35fd4f389` with a six-file RocksDB canonical-store diff containing 349 insertions and 79 deletions. Its binary diff SHA-256 was `cf2836c21645c5f96366251fd9d36dea6c7897be90e4a1605067bf3662c21c18`. The work was reviewed before use and later superseded by the authoritative runtime implementation. The compatibility branch is `feat/zodl-production-compatibility`, rebased onto the authoritative `feat/fact-first-runtime-cutover` commit shown above.

The authoritative checkout was treated as read-only. It advanced during the physical test and was reconciled again at `d96f0c617192fc24d38a5cb57f6caa0c2604a049`. At that final reconciliation point it contained a user-owned, uncommitted change in `services/zinder-bench/src/main.rs`. The binary diff SHA-256 was `a2fbc7e765fce4602e6267ce8c6a6ee69bf09fe9a51bb8152b3c4aba806bae67`. That file was not modified, staged, cleaned, or committed by this investigation.

The original ZODL checkout remained read-only at `05cb52e89dc20ccc272ca589691067ac6c64e333`. It retained the user-owned modified `app/src/zcashtestnetInternalDebug/AndroidManifest.xml` and untracked `docs/Local lightwalletd backend.md`. ZODL was built from a separate detached worktree containing only the Internal Debug manifest override and ignored local TLS files.

The resolved SDK was not inferred from ZODL's `2.6.5-SNAPSHOT` declaration. The build used an included SDK worktree at `ae884174523e3c25bb5fe9443f6807dd01f821dd`, whose base is upstream `f386369ee82b5aa470ff61a55d2bb40e0d75fae7`. The patch pins official librustzcash commit `633f04f5b2343b455703ce542d272ff463ba5abe`, resolves Orchard 0.15.0 for the active backend, and supplies the testnet NU6.3 activation height 4,134,000 with branch ID `37a5165b`. The patched native test and Android arm64 library build passed before the ZODL APK was built.

## APK and physical device

| Field | Pinned value |
| --- | --- |
| Build variant | `zcashtestnetInternalDebug` |
| Application ID | `co.electriccoin.zcash.testnet.internal.debug` |
| Version | 3.7.2, version code 2076 |
| APK | `app-zcashtestnet-internal-debug.apk`, 149,369,414 bytes |
| APK SHA-256 | `283f52641a12ba456437df9b8ac67ab57930d0e6ac97f1c38ed3e13e5c50e8d7` |
| Device | Pixel 10 Pro, Android 16, API 36 |
| Device fingerprint | `google/blazer/blazer:16/CP1A.260505.005/15081906:user/release-keys` |
| Connection | USB ADB reverse `tcp:19443` to host `tcp:19443` |
| TLS | Caddy 2.11.2, HTTP/2, SNI `localhost`, application-trusted local root |

The APK was installed as an in-place upgrade. Application data was not cleared, the existing wallet was not replaced, and no seed or full address was logged. ZODL was configured for Manual connection mode with `localhost:19443`. A live established TCP connection through ADB reverse confirmed that the application was using the local Caddy endpoint rather than an automatic reference server.

## RPC and data inventory

Receive addresses, balances, and transaction history are not fetched as wallet objects. The SDK derives addresses locally and constructs wallet state in its local database from compact-block scanning, transparent-address RPCs, and full transaction retrieval.

| RPC used by the pinned SDK | Client purpose and consumed fields | Fact-first result |
| --- | --- | --- |
| `GetLightdInfo` | Endpoint validation and network, branch, activation, tip, estimated height, and transparent-support discovery | Served truthfully with `chainName=test`, branch `37a5165b`, NU6.3 activation 4,134,000, exact canonical height, and `taddrSupport=true` only at an equal ready wallet fence. |
| `GetLatestBlock` | Initial tip discovery and periodic polling | Served from the version-1 canonical fence. |
| `GetTreeState` | New-wallet checkpoint and rewind state | Served from version-1 tree checkpoints. Exact response bytes matched the trusted reference at height 4,175,463. |
| `GetSubtreeRoots` | Sapling and Orchard scanning | Served from version-1 subtree-root artifacts. The physical SDK fetched and accepted both pools. |
| `GetBlockRange` | Compact-block download and shielded scanning | Served ordered version-1 compact blocks. The physical SDK scanned 165,600 historical blocks and later appended ranges. |
| `GetAddressUtxosStream` | Transparent UTXO discovery | Served from version-1 transparent UTXO artifacts. The test wallet had no transparent funds, so non-empty client behavior is not proven. |
| `GetTaddressTxids` | Transparent transaction discovery | Served from version-1 transparent history artifacts. Non-empty physical-client behavior is not proven. |
| `GetTransaction` | Full transaction retrieval and mined status | Served from version-1 raw transaction bytes. A known confirmed transaction was fetched at height 4,175,966 using lightwalletd's reversed txid-byte convention. |
| `SendTransaction` | Signed transaction broadcast | A physical Sapling self-transfer was signed after biometric approval and accepted through Zinder by Zebra. ZODL's immediate resubmission received the truthful `transaction already exists in mempool` response. |
| `GetMempoolStream` | Pending transaction discovery | Missing from the production fact-first topology. The adapter depends on an ingest-control endpoint that is not composed by the version-1 ingest runtime. It fails unavailable instead of inventing readiness. |

The pinned flow does not call unary `GetBlock`, either nullifier-range method, `GetTaddressBalance`, `GetMempoolTx`, `GetLatestTreeState`, unary `GetAddressUtxos`, or `Ping`. Those methods remain part of the broader public surface but do not support a ZODL compatibility claim by themselves.

## Exact-fence parity

At testnet height 4,175,463, `GetTreeState` produced the same SHA-256 digest from Zinder and the trusted lightwalletd: `be9c6152d1b413dcaab10a05e29110b838e4815800110296a3f5793e6649f5f1`. A normalized one-block compact range at that fence matched with digest `75d8b2189c264dd20bf59d6bc09a3d40f5435de4a6d501263e40b2826c11900e`. A normalized range spanning heights 4,133,999 through 4,134,001, across NU6.3 activation, matched with digest `7bd5a11a6464fd0a2950cfb111832d96cc7743e2d6a13743e7ed6d63cb9bc656`.

Zinder includes an additive compact-block header that the reference omitted at the fixed fence. Normalization removed only that additive field; transaction and shielded-action data matched.

## Physical ZODL results

The existing wallet scanned from height 4,009,864 through 4,175,463 over the Zinder endpoint, crossing NU6.3 activation. The SDK reported scan progress 1.0, and the application displayed its existing balance and history. At the later 4,175,999 fence, the internal wallet balance was 101,000,000 zatoshi in Sapling and 1,199,730,000 zatoshi in Orchard, for 1,300,730,000 zatoshi total. The UI rounded this to 13.007 ZEC. The application also derived and displayed its unified and transparent receive addresses and their QR codes.

The application scanned a 33-block live append from 4,175,967 through 4,175,999 in one batch, reported zero new Sapling notes, and advanced both `chainTipHeight` and `fullyScannedHeight` to 4,175,999 at progress 1.0. Subtree-root retrieval succeeded for both Sapling and Orchard.

The application then sent 100,000 zatoshi to its own Sapling receiver, with a 10,000-zatoshi fee. The SDK created transaction `bf9fecb237ed3ba41570ecdc3258e974c422ee5d2dcd6eca9b86dc4891f9d0b9`; Zinder forwarded it to Zebra, Zebra accepted the exact transaction ID, and the SDK changed the wallet state to 100,000,000 zatoshi available Sapling with 890,000 zatoshi pending change. An immediate SDK resubmission received `transaction already exists in mempool`, proving mempool presence even though the production `GetMempoolStream` path is absent. A trusted lightwalletd returned the transaction as mined at height 4,176,040 with 2,379 raw bytes. After Zinder advanced to 4,176,052 and published an equal wallet fence, its `GetTransaction` response had the same height and raw bytes. The physical SDK reported `fullyScannedHeight=4176052`, zero pending change, 100,990,000 zatoshi available Sapling, and the unchanged 1,199,730,000-zatoshi Orchard balance. ZODL displayed the completed activity as a 0.0001-ZEC net send, the fee for the self-transfer. Signing approval plus construction took 47.355 seconds, including the time waiting for a fingerprint; acceptance followed construction by 0.280 seconds.

A temporary scan through the trusted reference reached height 4,176,006 with the same balance and no incoming note. This was used only to disambiguate a faucet result. The application was then returned to Manual mode on the local Zinder endpoint. The higher reference height can remain cached in the local SDK database until the selected server reaches that fence, so it is not evidence that fixed-fence Zinder advanced beyond its authenticated height.

Fresh wallet creation and known-seed restore were not run because doing so would require clearing or replacing user-owned phone wallet state. No seed birthday was recorded. Those remain separate acceptance gates.

## Fauzec receive evidence

Fauzec request `01KXNGC1HFJRZPAR6JWYSVEG44` sent 100,000,000 zatoshi in transaction `0dc4a88013163490aa9d7e40ae422c989aacc195f63c3dde4b4da7dd43b4683f`, confirmed at height 4,175,966. Zinder appended the block and served the raw transaction successfully. A secure address comparison and a read-only copy of the phone wallet database proved that this payment targeted a different address. It must not be counted as a ZODL receive result.

The current phone receive address was then copied without logging it. The faucet placed that address on a 24-hour cooldown ending at `2026-07-17T13:28:10.713Z`, but no payment appeared through height 4,176,006 on the trusted reference. The follow-up request `01KXNHT85MMJWCXH64DYJBAMDQ` was explicitly refused with `address_on_cooldown`. A corrected Fauzec receive is therefore blocked by the faucet cooldown, not proven.

Fauzec's public curl surface was also probed directly. `GET /api/v1/network` reported testnet, Unified and Sapling claim recipients, a 100,000,000-zatoshi drip, and transparent funding receiver kinds. `GET /api/v1/faucet-status` reported ready with zero view lag at height 4,176,030. `GET /api/v1/donations/summary` returned the official testnet donation address without exposing it in this report.

Two 100,000-zatoshi ZODL donation attempts targeted that official address. It contains only an Orchard receiver. Both failed locally before `SendTransaction` with `Cross-address transfers are disabled for this builder; use add_change_output for wallet-controlled change`. The pinned official librustzcash revision recognizes NU6.3 but the generic wallet constructor sets no Ironwood anchor and states that Ironwood bundles are not yet constructed by the wallet. Upstream Android SDK branches `harry/ironwood-migration-sdk-interface` at `42dbc8a2` and `feature/orchard_migration` at `1b32a016` expose a separate Orchard-to-Ironwood migration workflow; they are unmerged work-in-progress interfaces, not a drop-in generic-send fix. The failure is therefore an Android SDK transaction-construction gap, not a Zinder broadcast rejection. No Fauzec donation was broadcast.

## Fact-first runtime coverage

| Requirement | Classification | Evidence or gap |
| --- | --- | --- |
| Canonical construction and reopen | Implemented | The accepted 4,175,463-block construction published and cold-reopened a ready version-1 store. |
| Canonical append | Implemented | Production `zinder-ingest` appended 503 blocks to height 4,175,966, 33 blocks to height 4,175,999, and 53 blocks to height 4,176,052 with authenticated epoch/event transitions and zero historical-prevout reads. |
| Canonical shallow reorg | Missing | No production replacement operation has been validated. |
| Wallet construction | Implemented as a one-shot diagnostic | The reusable `rocksdb-wallet-rebuild` command builds and validates a ready version-1 wallet store from a ready canonical store. |
| Wallet catch-up, follow, reorg, and restart | Missing | There is no independent authenticated wallet event follower. Every tested fence advance required a full rebuild. |
| Compatibility canonical and wallet readers | Implemented at a fixed fence | The adapter opens version-1 stores and serves compact blocks, trees, subtree roots, raw transactions, transparent history, and transparent UTXOs without legacy fallback. |
| RocksDB secondary serving | Missing | Compat currently opens primary stores. It cannot coexist safely with the writer and required root in this local container after writer-owned log files appeared. |
| Exact-fence readiness | Implemented for fixed stores | Compat reports ready and `taddrSupport=true` only when canonical and wallet identities, epoch, event sequence, height, hash, and digest match. |
| Broadcast | Implemented and physically proven for Sapling | Compat connected to Zebra's authenticated JSON-RPC broadcaster; the phone signed and submitted a transaction that Zebra accepted and the reference later returned at height 4,176,040. |
| Mempool and tip-change stream | Missing | The production fact-first ingest-control and mempool publisher are not composed. |
| Durable Compose topology | Missing | The evidence used real services and durable Docker volumes, but they were launched explicitly. A reviewer-owned Compose file does not yet reproduce the topology. |

No request was served from legacy canonical wallet tables, legacy derive consumers, migration readers, or fallback storage.

## Clocks

Zebra was already synchronized. Zebra initial synchronization is excluded from every Zinder clock.

### A. Indexer cold-start clock

The accepted empty-store construction to height 4,175,463 took 868.942 seconds and peaked at 5,960,953,856 bytes of container memory. It matched canonical and wallet fences and digests and performed zero historical-prevout reads. This is construction evidence, not the complete production clock, because it did not include a persistent wallet follower or compat secondary.

Appending 503 blocks from 4,175,463 to 4,175,966 took about 25.4 seconds. Rebuilding the wallet projection at height 4,175,966 took 313.359 seconds: 78.943 seconds canonical scan, 4.306 seconds outpoint sort, 94.315 seconds outpoint merge, 22.920 seconds secondary derivation, 0.056 seconds row load, 0.020 seconds flush and cold reopen, and 112.770 seconds cold validation.

Appending 33 blocks from 4,175,966 to 4,175,999 took 2.000 seconds. Rebuilding that wallet fence took 337.470 seconds: 82.376 seconds canonical scan, 4.197 seconds outpoint sort, 94.263 seconds outpoint merge, 28.419 seconds secondary derivation, 0.059 seconds row load, 0.024 seconds flush and cold reopen, and 128.110 seconds cold validation. The container used about 259 MiB when sampled, with approximately 1.88 GB block reads and 7.34 GB block writes. Network traffic was negligible because the rebuild read local canonical facts.

Appending 53 blocks from 4,175,999 to 4,176,052, including the submitted transaction at 4,176,040, took about 2.9 seconds and again reported zero historical-prevout or cross-block-wallet reads. Rebuilding the equal wallet fence took 319.913 seconds: 85.440 seconds canonical scan, 4.236 seconds outpoint sort, 94.333 seconds outpoint merge, 23.261 seconds secondary derivation, 0.078 seconds row load, 0.021 seconds flush and cold reopen, and 112.526 seconds cold validation. The resulting wallet fence matched canonical height, epoch, and event sequence `4,176,052/590/590`, with projection digest `2a0eba1a8196a728af21f6a16d58c91ea825dc238a309680eb43f67c945c9800`.

The dominant remaining indexer cost is full wallet reconstruction and cold validation after every append. A production follower must replace this rebuild before the complete clock can be certified.

### B. Wallet clock

The existing phone wallet completed a 165,600-block historical scan, displayed balance and history, and became ready to send. The start timestamp for that scan was not captured precisely enough for a reproducible total, so no wallet-clock total is claimed. The later 33-block SDK scan completed in about 0.73 seconds after compact-block delivery began.

Fresh-install create and known-seed restore clocks were not run because phone application data was preserved. Endpoint selection, first compact-block receipt, and ready-to-send behavior are proven for the existing wallet only.

### C. Total zero-to-wallet clock

No total zero-to-wallet time is claimed. The Zinder construction clock and existing-wallet scan overlap was not started from empty Zinder and fresh ZODL state in one controlled run.

### Warm restart and resume

A populated compat restart took 144 ms of actual service startup and 4.524 seconds including an intentional three-second downtime window. The phone displayed the existing balance again in 9.852 seconds including that downtime and downloaded zero compact blocks because its wallet fence was already current. No historical rebuild occurred during this compat-only restart.

After the confirmed self-transfer, a second compat restart returned `/readyz` at the unchanged 4,176,052 fence in 1.108 seconds. Eight seconds later the phone still displayed the balance and completed Sent activity with no pending label. The canonical and wallet volumes were reused and no reconstruction ran.

The writer-to-compat handoff after a canonical append is not a production warm restart. It required a full wallet rebuild and exclusive primary-store access.

## Acceptance ledger

| Gate | Result |
| --- | --- |
| Exact Zinder, ZODL, SDK, native dependency, APK, variant, device, network, and Zebra fences | Pass, except no seed birthday because no seed was accessed. |
| Truthful `GetLightdInfo` and fail-closed projection readiness | Pass at fixed equal fences. |
| Compact block, tree state, subtree root, and range behavior | Pass for existing-wallet scan and exact-fence parity. Fresh create and restore remain unproven. |
| Shielded balance, history, and receive-address display | Pass for the existing wallet. |
| Transparent non-empty balance and history | Unproven on the physical wallet. |
| Transaction submission, pending observation, and confirmation | Partial pass. Sapling signing, Zinder submission, Zebra acceptance, SDK pending state, trusted-reference confirmation, Zinder-local mined retrieval, and final physical-wallet balance/history passed. `GetMempoolStream` remains absent, and Orchard-only Fauzec sending is blocked in the pinned SDK. |
| Exact-fence application balance and history match against a trusted reference | Partial. The same wallet retained the same balance after a reference scan, but a full transaction-by-transaction exact-fence ledger was not captured. |
| Live append and restart | Partial pass. Canonical append, SDK appended-range scan, and compat restart passed. Continuous wallet following did not. |
| Shallow reorg | Not run; production operation missing. |
| No legacy or fallback serving | Pass for the exercised Zinder requests. |
| Reproducible timings identify the bottleneck | Pass. Full wallet reconstruction and cold validation dominate. |

## Smallest production unblocker

The next Zinder vertical slice should add one authenticated wallet projection follower over version-1 chain events, then expose canonical and wallet RocksDB secondaries to query and compatibility readers. The same slice must compose ingest control and mempool publication so `GetMempoolStream` reports pending transactions and closes on tip change. Readiness must continue to fail closed unless the wallet projection covers the exact canonical event fence.

The separate ZODL unblocker is to consume a reviewed Android SDK release that executes and persists the Orchard-to-Ironwood migration before offering post-NU6.3 Orchard sends. ZODL must surface migration state and prevent a generic send from reaching the currently invalid Orchard builder path. Adopting the unmerged migration branch directly would not be production evidence.

After that slice, add a durable Compose topology for Zebra reuse, version-1 ingest, wallet following, compat, Caddy TLS, durable Zinder-only volumes, resource limits, readiness, logs, and metrics. Then rerun fresh create, known-seed restore, non-empty transparent funds, corrected Fauzec receive, authenticated send, mempool, confirmation, append, restart, lag, and shallow reorg gates.

These changes must stay within ADR-0035's RocksDB single-host boundary. They must not add dual writes, migration readers, legacy fallback, legacy wallet-table preservation, or a competing storage abstraction.

## Repository validation

The version-1 query and compatibility packages passed formatting, all-target checks, strict Clippy, 12 query library tests, and 63 compatibility acceptance tests with 29 existing live-gated tests skipped. The wallet-rebuild command passed its CLI unit test and package check. The patched SDK native test passed, the arm64 native library built, and the pinned ZODL APK built, installed, and executed on the physical device.
