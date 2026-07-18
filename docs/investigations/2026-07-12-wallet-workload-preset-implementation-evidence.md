# Wallet Workload Preset Implementation Evidence

Status: Superseded historical evidence
Date: 2026-07-12
Author: Gustavo Valverde
Requirements: `docs/prd/wallet-workload-presets.md`
Plan: `docs/plans/wallet-workload-presets.md`

The fact-first cutover in
[`ADR-0035`](../adrs/0035-fact-first-storage-selection-and-lifecycle.md)
supersedes this report's production composition and backup paths. The legacy
backup command and bundled canonical-plus-derive manifest described below are
deleted; this file remains only as evidence for the earlier implementation.

This report records the evidence produced by the vertical slices of the wallet workload preset. The implementation exposes a fresh-store `wallet` or `explorer` choice, carries that choice through production composition and recovery, and keeps `explorer` as the default. The dense and recent fixed-input controls satisfy the public-selection gate; live evidence establishes the narrower boundaries that were exercised rather than treating the preset as blanket wallet certification.

Naming note (2026-07-15): the measured explorer arm was emitted as `complete`
by the historical binaries. Tables below use the current semantic name
`explorer`; current configuration rejects the old value instead of treating it
as an alias.

## Implemented boundary

The durable contract is a selected set of projection identities, not a store-global preset value. `wallet` currently expands to `transparent_address_transaction_history` and `transparent_outpoint_spend`; `explorer` expands to all 18 bundled projections. Each catalog entry also declares its product role, recovery source, preset membership, and retention authority. Only durable outpoint-spend history may acknowledge canonical transparent-spend retention.

The selection crosses fresh-store registration, chain-event dispatch, cursor consensus, startup guards and seeds, replay, snapshot bootstrap, tailing, background backfills and verification, retention, typed reads, capability decisions, tests, and backup metadata. One ingest-owned startup plan maps projection identities to the work they own. A wallet plan runs only the spend-retention guard, selected event replay, and selected event tailer; optional explorer-mode jobs do not start. The plan rejects a derive store opened with a different selection before it calls the source or mutates projection state. Production accepts `projection_preset = "wallet"` or `"explorer"`; omission resolves to `explorer`. Canonical facts and `raw_blob_policy` remain unchanged and independent of projection selection.

The initial typed-read slices cover both product planes. Wallet history and durable spender reads report their own projection availability, cursor, materialized height, and coverage against the visible tip. Explorer transaction history reports omitted, materializing, available, or fully verified coverage; its v1 and v2 capabilities follow those states independently, and v2 also requires the projection epoch, tip height, and tip hash to match the current canonical position. Schema incompatibility fails at store open before either reader starts.

## Requirement traceability

