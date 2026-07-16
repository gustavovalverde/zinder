# Fact-First Live Validation Evidence

Status: scoped implementation evidence, not topology certification
Date: 2026-07-15
Revision: `9eb509808d88b23e00df8aa1624fb18065f9d114`
Network: Zcash testnet

The current `rocksdb-single-host` deployment can rebuild a fresh testnet Zinder
store, converge its legacy projections, restart, and correctly serve the
sampled native wallet operations and smoke contract. Canonical catchup reached
the Zebra tip in 16 minutes and 46 seconds, projection replay converged in
another 11 minutes and 52 seconds, and the complete wallet-ready lifecycle took
28 minutes and 39 seconds. A separate read-only verifier scanned all 4,173,145
persisted replay envelopes and their canonical headers in 143.78 seconds
without finding a mismatch.

This is meaningful end-to-end evidence, but it is not evidence that the target
fact-first runtime is complete. The tested writer still expands block replay
facts into the legacy canonical schema, performs historical prevout reads, and
runs wallet and explorer projection work inside `zinder-ingest`. PostgreSQL is
still a diagnostic fact-store driver rather than a runtime composition. The
remaining architecture work is therefore ownership removal and lifecycle
construction, not another round of tuning the existing coupled schema.

## Deployment boundary

The run deliberately deleted the previous Zinder testnet containers and data,
then reused the standard deployment identity:

- Compose project: `zinder-testnet`;
- canonical and projection volume: `zinder-testnet-data`;
- ingest control and operations ports: `19100` and `19105`;
- wallet query and operations ports: `19101` and `19106`; and
- upstream: the existing `z3-testnet-zebra-1` full node and its cookie volume.

The old Zebra chain state was retained. The new Zinder volume started empty,
which makes the observed lifecycle a fresh Zinder reconstruction from the
running node rather than a snapshot restore or copied database.

The test ran with an 8 GiB ingest memory limit. Query-image construction
overlapped part of canonical catchup, and one canonical fsync tail took 82.095
seconds. The canonical result is therefore a conservative local measurement,
not an uncontended hardware maximum.

## End-to-end timeline

| Stage | Elapsed time | Average rate | Result |
| --- | ---: | ---: | --- |
| Fresh canonical catchup to height 4,172,908 | 16m 46.2s | 4,147 blocks/s | Reached the historical Zebra tip |
| Legacy projection replay after the canonical-first gate opened | 11m 51.7s | 5,864 blocks/s | Projection position reached canonical |
| Process start to zero projection lag | 28m 38.8s | n/a | Wallet serving became current |
| Full replay-envelope and canonical-header verification | 2m 23.78s | 29,025 blocks/s | All 4,173,145 pinned blocks passed |

Steady canonical bands usually processed between 5,000 and 7,000 blocks per
second. No source-fetch, prepare, commit, or projection error occurred during
the reconstruction. The ingest process did not restart or run out of memory.
Projection replay reached a peak working set of approximately 6.17 GiB.

The phase gate behaved as designed: bulk canonical catchup owned the resource
budget first, projection replay started after canonical entered tip following,
and wallet readiness waited for projection convergence. This proves the
scheduling policy on the legacy runtime. It does not prove that canonical and
wallet storage have been separated into their target services.

## Correctness evidence

The live validation used independent source and serving checks rather than
interpreting readiness as correctness:

- Zebra and Zinder block hashes matched at genesis, height 280,000, height
  1,500,000, height 3,633,000, and current testnet heights.
- Wallet query output, transparent balance, and transparent history results
  matched the corresponding Zebra observations for the sampled data.
- The native gRPC smoke passed after reconstruction and after service restart.
  It exercised capability discovery, latest block, height and hash selectors,
  typed block headers, and the documented unknown-transaction error.
- `IngestControl.WriterStatus` reported `WRITER_PHASE_FOLLOWING_TIP`, an
  upstream gap of zero, and `DERIVE_HEALTH_LIVE` at the same height as the
  canonical visible tip.
