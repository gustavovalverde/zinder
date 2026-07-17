# Zcash Chain Workload Eras

This document records the Zcash mainnet height ranges that matter for Zinder
performance architecture. It is a benchmark and operator-context document, not
a runtime consensus table.

Zinder must keep two concepts separate:

- **Consensus epoch**: a protocol-rule interval activated by a Zcash network
  upgrade.
- **Historical workload band**: a chain interval with unusual block density,
  transaction shape, parser cost, or storage-write pressure.

Consensus epochs are runtime truth. Zinder discovers them from the configured
node through `NetworkUpgradeActivations` and the source boundary. Historical
workload bands are measurement context. They can seed perf fixtures, initial
budget expectations, and runbook interpretation, but they must not change
consensus parsing, canonical commit semantics, or public API contracts.

As of 2026-05-25, Zcash mainnet is in NU6.1. NU6.1 activated at height
`3146400` on 2025-11-24 at 19:56 UTC. NU7 is listed as planned by the public
Zcash upgrade page, but no NU7 activation height is part of this document's
current evidence model.

## Design Rule

Do not add "sandblasting mode", "May 2017 mode", or similar incident names to
the hot ingest path. Those names describe observed history, not protocol.

Use source-discovered `ConsensusEpoch` or `NetworkUpgradeActivations` when the
code needs consensus behavior. Use `HistoricalWorkloadBand` or
`ChainWorkloadBand` only for benchmark metadata, operational explanations, and
offline analysis. Avoid the term `era` in code if it would blur this boundary.

The bulk-catchup scheduler should adapt to measured cost:

- source response bytes
- transaction count and transaction-version mix
- transparent input, output, and spend-reference counts
- Sprout JoinSplit count
- Sapling spend and output counts
- Orchard action count
- compact-block artifact bytes
- checkpoint tree-state and subtree-root attachment cost
- estimated canonical write bytes
- RocksDB write batch, WAL, memtable, and compaction pressure
- stage duration and head-of-line waits

Historical bands may initialize a budget hint for a new benchmark run. Once
live measurements are available, the controller should respond to current
resource pressure rather than the block height label.

## Consensus Epochs

Mainnet activation heights below are stable historical facts. Runtime code still
discovers activation heights from the configured node because Regtest, Testnet,
and custom deployments can differ.

| Consensus epoch | Mainnet height range | Primary behavior for Zinder |
| --- | ---: | --- |
| Sprout | `0..347499` | Launch rules, transparent and Sprout transaction forms, 150 second target spacing, pre-Overwinter transaction semantics. Early blocks include the slow-start issuance period, which is not itself an indexing budget problem. |
| Overwinter | `347500..419199` | Introduces network-upgrade versioning, replay protection, transaction expiry, and transparent-transaction performance improvements. |
| Sapling | `419200..653599` | Adds the Sapling shielded pool and the first broadly mobile-oriented compact block scanning shape. |
| Blossom | `653600..902999` | Halves target block spacing from about 150 seconds to about 75 seconds, doubling expected block frequency while preserving emission over wall-clock time. |
| Heartwood | `903000..1046399` | Adds shielded coinbase and FlyClient commitments, changing block-header and wallet-relevant artifact expectations. |
| Canopy | `1046400..1687103` | First halving and development-fund rules; also carries Sapling note-plaintext changes and Sprout value-pool restrictions from the related ZIP set. |
| NU5 | `1687104..2726399` | Adds transaction v5, Orchard, Unified Addresses, and non-malleable transaction identifiers. This is a major parser, compact-artifact, and wallet-sync shape change. |
| NU6 | `2726400..3146399` | Adds funding and lockbox consensus changes. It does not define a new transaction version, so it is less parser-heavy than NU5 for Zinder. |
| NU6.1 | `3146400..next activation` | Extends funding and lockbox disbursement rules. It does not alter the block header, and current measurements do not make it a distinct parser-budget band. |

Primary sources:

