# ADR-0017: Derive-consumer template, shared block context, and key codec convention

Status: Accepted
Date: 2026-05-19
Related: [ADR-0009](0009-explorer-plane-as-product-surface.md),
[ADR-0011](0011-explorer-freshness-envelope.md),
[Derive plane](../architecture/derive-plane.md),
[Fact-first indexer](../architecture/fact-first-indexer.md)

Revisions:

- 2026-07-04: The registration surface carries a per-consumer schema version:
  `DeriveConsumerSchema` binds a consumer's name, `schema_version`, and owned
  column families in one declaration. Versioning semantics live in
  [ADR-0028](0028-per-consumer-derive-schema-versioning.md).

Bundled derive writes run inside `zinder-ingest`'s derive tailer. The durable
contract in this ADR is the shared per-block fact context,
`BlockKeyedConsumer` range dispatch convention, key codecs in
`zinder-core::wire`, and atomic derive cursor persistence. Current
implementations live in `crates/zinder-derive`, and `zinder-ingest` hosts the
bundled tailer.

## Context

The explorer plane materializes column-family projections through reusable
derive consumers: `BlockSummaryConsumer`, `TransactionFeesConsumer`,
`TransparentAddressActivityConsumer`, and `TransactionHistoryConsumer`.
Each consumer needs (a) typed block-header and transaction facts at the height
being applied, (b) transparent spend facts for the block's transparent inputs
when the view computes fees or address deltas, and (c) a small per-height key
encoding for whichever column-family layout the consumer chose.

Letting each consumer roll its own block-context hydration, transparent-spend
hydration, key bytes, and range-loop scaffolding would multiply boilerplate by
N and let drift between consumers compound.

## Decision

### Per-block context shared across consumers

`zinder-ingest`'s derive tailer hydrates one `BlockCommitContext` per committed
block and passes shared references to every chain-event consumer observing that
height. The context carries block identity, block time, raw block size, ordered
`TransactionFactsArtifact` rows, and hydrated `TransparentSpendFact` rows when
the view needs transparent input values. Consumers do not fetch
`WalletQuery.FullBlock`, hold `WalletQueryClient` handles, parse raw block
bytes, or resolve transparent outputs over gRPC.

### Consumer trait split

`DeriveConsumer` is the SDK-facing trait the derive tailer calls; it has
`apply_chain_committed` and `apply_chain_reorged`.

`BlockKeyedConsumer` is the convention every production consumer implements:

- `name(&self) -> DeriveConsumerName`
- `apply_block(&mut self, &BlockCommitContext, &mut DeriveConsumerCtx<'_>)`
- `revert_block(&mut self, BlockHeight, &mut DeriveConsumerCtx<'_>)`

A blanket `impl<C: BlockKeyedConsumer> DeriveConsumer for C` provides
the range loop on top: `apply_chain_committed` walks
`start_height..=end_height`, pulls each `BlockCommitContext` from the
tailer-provided in-memory map, and calls `apply_block`;
`apply_chain_reorged` walks the reverted range calling `revert_block` then
walks the replacement range calling `apply_block`.

Test-only consumers that don't fit the per-block shape implement
`DeriveConsumer` directly without paying the per-block scaffolding tax.

### Key codec primitives

Key encoding lives in `crates/zinder-core/src/wire/`. Each primitive is
a `pub fn encode_*` and matching `decode_*`:

- `wire::height_key`: `encode_height_key_ascending(BlockHeight) -> [u8; 4]`
  (lexicographic = oldest-first), `encode_height_key_descending` (=
  `u32::MAX - height`, lexicographic = newest-first), and decoders.
- `wire::address_script_hash`:
  `encode_address_script_hash(TransparentAddressScriptHash) -> [u8; 32]`
  and decoder.
- `wire::in_block_position`:
  `encode_in_block_position(u32) -> [u8; 4]` for the per-block tx
  position component of composite keys.
- `wire::unix_seconds`:
  `encode_unix_seconds(u64) -> [u8; 8]` for time-bucketed projections.

Composite keys are concatenations of the above plus per-consumer
trailing tags. The conventions actually in use:

- per-block ascending: `[height_key_ascending(4)]` (`BlockSummary`,
  the `transaction_fees_index` family, the
  `transparent_address_activity_index` family)
- per-tx by id: `[transaction_id_internal(32)]`
  (`TransactionFeesConsumer` primary records)
- per-block-position descending: `[height_key_descending(4) | in_block_position(4)]`
  (`TransactionHistoryConsumer`)
- per-address descending: `[address_script_hash(32) | height_key_descending(4) | in_block_position(4)]`
  (`TransparentAddressActivityConsumer` primary records)
- per-second: `[unix_seconds(8)]`
  (`MempoolEventCountsConsumer`)

A consumer that needs a new shape adds the primitive to `wire/` and
documents the layout here, rather than encoding bytes inline. Inline
`.to_be_bytes()` on heights, positions, addresses, or unix seconds at
derive-store boundaries is a forbidden pattern.

### Freshness helpers

`DeriveStore` ships two helpers paired with the height-key primitives:

- `last_materialized_height_ascending(cf)` — decodes the last key as
  four-byte big-endian ascending height. Used by `BlockSummaryConsumer`
  and the explorer block-view handler.
- `last_materialized_height_descending(cf)` — decodes the first key
  (lowest reverse-height bytes correspond to highest height) as
  descending-height bytes. Used by every consumer that keys on
  reverse-height as the primary discriminator.

A consumer whose freshness signal is not chain-height (a mempool-driven
consumer, for example) defines its own helper on the store rather than
decoding bytes inline in a handler.

### Per-consumer schema versioning

Registration is one declaration per consumer: `DeriveConsumerSchema` binds a
consumer's stable `DeriveConsumerName` to a `schema_version` and the column
families it owns. Requiring the version at construction makes it impossible to
register a column family without one. The bundled consumers ship as
`DeriveStore::bundled_consumers()`; every consumer starts at version 1.

The persisted per-consumer manifest, the open-time scoped wipe-and-rebuild
flow, and the narrowed container-format version are defined in
[ADR-0028](0028-per-consumer-derive-schema-versioning.md).

## Rewind contract

Every chain-events consumer implements `revert_block(height, ctx)`. The
shape of the implementation depends on the key layout:

- **Per-height ascending key** (`BlockSummary`): one `delete_cf` per
  height. The blanket `apply_chain_reorged` walks the reverted range
  and calls `revert_block` per height.
- **Per-block-position descending composite**
  (`TransactionHistoryConsumer`): one `delete_range_cf` per height over
  the 4-byte `height_key_descending` prefix.
- **Composite where height is NOT a prefix**
  (`TransparentAddressActivityConsumer`,
  `TransactionFeesConsumer`): the consumer maintains a per-height
  *index* column family keyed by ascending height whose value is the
  concatenated list of secondary discriminators it wrote at that height
  (txids for fees, `(address, in_block_position)` pairs for
  address activity). On revert it reads the index for the reverted
  height, deletes each primary row by the reconstructed composite key,
  then deletes the index entry. All deletes go in the same batch as the
  cursor advance.

The two-CF index pattern is what makes rewind correct under reorg: the
canonical block at the reverted height has changed, so re-fetching from
the wallet would return the new block, not the one whose rows we
actually wrote. The persisted index captures what was actually
materialized at apply time.

## Consequences

- A new consumer that fits the per-block shape implements
  `BlockKeyedConsumer` (three methods) plus a key codec and an error
  enum. The trait blanket gives it the range-loop scaffolding for free.
- A new wire-key shape requires one primitive in
  `crates/zinder-core/src/wire/`. Subsequent consumers reuse it.
- `wire_invariants.rs` extends with bans for inline
  `.to_be_bytes()` on derive-store key fields when the temptation
  arises.
- Shared `BlockCommitContext` values bound derive replay cost to one canonical
  fact hydration pass and one transparent-spend hydration pass per height, even
  as the consumer count grows.
- Composite-key consumers that don't have a height prefix accept the
  index-CF overhead (a few dozen bytes per row at apply time) in
  exchange for correct, single-batch rewinds.