- Query reopened the populated store and became ready in approximately 30
  seconds. A first ingest restart took approximately 53 seconds, including
  38.3 seconds of ranking-snapshot activation. Once that snapshot existed, a
  later ingest restart completed in approximately 3 seconds.

The full-store verifier pinned one secondary-visible chain epoch before
scanning. For every height it decoded the replay envelope, rejected
non-canonical encoding, recomputed its semantic digest, compared the envelope
header with the separately persisted canonical header, and checked parent,
checkpoint, and pinned-tip continuity. Its report was:

| Field | Value |
| --- | --- |
| Scope | `replay_envelope_and_canonical_header_parity` |
| Pinned epoch | 4,547 |
| Range | 1 through 4,173,145 |
| Block count | 4,173,145 |
| Ordered digest | `658bbcaa32b81524caf2728ce1c141af7eb886019bf8a7db695dd3b10f7ed482` |
| Wall time | 143.78 seconds |

That verifier proves store-internal replay and canonical-header parity. It
does not independently reparse Zebra consensus bytes, prove every transaction
or output against a separate implementation, or replace the sampled Zebra and
wallet-serving parity checks.

## Fact-store candidate evidence

The isolated storage campaign used the production-intended direct dependency
`tokio-postgres` 0.7.18 and PostgreSQL 18.4. The fixture covered mainnet heights
178,000 through 180,000: 2,001 blocks, 62,675,167 raw bytes, 18,737
transactions, 228,262 transparent inputs, and 569,654 transparent outputs.
Its ordered semantic digest was
`36ea981b2b062b1608a4190b33f387fac7e7af886259170839b098ae6c028c57`.

Five alternating, non-overlapping RocksDB and PostgreSQL pairs consumed the
same warm fixture. Every arm decoded its persisted replay rows through a fresh
reader, recomputed all fact digests, checked scalar columns and chain
continuity, and reproduced the fixture digest.

| Metric | RocksDB median | PostgreSQL median |
| --- | ---: | ---: |
| Wall time | 2.329s | 2.548s |
| Throughput | 859.2 blocks/s | 785.2 blocks/s |
| Final fact-store bytes | 49,338,549 | 50,003,968 |
| Whole-arm sampled memory | 221,220,864 | 466,006,016 |
| Whole-arm sampled storage peak | 53,563,392 | 148,926,464 |

RocksDB had 9.43% higher median throughput and 8.61% lower median wall time in
this local campaign. PostgreSQL's final fact table and indexes were only 1.35%
larger, while its sampled whole-arm memory was 2.11 times higher and its
storage high-water mark was 2.78 times higher because the measurement included
database runtime and WAL effects. PostgreSQL emitted 50,869,920 WAL bytes per
trial.

The engine difference is not large enough to choose the architecture. The
fixture is an early transparent-heavy diagnostic, not the later mainnet hotspot
near height 1.869 million or a modern shielded workload. The driver excludes
Zebra fetching, full canonical publication, reorgs, projection construction,
query serving, failover, and snapshot restore. It proves equivalent semantic
fact persistence and supplies capacity clues. It does not certify either full
topology or prove that the new schema is faster than the current runtime.

## Bottleneck ranking

The evidence supports this causal order:

1. The architectural bottleneck remains canonical ownership of wallet state.
   Historical prevout lookup and wallet-shaped canonical writes are still
   present. A sparse testnet reconstruction can be fast while the same design
   collapses on dense positive-read bands.
2. Legacy projection construction is the next lifecycle cost. It consumed
   11m 52s after canonical catchup and reached approximately 6.17 GiB, even
   though it was correctly prevented from delaying the canonical stage.
3. Storage tails and startup-only index work create visible latency variance.
   The 82.095-second fsync tail and the first 38.3-second ranking-snapshot
   activation deserve bounded-stage metrics, but neither explains the known
   dense-band collapse.
4. Source transport was not the active limit in this run. More source
   concurrency cannot remove the foreground cross-block lookup dependency.
5. RocksDB versus PostgreSQL raw fact persistence is a secondary decision. Both
   candidates wrote and verified the dense fixture in seconds; topology,
   failover, read scaling, operational cost, and complete lifecycle evidence
   should decide which deployment an operator selects.