- [Overwinter](https://z.cash/upgrade/overwinter/) activated at height 347500.
- [Sapling](https://z.cash/upgrade/sapling/) activated at height 419200.
- [Blossom](https://z.cash/upgrade/blossom/) activated at height 653600 and shortened block times.
- [Heartwood](https://z.cash/upgrade/heartwood/) activated at height 903000.
- [Canopy](https://z.cash/upgrade/canopy/) activated at height 1046400.
- [NU5](https://z.cash/upgrade/nu5/) activated at height 1687104 and introduced Orchard and transaction v5.
- [NU6](https://z.cash/upgrade/nu6/) activated at height 2726400.
- [NU6.1](https://z.cash/upgrade/nu6-1/) activated at height 3146400.
- [ZIP 204](https://zips.z.cash/zip-0204) lists mainnet and testnet network
  upgrade protocol versions and activation heights through NU6.1.
- [ZIP 253](https://zips.z.cash/zip-0253) defines NU6 deployment constants.
- [ZIP 255](https://zips.z.cash/zip-0255) defines NU6.1 deployment constants.
- [ZIP 214](https://zips.z.cash/zip-0214) ties Canopy, NU6, and NU6.1 funding stream heights to halving schedules.

## Historical Workload Bands

These bands are not consensus activations. They are known chain-shape intervals
that should influence benchmark coverage and budget validation.

| Workload band | Approximate height range | Confidence | Why it matters |
| --- | ---: | --- | --- |
| Launch slow start | `0..19999` | High | Issuance ramps up over the launch period. This is useful for complete-chain correctness tests but not a bulk-catchup bottleneck in modern storage. |
| May 2017 transparent-input stress | `~106178..~108888` | Medium | Public reports describe performance issues from transactions with many transparent inputs, higher memory pressure, and elevated orphan risk. This is a useful transparent spend-reference benchmark before Overwinter's transparent-transaction improvements. |
| Overwinter transparent fix boundary | `347500` and nearby | High | Benchmark both sides of the boundary because historical discussion ties one class of transparent-input worst-case behavior to pre-Overwinter rules. |
| NU5 activation boundary | `1687104` and nearby | High | Parser and artifact shape change: v5 transactions, Orchard actions, Unified Address ecosystem support, and non-malleable transaction identifiers. |
| Sandblasting and post-NU5 high shielded load | `~1702296..~2175692` | Medium | Network load increased shortly after NU5, with unusually large shielded transaction activity and wallet sync failures. Treat this as a broad workload era, not an exact activation interval. |
| zcashd 5.1.0 observed heavy blocks | `1708048`, `1723244` | High for benchmark anchors | zcashd release notes cite these as observed historic blocks where improved Sapling and Orchard validation reduced worst-case validation time. They should be retained as focused parser and compact-artifact fixtures. |
| ZIP 317 deployment response | Policy rollout, not a fixed chain range | Medium | ZIP 317 changed fee policy and block-template behavior in response to sustained high load. It affects future load dynamics more than historical consensus parsing. |

Approximate date-to-height anchors used for the incident bands:

- May 1, 2017 maps near height `106178`:
  <https://api.blockchair.com/zcash/blocks?q=time(2017-05-01%2000:00:00..2017-05-01%2000:10:00)&s=id(asc)&limit=1>
- May 4, 2017 maps near height `108099`:
  <https://api.blockchair.com/zcash/blocks?q=time(2017-05-04%2008:00:00..2017-05-04%2008:10:00)&s=id(asc)&limit=1>
- May 5, 2017 maps near height `108888`:
  <https://api.blockchair.com/zcash/blocks?q=time(2017-05-05%2017:00:00..2017-05-05%2017:20:00)&s=id(asc)&limit=1>
- June 14, 2022 maps near height `1702296`:
  <https://api.blockchair.com/zcash/blocks?q=time(2022-06-14%2000:00:00..2022-06-14%2000:10:00)&s=id(asc)&limit=1>
- July 31, 2023 maps near height `2175692`:
  <https://api.blockchair.com/zcash/blocks?q=time(2023-07-31%2023:50:00..2023-07-31%2023:59:59)&s=id(asc)&limit=1>

The May 2017 and sandblasting anchors are date-to-height approximations. They
select representative measurement ranges; they are not exact incident start or
stop claims.

## Benchmark Corpus

Use a small but deliberate corpus when testing parser, artifact, and
bulk-catchup changes. For each anchor, prefer a window around the height rather
than the single block alone.

| Anchor | Purpose |
| ---: | --- |
| `106178` | Start-side sample for the May 2017 transparent-input stress incident. |
| `108099` | Mid-incident sample for transparent input and memory-pressure behavior. |
| `108888` | Recovery-side sample after the May 2017 stress window. |
| `347500` | Overwinter activation boundary. |
| `419200` | Sapling activation boundary. |
| `653600` | Blossom activation boundary and block-frequency shift. |
| `903000` | Heartwood activation boundary. |
| `1046400` | Canopy activation boundary and first halving. |
| `1687104` | NU5 activation boundary. |
| `1702296` | Start-side sample for post-NU5 high transaction load. |
| `1708048` | zcashd 5.1.0 heavy-block validation benchmark anchor. |
| `1723244` | zcashd 5.1.0 heavy-block validation benchmark anchor. |
| `2175692` | End-side sample after ECC reported network load had returned to pre-NU5 levels by the end of July 2023. |
| `2726400` | NU6 activation boundary. |
| `3146400` | NU6.1 activation boundary. |

Perf tests should capture at least these output metrics per corpus range:

- blocks per second
- source response bytes per block
- prepared canonical bytes per block
- estimated canonical write bytes per block
- transparent spend references per block
- Sapling spends and outputs per block
- Orchard actions per block
- chain-epoch commit latency
- RocksDB WAL, memtable, and compaction pressure
- head-of-line wait time in source, block-prepare, and commit reassembly

## Measured Parser Cost

The current local validation source is the mainnet Zebra container
`z3-mainnet-zebra-1`. Its `getblockchaininfo.upgrades` table advertises
Overwinter `347500`, Sapling `419200`, Blossom `653600`, Heartwood `903000`,
Canopy `1046400`, NU5 `1687104`, NU6 `2726400`, and NU6.1 `3146400`.

The parser benchmark used local raw blocks from that node and ran
`SourceBlock::from_raw_block_bytes` followed by the function then named
`derive_block_with_raw_blob_policy(..., RawBlobPolicy::None)`. That function is
now named `prepare_canonical_block`; the historical
measurement still describes its pre-optimization implementation. Those values
are the baseline because that implementation parsed the complete block twice
and serialized plus parsed each transaction again. The current bulk source
parses only the header before canonical preparation, which parses the complete
block once and builds facts directly from the parsed transactions. JSON-RPC
fetch time is excluded. The timings below are local-machine measurements, so
they are useful for relative cost shape, not as an absolute SLA.

Use `zinder-bench` fixed-range replay for comparisons after the parse-once
change. Its `stage_durations` report makes the acceptance boundary explicit:
compare the same captured range, store state, retention policy, and prepare
concurrency, then require higher `replay.blocks_per_second` without increased
`commit_fallback_reads` or peak resident memory. Keep the heavy Orchard, heavy
Sapling, and end-side ranges in the comparison so one workload shape cannot
hide a regression in another.

| Window | Range | Component evidence | canonical block preparation timing |
| --- | ---: | --- | --- |
| May 2017 peak | `108089..108109` | `43,201` transparent spend references, `2,063` transparent outputs, `6.5 MB` raw block bytes | avg `3.8 ms`, p95 `15.9 ms`, max `21.0 ms` at `108106` |
| Heavy Orchard anchor | `1708038..1708058` | `3,754` Orchard actions, `13.3 MB` raw block bytes | avg `78.4 ms`, p95 `154.1 ms`, max `235.1 ms` at `1708048` |
| Heavy Sapling anchor | `1723234..1723254` | `7,591` Sapling outputs, `1,074` Sapling spends, `7.8 MB` raw block bytes | avg `294.4 ms`, p95 `1445.0 ms`, max `2090.3 ms` at `1723244` |
| Sandblasting end-side sample | `2175682..2175702` | `469` Sapling outputs, `268` Orchard actions | avg `14.2 ms`, p95 `25.0 ms`, max `25.1 ms` at `2175684` |
| NU5 boundary | `1687094..1687114` | Only `6` Orchard actions across the 21-block window | avg `2.9 ms`, p95 `8.4 ms`, max `16.3 ms` at `1687109` |
| NU6 boundary | `2726390..2726410` | `44` Orchard actions, small raw blocks | avg `0.9 ms`, p95 `3.2 ms`, max `4.0 ms` at `2726404` |
| NU6.1 boundary | `3146390..3146410` | `69` Orchard actions, small raw blocks | avg `1.1 ms`, p95 `3.9 ms`, max `6.3 ms` at `3146394` |

Single-block anchors show the same shape:

| Anchor | Raw bytes | Transactions | Dominant counts | canonical block preparation time |
| ---: | ---: | ---: | --- | ---: |
| `108099` | `99,149` | `11` | `551` transparent spend references | `0.8 ms` |
| `1708048` | `1,999,370` | `96` | `552` Orchard actions | `129.7 ms` |
| `1723244` | `1,991,912` | `469` | `1,862` Sapling outputs and `452` Sapling spends | `1742.7 ms` |
| `2175692` | `33,424` | `16` | mixed Sapling and Orchard, much lower density | `12.6 ms` |
| `3146400` | `1,945` | `1` | small NU6.1 activation block | `0.1 ms` |

The current performance model is:

- The worst sampled parser cost is not at a network-upgrade boundary. It is at
  dense historical shielded-load blocks, especially the Sapling-heavy
  `1723244` anchor.
- The May 2017 range is a different workload class: transparent
  spend-reference pressure. It is real, but it is not the dominant parser cost
  in this sample.
- NU5 introduces the parser and artifact shape that makes Orchard possible, but
  the activation boundary itself is not enough to size budgets. Budgets must
  respond to observed component density.

## Architecture Implications

The performance architecture should stay measurement-driven:

1. The source adapter fetches bounded raw block segments and records response
   density. It may split dense ranges, but it must not change parsing behavior
   based on a historical label.
2. Canonical block prepare computes per-block cost signals before batching. Dense
   shielded blocks, transparent-input-heavy blocks, and artifact-heavy blocks
   can all close batches through the same measured budget model.
3. The commit accumulator closes by resource budget, not only by block count.
   The strongest common denominator is estimated write bytes, with artifact
   bytes and block count as additional bounds.
4. RocksDB tuning belongs to bounded resource budgets: memtable size, WAL size,
   write batch size, flush behavior, and compaction pressure. Do not fork
   storage semantics by historical incident.
5. Derive replay should use the same canonical facts and chain events as normal
   ingest. A historical band may explain why replay is slower, but it should
   not create a second replay contract.

This keeps DX and AX clean: developers and agents reason about one adaptive
budget model instead of a pile of named exceptions copied from old incidents.
Users benefit because sync behavior improves for future unknown workloads, not
only for the historical cases we already know.

## Operations Guidance

Operators need bounded, durable metrics. Do not label Prometheus series by block
height, block hash, transaction id, or arbitrary workload-band string. Those
labels are too high-cardinality for production metrics.

Use this document to interpret metrics externally:

- A slow range around `1687104` likely reflects NU5 parser and artifact shape.
- A slow range around `1702296..2175692` likely reflects post-NU5 shielded load.
- A slow range around `106178..108888` likely reflects transparent input and
  spend-reference pressure.

If a bounded runtime label is ever added, it must be a small versioned enum and
owned by an architecture doc. The default posture is no band label in the hot
path.

## Source Notes

Network-upgrade facts are sourced from Zcash's public upgrade pages and ZIPs.
Incident ranges are sourced from public forum reports, ECC retrospectives,
zcashd release notes, and third-party date-to-height samples:

- [Zcash upgrade index](https://z.cash/upgrade/)
- [Zcash Improvement Proposals index](https://zips.z.cash/)
- [A look back: NU5 and network sandblasting](https://electriccoin.co/blog/a-look-back-nu5-and-network-sandblasting/)
- [Heavily increased transaction load since June 14](https://forum.zcashcommunity.com/t/heavily-increased-transaction-load-since-june-14/42349)
- [Performance issues on the network](https://forum.zcashcommunity.com/t/performance-issues-on-the-network/15664)
- [ZIP 317: Proportional Transfer Fee Mechanism](https://zips.z.cash/zip-0317)
- [zcashd 5.1.0 release notes](https://github.com/zcash/zcash/blob/3c2b1622f0a6c047e7f91fafb01965279c252d2a/doc/release-notes/release-notes-5.1.0.md)