| Requirement | Implementation evidence | Validation boundary |
| --- | --- | --- |
| R-PRESET-1 | The projection catalog expands the closed `wallet` and `explorer` vocabulary to stable identities and rejects unknown presets. | Catalog, CLI, environment, TOML, persisted-manifest, and process tests cover both values and invalid input. |
| R-PRESET-2 | Omitted configuration resolves to `explorer`, whose catalog contains all 18 bundled projections. | A fresh local Z3 regtest deployment started without a preset, synchronized, reopened, and reported the historical explorer-arm name plus all 18 identities. |
| R-WALLET-1 | `wallet` contains only `transparent_address_transaction_history` and `transparent_outpoint_spend`. | Unit, integration, regtest, and testnet evidence all report exactly those 2 identities; omitted explorer reads return typed unsupported responses. |
| R-ROLE-1 | Every catalog entry declares product role, recovery source, preset membership, retention authority, schema, and owning column families. | Catalog completeness and ownership tests reject incomplete or duplicate declarations. |
| R-CANONICAL-1 | Presets select derive work only; both modes use the same canonical commit, epoch, reorg, event, and retention contracts. | Canonical contract suites pass, and both presets replay the same fixed fixture to the same tip with zero projection lag. |
| R-PAYLOAD-1 | `raw_blob_policy` and coverage remain independent configuration. `transactions` gates lightwalletd transaction retrieval; `all` gates full blocks. | Configuration and capability tests cover `none`, `transactions`, and `all`; the projection preset does not rewrite the configured payload policy. |
| R-READ-1 | Wallet and explorer readers evaluate the selected identity, schema, cursor, materialized height, epoch, and verified coverage. | Integration tests cover omitted, materializing, lagging, available, and fully verified states. |
| R-CAPABILITY-1 | Server information derives each capability from its canonical, payload, projection, and writer dependencies. | Query, explorer, and compatibility tests prove independent capability removal; live wallet stacks advertise wallet capabilities while omitting explorer-only capabilities. |
| R-LIFECYCLE-1 | Storage-pair preflight validates persisted selection and recovery safety read-only before writable reconciliation. | Mutation-sentinel tests prove rejected starts leave paths, manifests, cursors, rows, and physical column families unchanged. |
| R-LIFECYCLE-2 | Preset transitions fail with a rebuild instruction; no in-place expansion, reduction, or reclamation path exists. | Wallet-to-explorer, explorer-to-wallet, foreign-identity, and retained-canonical mismatch tests all fail closed. |
| R-OPS-1 | Printed configuration, readiness, server information, workload identity gauges, per-projection replay, lag, writes, bytes, startup recovery, and RocksDB properties expose the effective workload. | Exact-image regtest and testnet scrapes show only selected identities, independent write counters, replay height at the visible tip, zero lag, and per-projection compaction state. |
| R-BACKUP-1 | Backup manifest v2 records network, preset, canonical position, history bounds, and each projection's identity, schema, position, and `exact`, `behind`, or `omitted` state. Restore admission recomputes the manifest before opening a writer. | Round-trip, tamper, mismatch, behind, omission, historical-admission, and post-restore-readiness tests pass; a live checkpointed testnet backup self-validated. |
| R-BENCHMARK-1 | The harness captures immutable workload density and replays wallet and explorer arms over independent canonical clones. | Recent and dense controls show repeatable derive-write, disk, replay, and reopen reductions with zero lag; a final exact-image smoke confirms both arms still traverse the production pipeline. |
| R-FUTURE-1 | Presets persist projection identities rather than RocksDB column-family names, process locations, or backend types. | Startup planning and reader admission consume the catalog contract, leaving worker placement and storage adapters outside the public vocabulary. |
| R-CLAIM-1 | Documentation separates method coverage, configured support, local integration evidence, and release certification. | Zally is recorded as local consumer evidence; Fauzec and Zexplorer production observations are explicitly reference-only. |

## Lifecycle and recovery evidence

The storage-pair preflight opens the derive store read-only before any writable reconciliation. It rejects wallet-to-explorer and explorer-to-wallet mismatches, arbitrary foreign consumer identities, canonical history without its projection store, projection data without canonical history, and a retention-authoritative spend projection behind an irreversible canonical sweep. Tests confirm that rejected preflights do not create the derive path and leave manifest rows, cursors, projection rows, and the physical column-family list unchanged. Errors direct operators to create a new empty canonical path and re-ingest.

Backups now assemble the canonical checkpoint, derive checkpoint, and `zinder-backup-manifest.json` in a sibling staging directory. Manifest v3 records the network, effective preset, immutable raw-blob retention, canonical epoch, tip height, tip hash, artifact schema, durable canonical-history bounds, and every projection's identity, schema, cursor, materialized height when defined, and `exact`, `behind`, or `omitted` state. Complete history records height 1 as its first available artifact; checkpointed history records the checkpoint height and hash plus its derived successor height. Mixed backfill projections remain conservatively `behind` even when they have a live cursor; wallet projections require both an authenticated cursor and a materialized canonical-tip height; transaction history requires verified epoch, height, and hash coverage.

Before publication, Zinder strictly parses and structurally validates the staged manifest, reopens both staged RocksDB checkpoints through secondary-mode scratch directories, recomputes every recorded position, and requires an exact manifest match. The scratch directories are removed before the staging directory is atomically renamed and its parent synced. Tampered manifests and altered projection checkpoints leave the final destination absent and the staging bundle quarantined.

Restore admission now runs before the canonical primary opens. A restored working copy carrying `zinder-backup-manifest.json` is fully reopened and recomputed through the same strict validator used before backup publication. Admission compares the manifest retention, the canonical checkpoint's persisted retention, and the configured writer policy before it consumes pending evidence. Successful admission atomically renames that historical manifest to `zinder-restore-admission.json` and syncs the directory. Later restarts validate the admission record's network, preset, raw-blob retention, catalog, schemas, and structure without comparing an advanced live store to its old checkpoint position. Conflicting, malformed, mismatched, or tampered evidence fails closed. The recorded `exact` state never becomes readiness by itself; normal projection coverage remains the only capability authority. Existing-store preset transitions, derive-only repair after canonical retention, and automatic disk reclamation remain outside the supported lifecycle.