## Architecture consequence

The stable topology names remain `rocksdb-single-host` and
`postgres-scale-out`. Their display names may be “Single-host RocksDB” and
“Scale-out PostgreSQL.” `embedded` is inaccurate because Zinder remains a
service group, and `production` would incorrectly imply that a single-host
RocksDB deployment cannot be production-grade. Self-hosted and managed
PostgreSQL installations can satisfy the same scale-out contract, so
`managed-postgres` would also be too narrow.

The implementation sequence should now be:

1. Cut canonical schema vNext and remove every cross-block wallet lookup and
   wallet-owned index from canonical commit.
2. Implement fresh RocksDB canonical construction and live following over the
   same replay contract, then rerun the full mainnet workload anchors.
3. Add the independent wallet projection store and `zinder-projector` build,
   verify, catch-up, follow, and promotion lifecycle.
4. Shadow-compare wallet projection results with the legacy store, cut
   `WalletQuery` over behind an exact projection fence, then delete the legacy
   canonical wallet tables and wallet consumers from `zinder-derive`.
5. Certify the complete `rocksdb-single-host` lifecycle, including reorg,
   restart, checkpoint bundle, restore, and wallet-client parity.
6. Implement the PostgreSQL canonical writer, epoch-pinned read sessions,
   transactional event outbox, writer-generation fence, TLS, and replica lag
   contract using the same domain operations.
7. Implement the PostgreSQL wallet projection and certify
   `postgres-scale-out` with stale-writer rejection, failover, replica reads,
   restore, and the same wallet-client parity suite.
8. Move explorer projections after the wallet plane is stable. Do not put
   ClickHouse, SQLite, libSQL, Turso, or arbitrary backend mixing on the
   critical path. Delete `zinder-derive` only after its remaining explorer
   consumers have moved.

Fresh construction should continue reporting canonical elapsed time, wallet
projection elapsed time, wallet-ready elapsed time, canonical gap, projection
gap, memory high-water, and storage high-water separately. A single “sync
time” hides which owner is slow. Snapshot restore remains a separate
minutes-scale product and must not be used to claim that fresh reconstruction
meets its hours-scale gate.

## Current gate decision

The implementation is ready to proceed to canonical schema vNext. The replay
contract, atomic RocksDB persistence, full-store verifier, direct
`tokio-postgres` driver, Compose benchmark, and real service deployment are
credible tracer bullets. They remove enough uncertainty to stop debating
engines in the abstract.

No complete topology is newly certified by this report. `rocksdb-single-host`
continues to be the working deployment using the legacy coupled schema.
`postgres-scale-out` remains an accepted target with a diagnostic driver only.
The next release claim must come from the new canonical schema plus the
independent wallet projection lifecycle, not from extrapolating these
measurements.

## Version-1 RocksDB storage construction

Status: storage construction certified; serving topology not yet certified
Date: 2026-07-16
Revision: `d102acb94ad03fcf518824f3d42f569888c7084d`
Image: `sha256:057dd9a9536bf761a1e4da4ff0e4d7a2c0add8ee3c7d9c5b5a952f4f7942f90f`
Trial: `testnet-20260716T053041Z`

The clean version-1 `rocksdb-single-host` construction path rebuilt canonical
and wallet storage from height 1 through testnet height 4,175,080 in 15 minutes
47.21 seconds. Canonical storage reached `READY` in 11 minutes 16.05 seconds;
the independent wallet projection then reached `READY` in 4 minutes 31.16
seconds. Both stores passed semantic validation, publication, and a final cold
reopen at the same authenticated canonical fence.

The runner reused `z3-testnet-zebra-1`, its Docker network, and its read-only
cookie volume. It deleted only project-scoped Zinder canonical and wallet
volumes, and neither declared nor mounted Zebra's chain volume. Docker Desktop
exposed 10 CPU cores and 16,747,839,488 bytes of memory. The lifecycle
container had a 10-core quota and a 10 GiB memory limit; Zebra ran outside that
cgroup and remained healthy.

