# ADR-0018: Capability-gated optional payload fields

Status: Accepted
Date: 2026-05-19
Related: [ADR-0009](0009-explorer-plane-as-product-surface.md),
[ADR-0017](0017-materialized-view-consumer-and-key-codec.md)

## Context

Several wire fields in the explorer plane carry values that depend on
upstream state the operator may or may not have wired:

- `TransactionDetailResponse.paid_fee_zat` requires the
  `TransactionFeesConsumer` projection. The admissible detail path parses the
  raw transaction bytes retained by the admitted wallet endpoint; a canonical
  secondary is not an alternative transaction-fact provider. The current
  release wallet endpoint does not advertise the two required transaction
  capabilities, so an operator-built explorer binary paired with that wallet
  endpoint omits TransactionDetail entirely.
- `TransparentAddressActivityEntry.net_value_zat` requires resolved prevout
  evidence in the admitted activity projection; the row materializes either
  way.
- `TransactionHistoryEntry.zip317_conventional_fee_zat` is unset for
  coinbase rows; non-coinbase rows always carry a value.

We need one convention for "this endpoint structurally supports field X, while
the field's value may still be legitimately absent for a particular row" that
consumers can use without inferring support or missing evidence from sentinel
values like zero.

## Decision

Optional payload fields paired with named capability strings are the
convention. Specifically:

1. A singular value whose absence is meaningful uses proto3 explicit presence
   (`optional <scalar>` or an optional message). An empty repeated field is a
   legitimate empty collection, not a generic presence signal; its companion
   capability describes structural support. Sentinels like zero or empty bytes
   are never overloaded to mean missing evidence.

2. When a supported field's row-level absence has multiple causes a reader
   might want to distinguish, the message carries a companion enum alongside
   the optional value. The current example is
   `TransparentAddressActivityRecord.prevout_resolution_status`
   (`Resolved | Partial | Unavailable | Unspecified`), which is set on
   every row so the handler renders a chip rather than guessing.

3. The capability string is registered in `zinder-proto::capabilities` and
   added to the `CAPABILITIES` table. The table owns vocabulary, surface,
   ordering, and method association; it does not decide whether a running
   endpoint supports the field.

4. Structural dependency absence omits the method or field capability and
   suppresses both the optional field and its supporting read. Row-level
   missing evidence leaves the optional field absent and uses its companion
   status where the response defines one. A transient failure of an admitted
   dependency is a typed request error, never structural omission or row-level
   absence. When that dependency belongs to the runtime's readiness contract,
   its owning health projection also makes readiness false. The handler never
   emits a partial sentinel: never `paid_fee_zat = 0` to mean "unresolved",
   never `net_value_zat = 0` to mean "we only saw one side of the transaction".

5. Each service derives one immutable endpoint capability set after all
   dependencies have been admitted and before it binds gRPC or operational
   listeners. Evidence is concrete: exact materialized-view consumer
   membership, admitted storage, configured providers, and the capability
   strings returned by an authenticated dependency's `ServerInfo`. Mutable
   readiness, replica lag, backfill progress, and current epochs never rewrite
   structural support.

6. A field capability is retained only when at least one response method that
   carries the field is admitted by the same endpoint. The carrier invariant is
   endpoint-local; the protocol registry remains free of runtime composition
   policy.

7. The finalized adapter consults its frozen set at the emission point. It
   suppresses the optional value and avoids the supporting read when the field
   capability is absent, even if a stored row or upstream response contains a
   value. The gRPC `ServerInfo` and operational endpoint share the exact same
   immutable capability allocation.

## Examples shipped under this convention

