# Zebra Historical Block Transport Diagnostic

Status: local diagnostic evidence, not runtime or canary certification\
Date: 2026-07-16\
Revision: `5a3b1c1550d129835b22ab0703b1348e43451a34`\
Fixture: Zcash mainnet heights 1,730,000 through 1,734,999

Unary `Indexer.GetBlock` is not a sufficient production replacement for the
current batched JSON-RPC source. Raw binary gRPC cut client network input by
49.77% and reduced the adverse 5.82-second arm from 596.77 seconds to 393.85
seconds, but the best complete canonical-v1 lifecycle still reached only 13.22
blocks per second. The remaining limit is block-local canonical construction
CPU, with transaction fact extraction, block parsing, and compact artifact
construction accounting for most measured task time.

Do not wire the unary source into `zinder-ingest`. A historical server-streaming
range remains a concrete Zebra proposal, but transport work cannot meet the
dense-band target until the captured-input canonical path improves by at least
an order of magnitude.

## Endpoint capability

The existing local Zebra exposes finalized historical blocks through
`Indexer.GetBlock`. The proof used the running `z3-testnet-zebra-1` container,
image `zfnd/zebra:6.0.0-rc.0`, OCI revision
`15d578362448fb8c4a5d29a00dcfe8adb5184082`, and indexer listener
`0.0.0.0:18230`. The image includes Zebra revision
`676a4fddf83c8ad6eaeb81495da12faf65c32b22`, which added the method.

A request for testnet height 1,730,000 returned a 1,694-byte block more than 2.4
million blocks below the observed tip. Its displayed hash was
`00383c58cdc2e25505994b948433302c5adf0754a954fafb2734e717da1a8f96`, and both
gRPC and JSON-RPC returned bytes with SHA-256
`a0a1224a03555b84e82af14f838127379867adeb2186e13816e42d5bf563becc`. This proves
endpoint availability, height encoding, finalized historical lookup, and byte
identity on the local testnet node.

The indexer port is container-internal rather than host-published, and no local
mainnet Zebra exposes the dense fixture range. The dense comparison therefore
used captured mainnet bytes with controlled response delays; it does not measure
Zebra state-read cost.

## Authenticated experiment boundary

Every arm consumed the same immutable 5,000-block fixture. The fixture contains
3,829,454,475 raw block bytes, 58,285 transactions, 61,870 transparent inputs,
and 119,167 transparent outputs. Its manifest digest is
`1bdc7b4b774e3a5d1e30ac68e33aed102ed2cb89ad769a3d6d29c32eaba64435`, and its
canonical sequence digest is
`91e3fb1e71ce4893fbdb425f45b64f8a8a1b0551b38fe3fe50634d170ff251b9`.

The benchmark presented those bytes through 2 protocols on a private Docker
network. The current source used batched JSON-RPC `getblock` with verbosity 0;
the diagnostic source used concurrent unary gRPC `GetBlock` calls while
retaining JSON-RPC only for the atomic tip control fact. A
fixture-authentication wrapper compared every returned raw block with the
admitted fixture before canonical construction observed it. The controlled delay
applied once per JSON request or batch and once per gRPC unary response.

The replay client ran with 10 CPUs, 10 GiB of memory, a 402,653,184-byte source
watermark, 12 outer source requests, and 10 block preparation workers. The
fixture source ran with 2 CPUs and 2 GiB of memory. Each arm used a fresh named
RocksDB volume, published canonical READY, reopened the store cold, scanned all
5,000 blocks, and reproduced the fixture sequence digest. The source server
exposed only atomic tip and raw block methods, and the client mounted no legacy
or wallet store, so successful construction proves zero upstream
historical-prevout and cross-block wallet reads.

The benchmark image was
`sha256:66a9980fd113e14552e316e655ba046874c6390fc38d8b3b8ebdf7e98ddc56c4`.
Per-arm reports, cgroup counters, Docker inspections, resource samples, and
network deltas are preserved under `.tmp/zebra-indexer-grpc-diagnostic/` with
these trial prefixes:

- `dense-json-0ms-20260716T191147Z`;
- `dense-grpc12-0ms-20260716T192102Z`;
- `dense-json-2070ms-20260716T192829Z`;
- `dense-grpc512-2070ms-20260716T193642Z`;
- `dense-json-5820ms-20260716T195048Z`; and
- `dense-grpc512-5820ms-20260716T194331Z`.

## End-to-end result

The complete lifecycle includes source fetch, byte authentication, parsing,
block-local fact preparation, SST generation and loading, READY publication,
authenticated readback, and cold reopen.

| Source           | Injected delay | Unary ceiling |   Total | Blocks/s | Client CPU time | Client network input | Client memory peak |
| ---------------- | -------------: | ------------: | ------: | -------: | --------------: | -------------------: | -----------------: |
| Batched JSON-RPC |             0s |           n/a | 453.23s |    11.03 |  1,765.07 CPU-s |             7.667 GB |            5.03 GB |
| Unary gRPC       |             0s |            12 | 383.38s |    13.04 |  1,755.80 CPU-s |             3.849 GB |            4.76 GB |
| Batched JSON-RPC |          2.07s |           n/a | 457.54s |    10.93 |  1,729.63 CPU-s |             7.667 GB |            4.92 GB |
| Unary gRPC       |          2.07s |           512 | 378.07s |    13.22 |  1,712.48 CPU-s |             3.852 GB |            4.64 GB |
| Batched JSON-RPC |          5.82s |           n/a | 596.77s |     8.38 |  1,656.55 CPU-s |             7.667 GB |            4.71 GB |
| Unary gRPC       |          5.82s |           512 | 393.85s |    12.70 |  1,709.45 CPU-s |             3.850 GB |            4.40 GB |

Binary unary transport reduced zero-delay lifecycle time by 15.41%, the
2.07-second arm by 17.37%, and the 5.82-second arm by 34.00%. The improvement is
real but too small for the target. Client CPU time changed by less than 4%
across all arms, which shows that hex removal and lower wire volume do not
remove the dominant canonical work.

Every client stayed below the 10 GiB limit, and every store reached an
approximately 4.36 GB sampled storage high-water mark. The source container
reached its 2 GiB cgroup peak while sampled working memory stayed low, which is
consistent with reclaimable fixture page cache; Docker inspections reported no
out-of-memory kill. Its sampled CPU averaged between 0.015 and 0.044 cores, with
a maximum below 0.90 cores, so the 2-CPU source ceiling did not bind these runs.

## Request concurrency and admission

The request ceiling and achieved concurrency differ because the outer canonical
pipeline still owns byte admission. The server measured one transport unit per
JSON request or batch and one unit per unary gRPC call.

| Source                  | Delay | Responses/s during source interval | Mean latency | p95 latency | Max active | Time-weighted active |
| ----------------------- | ----: | ---------------------------------: | -----------: | ----------: | ---------: | -------------------: |
| Batched JSON-RPC        |    0s |                               1.63 |       0.026s |      0.074s |          8 |                 0.04 |
| Unary gRPC, ceiling 12  |    0s |                              14.95 |       0.004s |      0.010s |         12 |                 0.06 |
| Batched JSON-RPC        | 2.07s |                               1.58 |       2.096s |      2.140s |          8 |                 3.31 |
| Unary gRPC, ceiling 512 | 2.07s |                              14.98 |       2.088s |      2.116s |        336 |                31.27 |
| Batched JSON-RPC        | 5.82s |                               1.17 |       5.851s |      5.910s |          9 |                 6.84 |
| Unary gRPC, ceiling 512 | 5.82s |                              13.99 |       5.851s |      6.006s |        384 |                81.84 |

At 2.07 seconds, 12 one-block unary calls have a theoretical ceiling of 5.8
blocks per second. The fixed 402,653,184-byte watermark allowed the
high-concurrency arms to exceed the configured outer request count while
retaining bounded response bytes, but it limited observed unary concurrency to
336 and 384 rather than 512. That concurrency hid both injected delays well
enough to keep canonical work active; it did not raise the construction ceiling.