The backup command now discovers the selected workload from the persisted derive manifest before restore admission or primary open. A wallet store is therefore validated, checkpointed, and recorded as `wallet`; the command no longer assumes that every source store is `explorer`.

A wallet-specific retention test now exercises the real replay path. Safe-tip advancement before replay leaves the canonical spend fact intact because no durable release floor exists. Wallet replay materializes the spender and publishes its durable height; the derive store is then closed and reopened. A later safe-tip advancement deletes the canonical spend fact, while the reopened derive store still resolves the spender and passes the startup retention guard. This proves the 2-projection execution path preserves the flush-before-release correctness boundary across restart and canonical pruning.

## Recent-range benchmark

The paired control used the same captured testnet source bytes and cloned canonical starting state for each run:

- Range: 4,158,544 through 4,159,043, inclusive.
- Input: 500 blocks and 633 transactions.
- Runs used for the comparison: paired runs 6, 7, and 8.
- Projection replay scope: `fixed-range` with a fresh derive store.
- Block preparation concurrency: 16.

The table reports the median of 3 runs. Row counts, logical write bytes, and final disk use were deterministic across the runs.

| Metric | Wallet | Explorer | Observed change |
| --- | ---: | ---: | ---: |
| Projection rows | 3,620 | 14,700 | 75.4% lower |
| Serialized derive write-batch bytes | 398,153 | 1,530,947 | 74.0% lower |
| Final derive-store bytes | 564,359 | 1,948,755 | 71.0% lower |
| Derive replay seconds | 0.180 | 0.217 | 17.0% lower median |
| Populated-store reopen seconds | 0.034 | 0.193 | 82.4% lower median |
| Projection lag in blocks | 0 | 0 | Equal |
| Derive compaction bytes | 0 | 0 | Inconclusive |
| Peak resident bytes | 100,331,520 | 101,707,776 | Inconclusive |

The reduction in rows, logical writes, and final disk use is large and repeatable. The reopen ranges also did not overlap: wallet took 0.027 to 0.044 seconds, while explorer took 0.148 to 0.485 seconds. These results establish that projection fan-out has a material storage and recovery cost on this recent range.

This short control does not decide the gate alone. Canonical throughput ranged from 1,228 to 2,325 blocks per second for wallet and 908 to 3,523 for explorer. Derive replay times overlapped, peak resident memory overlapped from roughly 100 to 107 MB, and the range caused no compaction. The dense mainnet control below supplies the required sustained workload comparison.

## Dense mainnet control

The gate-closing control uses a captured mainnet fixture for heights 100,000 through 149,999 and an exact canonical checkpoint at height 99,999. The checkpoint contains 99,999 blocks, 489,778 transactions, 4,991,220 transparent inputs, and 18,628,842 transparent outputs. The replay fixture contains 50,000 blocks, 382,767 transactions, 11,956,873 non-coinbase transparent inputs, 12,707,771 transparent outputs, and 2,423,779,501 raw bytes.

The density record shows sustained work rather than one dominant burst: 84.412% of blocks contain transparent inputs, every block contains transparent outputs, and the maximum block contributes only 0.113% of the range's transparent inputs. Both arms started from independent clones of the same checkpoint, consumed the same immutable fixture, used the same binary and resource limits, and reached zero projection lag.

| Metric | Wallet | Explorer | Wallet change |
| --- | ---: | ---: | ---: |
| Canonical replay blocks per second | 181.79 | 188.21 | 3.41% lower; within repeat-run noise |
| Projection rows | 56,216,457 | 104,825,002 | 46.37% lower |
| Serialized derive write-batch bytes | 8,306,237,664 | 16,311,353,143 | 49.08% lower |
| Derive compaction bytes | 7,441,564 | 17,580,738 | 57.67% lower |
| Final derive-store bytes | 5,398,511,190 | 11,447,674,177 | 52.84% lower |
| Derive replay seconds | 277.063 | 472.559 | 41.37% lower |
| Populated-store reopen seconds | 0.458 | 1.543 | 70.28% lower |
| Peak resident bytes | 8,350,687,232 | 8,573,894,656 | 2.60% lower; inconclusive |
| Projection lag in blocks | 0 | 0 | Equal |

An independent repeat reproduced the roughly 49.1% logical-write reduction with the same density totals. Its canonical arm ran at 193.54 blocks per second for wallet and 180.46 for explorer, reversing the small difference in the table and confirming that canonical throughput remained inside run-to-run noise. The result establishes a repeatable derive-write reduction outside noise, improves compaction, disk use, and populated-store recovery time, preserves zero lag and wallet correctness, and does not show a memory regression. It therefore satisfies R-BENCHMARK-1. Docker Desktop validation also established an execution constraint for reproducibility: writable RocksDB stores use ext4-backed named volumes because virtiofs bind mounts produced invalid padded SST sizes under direct I/O.