The host captured height 4,175,080 before starting the container. Zebra
advanced to 4,175,081 before source discovery and to 4,175,113 before canonical
loading ended. The build fence remained height 4,175,080 with hash
`96e1db9b5dc679f22775f93ca838a9066f04c2a50de5fa0eb035e2b5ebfe6600`,
so an advancing source could not move the acceptance range.

| Stage | Time | Result |
| --- | ---: | --- |
| Canonical source load | 8m 24.66s | 4,175,080 blocks and 4,786,733 transactions persisted |
| Canonical cold validation | 2m 51.29s | Every required canonical family admitted |
| Canonical storage ready | 11m 16.05s | Epoch 1, event 1, and the exact fixed tip cold-reopened |
| Wallet canonical scan | 46.35s | The same block and transaction counts observed |
| Wallet outpoint sort and merge | 1m 38.29s | 14,135,328 outpoint events reduced |
| Wallet secondary derivation | 21.28s | Address indexes and history constructed |
| Wallet cold validation | 1m 44.98s | Primary and secondary relations independently compared |
| Wallet storage ready | 4m 31.16s | Projection published at the canonical fence |
| Complete storage lifecycle | 15m 47.21s | Final canonical and wallet cold reopen passed |

The canonical source load averaged 8,273 blocks per second. Including full
cold validation and admission, canonical construction averaged 6,176 blocks
per second. The loader adapted segment size at dense testnet bands. The
captured log contained 30 density restarts and 2 response-size restarts; those
restarts discarded 422 completed speculative segments plus in-flight work.
This did not break ordered construction, but it is the leading source-side
tuning opportunity.

The canonical store occupied 12,349,170,949 bytes. Its ordered sequence digest
was `57112f5254593b7c290be4d75a39337959bd0820ee18fa2174286de9bd1d2740`.
The 2,704,566,725-byte wallet store published projection digest
`3ac9577d7ca1e2372c25691e0224e3e93f2c0885aa71955459066ff488499d9a`
and contained:

| Wallet family | Rows |
| --- | ---: |
| Transparent unspent outputs | 10,758,948 |
| Unspent outputs by address | 10,758,948 |
| Transparent spent outputs | 1,688,190 |
| Address transactions | 12,935,247 |
| Positive address balances | 337,252 |
| Reorg undo blocks | 100 |

Construction and cold validation performed zero historical prevout reads and
zero random validation reads. The outpoint events fit in one 2.26 GB accounted
run under a 4 GiB ceiling. The address-transaction sort used two initial runs
and one merge pass under its 1 GiB ceiling; cold validation independently
reproduced that bounded shape. The largest accounted reorg suffix was
2,882,488 bytes under a 512 MiB ceiling.

The exact private-cgroup memory peak was 6,496,649,216 bytes, approximately
6.05 GiB. Process peak RSS was 3,752,452,096 bytes, approximately 3.49 GiB.
Sampled allocated storage peaked at 18,114,859,008 bytes, approximately 16.87
GiB, while stores and external-sort staging files coexisted. The strict
validator pinned the image, revision, trial, resource profile, fixed tip,
version-1 schemas, row relations, sorter ceilings, zero-random-read invariants,
cold reopens, and full resource interval. Both the 10,000-block smoke and
full-tip run passed.

The measured bottlenecks are now explicit. Source loading consumed 504.66
seconds, 53.3% of the lifecycle. Canonical cold validation consumed 171.29
seconds, 18.1%. Wallet cold validation consumed 104.98 seconds, 11.1%, and the
wallet outpoint merge consumed 95.00 seconds, 10.0%. Increasing sorter memory
is not the important next change: the outpoint sort itself needed only 3.29
seconds.

The next pass should remove discarded source work and duplicated cold scans,
then parallelize only independent validation or merge work whose ordering does
not affect the version-1 contract. Every optimization must rerun the clean
full-tip gate. The remaining topology certification must still wire these
stores into live ingest, projection following, and query serving, then prove
reorg, restart, restore, and real wallet-client parity. PostgreSQL remains the
separate `postgres-scale-out` implementation after the RocksDB runtime contract
is stable; this evidence neither blocks nor certifies it.