| Field | Capability | Status-companion field |
| ----- | ---------- | ---------------------- |
| `TransactionDetailResponse.paid_fee_zat` | `explorer.transaction.fees_v1` | `prevout_resolution_status` |
| `TransactionDetailResponse.transparent_inputs[].value_zat` | `explorer.transaction.fees_v1` | (status on the parent) |
| `TransparentAddressActivityRecord.net_value_zat` | `explorer.transparent_address.activity_v2` | `prevout_resolution_status` on the record |
| `RecentTransactionEntry.zip317_conventional_fee_zat` | `explorer.transaction.recent_v1` | (none; `is_coinbase = true` explains absence) |
| `RecentTransactionEntry.paid_fee_zat` | `explorer.transaction.fees_v1` | (none; absence means "not provable from retained facts") |
| `TransactionHistoryEntry.zip317_conventional_fee_zat` | `explorer.transaction.history_v2` | (none; `is_coinbase = true` explains absence) |
| `TransactionHistoryEntry.intrinsic_value_balances` | `explorer.transaction.intrinsic_value_balances_v1` | (none; absence remains unknown and never means all-zero balances) |
| `TransactionDetailResponse.intrinsic_value_balances` | `explorer.transaction.intrinsic_value_balances_v1` | (none; absence remains unknown and never means all-zero balances) |
| `TransactionHistoryEntry.paid_fee_zat` | `explorer.transaction.fees_v1` | (none; absence means "not provable from retained facts") |
| `BlockTransaction.transparent_inputs[].value_zat` fee-projection fallback | `explorer.transaction.fees_v1` | (none; retained parent values remain independent) |
| `BlockTransactionsResponse.final_note_commitment_roots` | `explorer.block.final_note_commitment_roots_v1` | (none; individual pool roots remain optional by activation and artifact availability) |
| `CommitmentRootSearchResponse.displaced_matches` and `.displaced_coverage` | `explorer.commitment_root.displaced_matches_v1` | `displaced_coverage` explains the retained range when the field capability is present |
| `UtxoSetSummaryResponse.commitment` | `explorer.utxo_set.commitment_v1` | (none; absence is required when the field capability is unavailable) |
| `MinedTransaction.raw_transaction_bytes` | `wallet.read.transaction_bytes_v1` | (none; absence means "transaction blob not retained") |

`MinedTransaction.raw_transaction_bytes` is the wallet-surface example.
The field is `optional bytes`. The native query derives
`wallet.read.transaction_bytes_v1` from admitted persisted transaction-blob
retention, and the handler carries the store's `Option<Vec<u8>>` into the
field. No caller-populated policy or support boolean can add the claim.

## Persisted blob retention as capability evidence

Blob-serving wallet capabilities are gated on the retention the writer
persisted, not advertised unconditionally:

- Full-block retention admits `wallet.read.full_block_at_v1` and
  `wallet.read.full_block_range_v1`.
- Transaction-blob retention admits `wallet.read.transaction_bytes_v1`.

The signal travels in a `StorageControl` `raw_blob_policy` singleton
(key byte 16, one-byte value: `0 = none`, `1 = transactions`,
`2 = all`). An empty primary may replace the signal before its first canonical
commit. After that commit, the value is the store's immutable historical
coverage contract: opening the primary with another policy fails with
`RawBlobRetentionMismatch` and requires a rebuild. Readers use only the
persisted value. A non-empty store with no signal is corrupt and
fails closed; it is never treated as `none`.

## Materialized-view support is structural

An attached materialized-view store supports only the consumers named in its
admitted manifest. Explorer capability derivation checks those exact stable
identities. Materializing, partial, and fully covered states do not change the
advertised set. A request made while a structurally present consumer is not yet
ready returns its typed materialization outcome; responses that expose
coverage continue to report the mutable range in-band.

## Consequences

- Consumers branch on proto field presence (`response.paid_fee_zat
  .is_some()`) and on the capability list returned by
  `ServerInfo.capabilities`, never on sentinel comparisons. The capability
  means the endpoint structurally admits the fee projection and
  transparent-input resolution; it does not promise an actual fee for
  shielded or unclassified transactions.
- Adding a new field of this shape requires three diff sites in the
  same change: the proto, the capability constant + uniqueness/coverage
  tests, and the handler that populates it. When the field needs a
  status companion, the record gains a fourth diff site.
- Operators see in `/healthz` and `ExplorerQuery.ServerInfo` which optional
  fields the endpoint may populate, since the capability list mirrors the
  structural proto-field decisions.
- Adding a field capability requires its exact structural evidence, carrier
  methods, emission guard, negative composition proof where one exists, and a
  current production consumer. Readiness flags and generic capability-policy
  layers are not extension points.