## Live topology evidence

The explorer preset ran in the production 3-service topology against both local networks with Zaino disabled. Validation uncovered that Docker's global `zinder-ingest:latest` tag and the original `zinder-testnet-data` volume had also been used by the uncommitted Cipherscan worktree. That image persisted its private `block_production_time` consumer and 3 owned column families. The current branch correctly rejected that foreign manifest, first during writable reconciliation and then, after the storage-pair hardening, during read-only preflight. No compatibility exception or destructive migration was added for an unshipped consumer. The original testnet service was restored to its immediately preceding image with Zaino disabled.

Current-branch testnet validation therefore uses an isolated Compose project, network, ports, and Zinder volume while sharing only the existing Z3 testnet Zebra and cookie. A fresh store bootstrapped 200 blocks behind tip, caught up to height 4,163,680, and brought ingest, query, and explorer to ready. Native wallet `LatestBlock` and compact-block range reads succeeded. Explorer returned `explorer.transaction.history_v2` pages with a projection read fence and served 5 block-production points without the foreign Cipherscan projection.

The first isolated restart found 2 checkpoint-boundary assumptions in the optional paid-fee projection. Its historical floor walked below the first retained header into the artifactless checkpoint, and its tail seed treated a spend of a pre-checkpoint output as fatal even though the paid-fee contract already models unavailable exact fees. The fix initializes the live tail before resolving optional history, clips historical coverage at the first retained header through the range-read contract, and records transactions with unresolved pre-checkpoint parents as unavailable rather than inventing a fee. Existing-parent/missing-output and duplicate-outpoint failures remain fatal. The rebuilt writer reopened the same isolated volume, persisted the paid-fee floor at the first retained height, caught up, and returned to ready.

The next slice replaced those inferred floors with one durable `CanonicalHistoryBounds` contract owned by the canonical store. A normal first commit atomically records complete history and must prove height 1; the dedicated artifactless-checkpoint commit atomically records the checkpoint height and hash. Legacy full stores are accepted only when height 1 is readable. Legacy checkpoint stores require the configured checkpoint to match epoch 1 and, after progress, require the first retained header to connect to its hash. Height-addressed reads below the floor return intentional unavailability, while a missing or corrupt artifact at or above it remains a storage failure.

Commitment-root, conventional-fee, paid-fee, transaction-component, transaction-history verification, value-pool-balance, and value-pool-flow jobs now derive their historical start from that contract. Commitment roots additionally respect Sapling activation. Coverage records keep the actual retained floor, but explorer completeness remains domain-based: checkpoint-bounded transaction history advertises v1, not full-history v2; fee time ranges do not claim completeness; commitment-root negative searches are definitive only when the retained suffix includes the full Sapling domain.

The rebuilt isolated testnet stack migrated the existing checkpoint store, caught up, and brought ingest, query, and explorer to the same tip with Zaino explicitly disabled. Across 2 full 3-service restarts, all services returned ready, a 5-block compact range streamed successfully, and none of the 7 checkpoint-sensitive retry events recurred. The earlier `replica_lagging` result was a validation-topology error: both Compose projects advertised the same shared-network service name, so the isolated query sometimes dialed the original writer. Project-unique internal aliases now keep each reader paired with its intended writer.

An offline backup of that live store reopened and self-validated both staged checkpoints. Its manifest recorded checkpoint 4,163,473, first available height 4,163,474, the checkpoint hash, canonical tip 4,163,748, and checkpoint-bounded transaction history as `behind`. This is the intended result because local catch-up does not prove genesis-complete history.

One paid-fee limitation remains explicit. A checkpointed store can observe a transparent spend whose parent predates retained history, but a missing parent lookup does not yet prove the parent's height. Complete-history stores fail on a missing parent; checkpointed stores record the fee as unavailable. Durable parent provenance or an authenticated node lookup is still required to distinguish a genuine pre-checkpoint parent from lost retained transaction facts.

A fresh regtest volume synchronized to height 2,123. The first run exposed a paid-fee startup race: the one-shot seed ran before the first canonical epoch, and the background task assumed the tail existed. A regression test now proves that the background task waits for canonical and derive progress, initializes the durable tail, and then starts reconciliation and backfill. The clean-volume rerun, a forced container recreation on the same volume using the final startup-plan binary, and a restart after backup all reached ready state without the previous retry loop.