The retained-prefix source planner eliminated density-triggered discard in every
arm: density restart count and discarded completed response bytes remained zero.
JSON-RPC still hit one oversized-response restart, discarded 6 future in-flight
segments, and served 5,045 block responses for a 5,000-block range. The extra 45
blocks added 33,832,956 raw bytes, or 67,665,912 hex payload bytes. This
remaining refetch is measurable but below 1% of fixture bytes, so eliminating it
cannot close the target gap.

## Block-local CPU ceiling

The best 2.07-second gRPC arm spent 1,712.48 client CPU-seconds across 5,000
blocks, or 342.50 CPU-milliseconds per block. The largest instrumented
preparation families were stable across transports and delays:

| Canonical stage               |    Task time | Task time per block |
| ----------------------------- | -----------: | ------------------: |
| Transaction facts             | 850.59 CPU-s |            170.12ms |
| Block parse                   | 488.72 CPU-s |             97.74ms |
| Compact artifacts             | 198.95 CPU-s |             39.79ms |
| All other instrumented stages |   0.39 CPU-s |              0.08ms |

Transport changes cannot remove these costs because both sources feed identical
raw block bytes into the same parser and preparation path. High unary
concurrency improves utilization when JSON batches arrive slowly, but the client
still spends most CPU in transaction facts, parsing, and compact artifacts. The
captured-input path must reduce those costs and improve parallel efficiency
before another transport can produce an order-of-magnitude result.

## One-hour target decision

The current path cannot support a fresh mainnet sync in less than 1 hour. The
target comparisons are:

| Budget                            |  Required rate |    Best relevant result | Remaining multiplier |
| --------------------------------- | -------------: | ----------------------: | -------------------: |
| Whole-chain 1-hour average        | 948.5 blocks/s |          13.22 blocks/s |               71.72x |
| Dense band consumes the full hour | 131.5 blocks/s | 12.70 blocks/s at 5.82s |               10.36x |
| 30-minute dense-band budget       |   263 blocks/s | 12.70 blocks/s at 5.82s |               20.72x |

Applying the best dense-fixture rate to the 3,414,286-block fixed fence would
imply approximately 71.7 hours, but that is not a whole-chain estimate because
other eras have different density. The valid conclusion is narrower: this exact
dense workload misses every accepted rate by a large margin, so neither unary
gRPC nor a larger source watermark can establish a sub-hour lifecycle.

## Historical range streaming proposal

If Zebra adds a historical streaming surface, the smallest useful contract is a
fixed-range `GetBlockRange` rather than another generic indexer adapter. A
request should bind a start height, inclusive end height, expected predecessor,
and fixed observed tip identity. Each ordered response should carry height,
display-order hash, parent identity, and raw block bytes. Zebra should terminate
the stream on view disagreement instead of mixing best-chain views, and the
client should resume only from its last authenticated canonical fence.

The client should feed the stream through the existing byte watermark and
preparation admission, with no JSON block fallback and no dual write.
Measurements must separate Zebra state-read time, stream admission wait, raw
bytes, framing bytes, client decode, block parse, preparation, SST generation,
and database load. A captured-stream arm with the same delays remains the
transport control.

Streaming can remove 5,000 unary request envelopes and may let Zebra read
contiguous state more efficiently, but the present evidence predicts only a
secondary gain until block-local CPU improves. Begin a protocol implementation
only after the identical captured-input path demonstrates at least 131.5 blocks
per second or isolates a Zebra state-read limit that streaming can remove.

## Certification boundary

This investigation certifies local endpoint capability and local authenticated
transport experiments. It does not certify the shipped ingest composition,
continuous following, restart recovery, reorg handling, wallet projection,
client serving, mainnet Zebra database reads, Railway behavior, or production
readiness. No Railway service, production traffic, Zebra state, or external
volume changed during this work.