At the regtest tip, query advertised transparent history and durable spender capabilities. Explorer advertised transaction-history v1 and v2, returned a bounded history page with a read fence, and reported complete verified coverage through height 2,123. The backup command then produced an atomic canonical-plus-derive bundle with projection-aware metadata.

Those earlier live containers exercised `explorer`. After the benchmark gate passed, fresh public `wallet` deployments were brought up against the same Z3 regtest and testnet Zebras, with Zaino disabled and isolated ext4-backed data volumes.

The live transparent-wallet acceptance flow uses the 2-projection public wallet store instead of the bundled explorer store. Against both the local regtest Zebra and the synced Z3 testnet Zebra, checkpointed catchup served native and lightwalletd-compatible compact ranges, latest block, tree state, checkpoint-aware subtree roots, transparent UTXOs and history, and transaction lookup when transaction bytes were retained. The derive store declared the transaction-history and durable-spender projections and omitted optional product projections. No Zaino endpoint participated.

Production readers inspect that durable manifest before opening their derive secondaries. Process and live tests verify that `ServerInfo` reports `wallet`, the 2 stable identities, and both projection-backed wallet capabilities. The lightwalletd adapter advertises `taddrSupport` only when transaction blobs are retained and both projection cursors and materialized heights cover the canonical tip. Explorer `ServerInfo` remains available on a wallet store, omits explorer-only capabilities, and treats calls into omitted explorer views as unsupported instead of surfacing missing-column-family internals.

That test expansion exposed a wallet-correctness gap at the checkpoint boundary. The durable spender consumer previously joined each child input to the hydrated parent spend-fact map and silently skipped an input whose parent lay below retained history. It now derives the spent outpoint, input index, spending transaction, height, and block hash from the child transaction facts alone. Unit tests cover empty and offline parent-fact maps, and an integration test proves a checkpoint-crossing spend resolves through `WalletQuery.TransparentSpendsByOutpoint` after wallet replay.

The public regtest and testnet stacks each ran ingest, query, explorer, and the compatibility adapter against a fresh wallet store. Readiness, server information, and metrics reported `wallet` plus exactly the 2 selected identities. Explorer omitted explorer-only capabilities and returned typed `UNIMPLEMENTED` for omitted transaction history. Every process restarted over the same persistent volume and returned ready with the same selection. A target-height ingest run over a separate fresh volume stopped in under 1 second after committing height 2,000; reopening the store also exited cleanly. This verified that cancellation can interrupt a long derive replay without advancing a partially processed event.

A forced regtest reorg replaced height 2,586 and extended the new branch to 2,587. A live native subscription emitted a typed `ChainReorged` event with the reverted and replacement hashes, both wallet projections replayed the replacement branch, and transparent history returned the new block hashes. Zally's native source test passed again after the reorg.

The final observability slice initially exposed a false-lag bug during a safe-tip-only event. An empty committed range encodes its sentinel at the settled tip, while the authenticated chain epoch already names the visible tip; using that sentinel regressed both projection gauges by 100 blocks. Replay progress now uses the event envelope's visible tip. Progress attribution also distinguishes block-keyed consumers from event-only consumers, so explorer mode no longer assigns block-consumer progress to `reorg_incidents`; the event-only tail records its own progress after dispatch. Regression tests cover the safe-tip-only case. After rebuilding the exact release images and advancing regtest, the wallet stack reported both selected projections at height 2,933 with zero lag, separate write and byte counters, startup recovery results, and per-projection RocksDB live-data, SST, memtable, reader-memory, pending-compaction, and running-compaction values. The testnet wallet stack independently reached height 4,164,630 with all 4 services ready and the same 2 identities.

A separate fresh regtest deployment omitted `projection_preset` and therefore exercised the default. Ingest and query synchronized and reopened at height 2,933, emitted the historical `complete` label for the arm now named `explorer`, and declared all 18 identities. All 16 block- or event-keyed replay projections reported the visible tip and zero lag; the mempool-only and pure historical-backfill projections did not invent block-lag semantics.

## Wallet-consumer validation

Zally's native `ZinderChainSource` was traced against the preset contract. It consumes latest and safe block positions, compact ranges, exact tree state, subtree roots, transaction status, transparent UTXOs, cursor-bound chain events, and the shared transaction-submission channel. Those methods require canonical wallet facts plus `transparent_address_transaction_history` and `transparent_outpoint_spend`; they do not require an explorer projection. Fauzec embeds that same Zally chain source and submitter, so its indexer dependency has the same wallet-preset boundary. Zexplorer consumes explorer histories, distributions, rankings, and summaries and therefore remains an explorer-preset consumer.

The local Zally checkout at base commit `0b35587` passed its ignored native-source test against the isolated current-branch Zinder query service on both regtest and testnet. The funded regtest acceptance flow used the public wallet preset and completed in 203.194 seconds. It created and synchronized a wallet, received transparent funds, mined the maturity interval, shielded funds, sent shielded funds, extracted and submitted a PCZT, broadcast through Zinder, mined and rescanned the transaction, and verified its payment disclosure. The final branch build repeated that flow successfully at regtest height 2,932. A separate 36.677-second flow verified pending-input and mempool protection: after the first shield broadcast, Zally retained the pending transparent inputs and refused an immediate conflicting shield. Zaino was disabled. The Zally checkout contains pre-existing uncommitted work, so this is local integration evidence rather than release certification for a reproducible Zally commit.

Read-only production observations supply consumer-boundary evidence without claiming that this branch or the wallet preset is deployed. On 2026-07-13, `fauzec.com` reported testnet live and ready at height 4,164,625 with zero wallet lag and equal scanned and chain-tip heights; its public network, faucet-status, donation-summary, and health endpoints responded. On the same date, `zexplorer.app` reported both configured networks reachable and served testnet overview, mempool, and reorg response envelopes. Overview and mempool were live at height 4,164,625 with zero lag. The network response's operator state was also live and at tip, while its generic freshness envelope remained `catching_up` without an indexed tip; the reorg response contained 25 recorded events but reported `awaiting_upstream` with no chain tip despite the same indexed tip. Those deployed semantic observations are outside this branch and are not counted as preset evidence.

## Reproducible density evidence

Fixture format 2 records the workload density produced by the same deterministic block parser used by canonical ingest. The manifest and every replay report now include total raw block bytes, transactions, non-coinbase transparent inputs, transparent outputs, blocks populated by transparent inputs and outputs, and per-block maxima. Totals prove the amount of work; the populated-block counts and maxima reveal whether one block dominates an otherwise sparse range. Round-trip and real-pipeline replay tests cover the new manifest contract, and strict `zinder-bench` Clippy remains clean.

The release Docker image then captured 10 blocks from the existing Z3 regtest node and wrote a format-2 manifest with 10 measured blocks, 11 transactions, 10 transparent outputs, 25,476 raw bytes, populated-block counts, and per-block maxima. A second container capture of heights 1 through 10 replayed through independent wallet and explorer stores. The final exact-image smoke repeated that replay with ext4-backed named volumes. Both arms reproduced the identical density object and reached zero projection lag; wallet wrote 40 projection rows and 3,631 logical bytes, while explorer wrote 225 rows and 22,462 logical bytes. This smoke input is intentionally sparse and is not performance evidence; it proves that the release container's capture, manifest admission, real canonical pipeline, both projection arms, and JSON report agree on the new contract.

## Repository gate

The final branch-wide gate passed after all implementation and documentation changes. Formatting and whitespace checks, workspace type checking, strict all-target and all-feature Clippy, rustdoc warnings, dependency policy, unused-dependency analysis, and the workspace coverage build and test run are clean. The CI-profile suite ran 1,541 tests across 31 binaries: all 1,541 passed, with 91 tests intentionally skipped by the profile or filter. The performance profile ran 3 tests: all 3 passed. `cargo deny` retained only the repository's allowed duplicate-version warnings. Live network evidence remains reported separately because readiness and wallet-serving behavior are stronger boundaries than repository tests alone.

## Gate decision

The public preset gate passes. The dense mainnet control demonstrates repeatable reductions in derive writes, rows, final disk use, replay time, and populated-store recovery time while preserving zero projection lag and the wallet correctness contract. The production selector remains deliberately narrow: `wallet` or `explorer`, fresh stores only, with `explorer` as the default.

The implementation does not turn preset support into blanket wallet certification. The release claim is limited to the configurations, networks, interfaces, and wallet revision recorded above. Payload retention remains an independent deployment decision: native canonical and projection reads can operate with `raw_blob_policy = "none"`, lightwalletd transparent transaction retrieval requires `transactions` or `all`, and full-block reads require `all`. Paid-fee parent provenance remains a limitation of checkpointed explorer analytics, not a blocker for the wallet preset.
