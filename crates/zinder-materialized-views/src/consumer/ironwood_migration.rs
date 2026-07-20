//! `IronwoodMigration` materialized-view consumer.
//!
//! Projects the Orchard-to-Ironwood shielded migration described by the draft
//! ZIP "Orchard to Ironwood Migration" (github.com/zcash/zips PR #1317, branch
//! `pacu/ironwood-migration-unified`) into two consumer-owned column families:
//!
//! - [`IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY`]: one running-total record
//!   per canonical block, keyed by ascending height, carrying the cumulative
//!   Orchard and Ironwood value balances (net zatoshi that has left each pool)
//!   summed over every transaction from the first replayed block. This is a
//!   genuinely cumulative, non-idempotent-by-content materialized view: each block's
//!   record depends on its predecessor's running total, and reverting a block
//!   subtracts that block's own contribution back out of the running total
//!   rather than only deleting the row.
//! - [`IRONWOOD_MIGRATIONS_COLUMN_FAMILY`]: one record per migration
//!   transaction, keyed by `(ascending height, block-local transaction index)`
//!   so several migrations in one block get distinct keys. A row is written
//!   only for a transaction matching the loose migration predicate (Orchard
//!   value leaving the pool while Ironwood value enters it).
//!
//! Cohort grouping (by `orchard_anchor`) and denomination binning (by the
//! magnitude of the Ironwood side) are intentionally not materialized here;
//! per the compute-at-read-time convention the explorer handler groups a
//! bounded range of these raw rows in memory at request time.
//!
//! ## Conformance is an approximation
//!
//! The [`Migration::conformant`] flag applies the shape half of the draft ZIP's
//! predicate: no transparent legs, no Sapling legs, no Sprout joinsplits,
//! exactly one Ironwood action, and an Orchard anchor present. It does **not**
//! verify the ZIP-317 canonical fee or `lock_time == 0`, because
//! [`TransactionFactsArtifact`] carries neither a resolved fee nor a raw
//! lock-time value at this layer. `conformant == true` is therefore a
//! necessary-but-not-fully-sufficient approximation; a downstream consumer that
//! needs the full predicate must re-check the fee and lock time from raw bytes.
//!
//! ## Running total baseline
//!
//! The cumulative totals are relative to the earliest block replayed into the
//! materialized-view store. On a from-genesis rebuild that baseline is the genesis block,
//! so the totals are chain-absolute; on a materialized-view store first populated against
//! an already-pruned canonical store the baseline is the earliest retained
//! height.

use zinder_core::wire::{HEIGHT_KEY_LEN, encode_height_key_ascending};
use zinder_core::{BlockHeight, TransactionFactsArtifact, TransactionId};

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, MaterializedViewConsumerCtx,
    MaterializedViewConsumerError, MaterializedViewConsumerName, MaterializedViewConsumerSchema,
};
use crate::error::{MaterializedViewStoreColumnFamily, MaterializedViewStoreError};
use crate::store::MaterializedViewStore;

/// Column family holding one cumulative running-total record per block.
///
/// Key: 4-byte ascending block height. Value:
/// `POOL_TOTALS_VALUE_LEN` bytes of `cumulative_orchard | cumulative_ironwood
/// | block_orchard | block_ironwood`, each a big-endian `i64` zatoshi value.
pub const IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY: &str = "ironwood_migration_pool_totals";

/// Column family holding one record per migration transaction.
///
/// Key: `(4-byte ascending height, 4-byte block-local transaction index)`.
/// Value: `MIGRATION_VALUE_LEN` bytes of `transaction_id | orchard_balance |
/// ironwood_balance | orchard_anchor | conformant`.
pub const IRONWOOD_MIGRATIONS_COLUMN_FAMILY: &str = "ironwood_migrations";

/// Column families the consumer needs registered before its first write.
pub const IRONWOOD_MIGRATION_COLUMN_FAMILIES: &[&str] = &[
    IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY,
    IRONWOOD_MIGRATIONS_COLUMN_FAMILY,
];

/// Stable consumer name persisted in the SDK cursor table.
pub const IRONWOOD_MIGRATION_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("ironwood_migration");

/// On-disk schema declaration for the Ironwood-migration materialized-view consumer.
pub const IRONWOOD_MIGRATION_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(
        IRONWOOD_MIGRATION_CONSUMER_NAME,
        1,
        IRONWOOD_MIGRATION_COLUMN_FAMILIES,
    );

const TXID_LEN: usize = 32;
const ANCHOR_LEN: usize = 32;
const AMOUNT_LEN: usize = 8;
const TX_INDEX_LEN: usize = 4;

/// Length of one migrations-column-family key: 4 height + 4 transaction index.
const MIGRATION_KEY_LEN: usize = HEIGHT_KEY_LEN + TX_INDEX_LEN;

/// Length of one pool-totals record value.
const POOL_TOTALS_VALUE_LEN: usize = AMOUNT_LEN * 4;

/// Length of one migration record value.
const MIGRATION_VALUE_LEN: usize = TXID_LEN + AMOUNT_LEN + AMOUNT_LEN + ANCHOR_LEN + 1;

/// Signed cumulative or per-block Orchard and Ironwood value balances.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PoolBalances {
    orchard_value_balance_zat: i64,
    ironwood_value_balance_zat: i64,
}

impl PoolBalances {
    const ZERO: Self = Self {
        orchard_value_balance_zat: 0,
        ironwood_value_balance_zat: 0,
    };

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            orchard_value_balance_zat: self
                .orchard_value_balance_zat
                .checked_add(other.orchard_value_balance_zat)?,
            ironwood_value_balance_zat: self
                .ironwood_value_balance_zat
                .checked_add(other.ironwood_value_balance_zat)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            orchard_value_balance_zat: self
                .orchard_value_balance_zat
                .checked_sub(other.orchard_value_balance_zat)?,
            ironwood_value_balance_zat: self
                .ironwood_value_balance_zat
                .checked_sub(other.ironwood_value_balance_zat)?,
        })
    }
}

/// Cumulative Orchard and Ironwood pool totals as of one block height.
///
/// Returned by [`IronwoodMigrationConsumer::read_pool_totals_at_or_before`] and
/// [`IronwoodMigrationConsumer::read_latest_pool_totals`]. The cumulative fields
/// are the running totals up to and including [`block_height`](Self::block_height);
/// the block fields are that single block's own contribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationPoolTotals {
    /// Height whose running total this record describes.
    pub block_height: u32,
    /// Cumulative net Orchard value balance in zatoshi up to this height.
    pub cumulative_orchard_value_balance_zat: i64,
    /// Cumulative net Ironwood value balance in zatoshi up to this height.
    pub cumulative_ironwood_value_balance_zat: i64,
    /// This block's own net Orchard value balance in zatoshi.
    pub block_orchard_value_balance_zat: i64,
    /// This block's own net Ironwood value balance in zatoshi.
    pub block_ironwood_value_balance_zat: i64,
}

/// One migration transaction's persisted facts.
///
/// Returned by [`IronwoodMigrationConsumer::read_migrations_in_range`]. The
/// migrated amount is the absolute value of
/// [`ironwood_value_balance_zat`](Self::ironwood_value_balance_zat): a migration
/// mints exactly one canonical Ironwood denomination, so the Ironwood magnitude
/// (not the Orchard side, which additionally covers the fee) is the amount
/// migrated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Migration {
    /// Height of the block containing the migration.
    pub block_height: u32,
    /// Block-local index of the migration transaction.
    pub tx_index_in_block: u32,
    /// Canonical identifier of the migration transaction.
    pub transaction_id: TransactionId,
    /// Signed net Orchard value balance in zatoshi (positive: value leaves the
    /// Orchard pool, covering the migrated amount plus the fee).
    pub orchard_value_balance_zat: i64,
    /// Signed net Ironwood value balance in zatoshi (negative: value enters the
    /// Ironwood pool).
    pub ironwood_value_balance_zat: i64,
    /// `anchorOrchard` root the migration's Orchard spends prove membership
    /// against; conformant migrations broadcast in one anchor bucket share it.
    pub orchard_anchor: [u8; ANCHOR_LEN],
    /// `true` when the transaction matches the strict shape predicate.
    pub conformant: bool,
}

impl Migration {
    /// Returns the migrated amount in zatoshi: the absolute value of the
    /// Ironwood side. The loose predicate only ever persists rows with a
    /// negative `ironwood_value_balance_zat`, so this is always the true
    /// migrated magnitude for a stored row.
    #[must_use]
    pub const fn migrated_amount_zat(self) -> u64 {
        self.ironwood_value_balance_zat.unsigned_abs()
    }
}

/// Materializes cumulative pool totals and per-migration records.
#[derive(Default)]
pub struct IronwoodMigrationConsumer {
    running_total: Option<PoolBalances>,
}

impl IronwoodMigrationConsumer {
    /// Builds the consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            running_total: None,
        }
    }

    /// Returns the cumulative pool totals at `height`, or the nearest earlier
    /// height when `height` sits above the highest materialized block.
    ///
    /// Returns `None` when `height` precedes the earliest materialized block.
    /// Materialized heights are contiguous from the earliest replayed block, so
    /// any in-range height resolves to an exact record.
    pub fn read_pool_totals_at_or_before(
        store: &MaterializedViewStore,
        height: BlockHeight,
    ) -> Result<Option<MigrationPoolTotals>, MaterializedViewStoreError> {
        if let Some(record) = Self::read_pool_totals_at(store, height)? {
            return Ok(Some(record));
        }
        match Self::read_latest_pool_totals(store)? {
            Some(latest) if latest.block_height < height.value() => Ok(Some(latest)),
            _ => Ok(None),
        }
    }

    /// Returns the cumulative pool totals at the highest materialized block, or
    /// `None` when no block has been materialized yet.
    pub fn read_latest_pool_totals(
        store: &MaterializedViewStore,
    ) -> Result<Option<MigrationPoolTotals>, MaterializedViewStoreError> {
        let Some((key, payload)) =
            store.last_consumer_entry(IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY)?
        else {
            return Ok(None);
        };
        let height = decode_height_key(&key)?;
        Ok(Some(decode_pool_totals_value(height, &payload)?))
    }

    /// Returns every migration record in `[start_height, end_height]` in
    /// ascending `(height, transaction index)` order, capped at `limit` rows.
    pub fn read_migrations_in_range(
        store: &MaterializedViewStore,
        start_height: BlockHeight,
        end_height: BlockHeight,
        limit: usize,
    ) -> Result<Vec<Migration>, MaterializedViewStoreError> {
        if limit == 0 || end_height.value() < start_height.value() {
            return Ok(Vec::new());
        }
        let start_key = migration_key(start_height, 0);
        let end_key = migration_key(end_height, u32::MAX);
        let entries = store.range_iterate_consumer(
            IRONWOOD_MIGRATIONS_COLUMN_FAMILY,
            &start_key,
            &end_key,
            limit,
        )?;
        let mut migrations = Vec::with_capacity(entries.len());
        for (key, payload) in entries {
            migrations.push(decode_migration(&key, &payload)?);
        }
        Ok(migrations)
    }

    fn read_pool_totals_at(
        store: &MaterializedViewStore,
        height: BlockHeight,
    ) -> Result<Option<MigrationPoolTotals>, MaterializedViewStoreError> {
        let Some(payload) = store.get_consumer(
            IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY,
            &encode_height_key_ascending(height),
        )?
        else {
            return Ok(None);
        };
        Ok(Some(decode_pool_totals_value(height.value(), &payload)?))
    }

    fn seed_running_total_for_apply(
        &mut self,
        store: &MaterializedViewStore,
        height: BlockHeight,
    ) -> Result<PoolBalances, MaterializedViewStoreError> {
        if let Some(running_total) = self.running_total {
            return Ok(running_total);
        }
        let base = match height.value().checked_sub(1) {
            Some(previous) => Self::read_pool_totals_at(store, BlockHeight::new(previous))?
                .map_or(PoolBalances::ZERO, pool_balances_from_record),
            None => PoolBalances::ZERO,
        };
        self.running_total = Some(base);
        Ok(base)
    }

    fn seed_running_total_for_revert(
        &mut self,
        store: &MaterializedViewStore,
    ) -> Result<(), MaterializedViewStoreError> {
        if self.running_total.is_none() {
            self.running_total = Some(
                Self::read_latest_pool_totals(store)?
                    .map_or(PoolBalances::ZERO, pool_balances_from_record),
            );
        }
        Ok(())
    }
}

impl BlockKeyedConsumer for IronwoodMigrationConsumer {
    fn name(&self) -> MaterializedViewConsumerName {
        IRONWOOD_MIGRATION_CONSUMER_NAME
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let block_balances = block_pool_balances(&block.transactions)?;
        let base = self.seed_running_total_for_apply(ctx.store, block.height)?;
        let cumulative = base.checked_add(block_balances).ok_or_else(|| {
            IronwoodMigrationConsumerError::RunningTotalOverflow {
                height: block.height.value(),
            }
        })?;
        self.running_total = Some(cumulative);

        let totals_cf = ctx
            .store
            .consumer_column_family(IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY)?;
        ctx.batch.put_cf(
            &totals_cf,
            encode_height_key_ascending(block.height),
            encode_pool_totals_value(cumulative, block_balances),
        );

        let migrations_cf = ctx
            .store
            .consumer_column_family(IRONWOOD_MIGRATIONS_COLUMN_FAMILY)?;
        for transaction in &block.transactions {
            if let Some(record) = migration_facts(transaction) {
                ctx.batch.put_cf(
                    &migrations_cf,
                    migration_key(block.height, transaction.location.tx_index_in_block),
                    encode_migration_value(&record),
                );
            }
        }
        Ok(())
    }

    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        self.seed_running_total_for_revert(ctx.store)?;
        let totals_cf = ctx
            .store
            .consumer_column_family(IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY)?;
        if let Some(record) = Self::read_pool_totals_at(ctx.store, height)? {
            let running_total = self.running_total.unwrap_or(PoolBalances::ZERO);
            let reverted = running_total
                .checked_sub(block_balances_from_record(&record))
                .ok_or_else(|| IronwoodMigrationConsumerError::RunningTotalOverflow {
                    height: height.value(),
                })?;
            self.running_total = Some(reverted);
        }
        ctx.batch
            .delete_cf(&totals_cf, encode_height_key_ascending(height));

        let migrations_cf = ctx
            .store
            .consumer_column_family(IRONWOOD_MIGRATIONS_COLUMN_FAMILY)?;
        ctx.batch.delete_range_cf(
            &migrations_cf,
            migration_key(height, 0).as_slice(),
            migration_range_end(height).as_slice(),
        );
        Ok(())
    }
}

/// Sums every transaction's signed Orchard and Ironwood value balances in a
/// block, treating an absent balance as zero.
fn block_pool_balances(
    transactions: &[TransactionFactsArtifact],
) -> Result<PoolBalances, MaterializedViewConsumerError> {
    let mut balances = PoolBalances::ZERO;
    for transaction in transactions {
        let facts = &transaction.public_facts;
        let contribution = PoolBalances {
            orchard_value_balance_zat: facts.orchard_value_balance_zat.unwrap_or(0),
            ironwood_value_balance_zat: facts.ironwood_value_balance_zat.unwrap_or(0),
        };
        balances = balances.checked_add(contribution).ok_or_else(|| {
            IronwoodMigrationConsumerError::RunningTotalOverflow {
                height: transaction.location.block_height.value(),
            }
        })?;
    }
    Ok(balances)
}

/// Classifies a transaction as a migration and captures its facts, or `None`
/// when the loose predicate does not fire.
///
/// The loose predicate is: Orchard value leaves the pool (`Some(v)`, `v > 0`)
/// while Ironwood value enters it (`Some(w)`, `w < 0`). The strict predicate
/// tightens this to the uniform migration shape and sets
/// [`Migration::conformant`].
fn migration_facts(transaction: &TransactionFactsArtifact) -> Option<Migration> {
    let facts = &transaction.public_facts;
    let orchard_value_balance_zat = facts.orchard_value_balance_zat?;
    let ironwood_value_balance_zat = facts.ironwood_value_balance_zat?;
    if orchard_value_balance_zat <= 0 || ironwood_value_balance_zat >= 0 {
        return None;
    }
    let counts = facts.counts;
    let conformant = !counts.has_transparent_input()
        && !counts.has_transparent_output()
        && counts.sapling_spend_count == 0
        && counts.sapling_output_count == 0
        && counts.sprout_joinsplit_count == 0
        && counts.ironwood_action_count == 1
        && facts.orchard_anchor.is_some();
    Some(Migration {
        block_height: transaction.location.block_height.value(),
        tx_index_in_block: transaction.location.tx_index_in_block,
        transaction_id: transaction.location.transaction_id,
        orchard_value_balance_zat,
        ironwood_value_balance_zat,
        orchard_anchor: facts.orchard_anchor.unwrap_or([0u8; ANCHOR_LEN]),
        conformant,
    })
}

fn pool_balances_from_record(record: MigrationPoolTotals) -> PoolBalances {
    PoolBalances {
        orchard_value_balance_zat: record.cumulative_orchard_value_balance_zat,
        ironwood_value_balance_zat: record.cumulative_ironwood_value_balance_zat,
    }
}

fn block_balances_from_record(record: &MigrationPoolTotals) -> PoolBalances {
    PoolBalances {
        orchard_value_balance_zat: record.block_orchard_value_balance_zat,
        ironwood_value_balance_zat: record.block_ironwood_value_balance_zat,
    }
}

fn migration_key(height: BlockHeight, tx_index_in_block: u32) -> [u8; MIGRATION_KEY_LEN] {
    let mut key = [0u8; MIGRATION_KEY_LEN];
    key[..HEIGHT_KEY_LEN].copy_from_slice(&encode_height_key_ascending(height));
    key[HEIGHT_KEY_LEN..].copy_from_slice(&tx_index_in_block.to_be_bytes());
    key
}

/// Exclusive upper bound covering every transaction index at `height`.
///
/// `delete_range_cf` excludes its end key; the all-`0xFF` transaction-index
/// suffix leaves only the unreachable `u32::MAX` index uncovered, matching the
/// height-prefixed range-delete convention the sibling consumers use.
fn migration_range_end(height: BlockHeight) -> [u8; MIGRATION_KEY_LEN] {
    let mut key = [0xFFu8; MIGRATION_KEY_LEN];
    key[..HEIGHT_KEY_LEN].copy_from_slice(&encode_height_key_ascending(height));
    key
}

fn encode_pool_totals_value(
    cumulative: PoolBalances,
    block: PoolBalances,
) -> [u8; POOL_TOTALS_VALUE_LEN] {
    let mut payload = [0u8; POOL_TOTALS_VALUE_LEN];
    payload[0..8].copy_from_slice(&cumulative.orchard_value_balance_zat.to_be_bytes());
    payload[8..16].copy_from_slice(&cumulative.ironwood_value_balance_zat.to_be_bytes());
    payload[16..24].copy_from_slice(&block.orchard_value_balance_zat.to_be_bytes());
    payload[24..32].copy_from_slice(&block.ironwood_value_balance_zat.to_be_bytes());
    payload
}

fn encode_migration_value(record: &Migration) -> [u8; MIGRATION_VALUE_LEN] {
    let mut payload = [0u8; MIGRATION_VALUE_LEN];
    payload[0..32].copy_from_slice(&record.transaction_id.as_bytes());
    payload[32..40].copy_from_slice(&record.orchard_value_balance_zat.to_be_bytes());
    payload[40..48].copy_from_slice(&record.ironwood_value_balance_zat.to_be_bytes());
    payload[48..80].copy_from_slice(&record.orchard_anchor);
    payload[80] = u8::from(record.conformant);
    payload
}

fn decode_pool_totals_value(
    height: u32,
    payload: &[u8],
) -> Result<MigrationPoolTotals, MaterializedViewStoreError> {
    if payload.len() != POOL_TOTALS_VALUE_LEN {
        return Err(malformed_row(
            IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY,
            payload.len(),
            POOL_TOTALS_VALUE_LEN,
        ));
    }
    Ok(MigrationPoolTotals {
        block_height: height,
        cumulative_orchard_value_balance_zat: read_i64(&payload[0..8]),
        cumulative_ironwood_value_balance_zat: read_i64(&payload[8..16]),
        block_orchard_value_balance_zat: read_i64(&payload[16..24]),
        block_ironwood_value_balance_zat: read_i64(&payload[24..32]),
    })
}

fn decode_migration(key: &[u8], payload: &[u8]) -> Result<Migration, MaterializedViewStoreError> {
    if key.len() != MIGRATION_KEY_LEN {
        return Err(malformed_row(
            IRONWOOD_MIGRATIONS_COLUMN_FAMILY,
            key.len(),
            MIGRATION_KEY_LEN,
        ));
    }
    if payload.len() != MIGRATION_VALUE_LEN {
        return Err(malformed_row(
            IRONWOOD_MIGRATIONS_COLUMN_FAMILY,
            payload.len(),
            MIGRATION_VALUE_LEN,
        ));
    }
    let block_height = decode_height_key(&key[..HEIGHT_KEY_LEN])?;
    let mut tx_index_bytes = [0u8; TX_INDEX_LEN];
    tx_index_bytes.copy_from_slice(&key[HEIGHT_KEY_LEN..]);
    let mut transaction_id_bytes = [0u8; TXID_LEN];
    transaction_id_bytes.copy_from_slice(&payload[0..32]);
    let mut orchard_anchor = [0u8; ANCHOR_LEN];
    orchard_anchor.copy_from_slice(&payload[48..80]);
    Ok(Migration {
        block_height,
        tx_index_in_block: u32::from_be_bytes(tx_index_bytes),
        transaction_id: TransactionId::from_bytes(transaction_id_bytes),
        orchard_value_balance_zat: read_i64(&payload[32..40]),
        ironwood_value_balance_zat: read_i64(&payload[40..48]),
        orchard_anchor,
        conformant: payload[80] != 0,
    })
}

fn decode_height_key(key: &[u8]) -> Result<u32, MaterializedViewStoreError> {
    zinder_core::wire::decode_height_key_ascending(key)
        .map(BlockHeight::value)
        .map_err(|error| MaterializedViewStoreError::Decode {
            column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
            reason: format!(
                "ironwood-migration key is not a {HEIGHT_KEY_LEN}-byte ascending height: {error}"
            ),
        })
}

fn read_i64(bytes: &[u8]) -> i64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(bytes);
    i64::from_be_bytes(array)
}

fn malformed_row(
    column_family: &str,
    actual: usize,
    expected: usize,
) -> MaterializedViewStoreError {
    MaterializedViewStoreError::Decode {
        column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
        reason: format!(
            "ironwood-migration column family `{column_family}` row is {actual} bytes, expected {expected}"
        ),
    }
}

/// Consumer-specific failure modes [`IronwoodMigrationConsumer`] can surface.
///
/// Infrastructure failures (store I/O, materialized-view writes) reach the SDK
/// through the boxed [`MaterializedViewConsumerError`] without going through this enum.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IronwoodMigrationConsumerError {
    /// Accumulating a block's value balances into the running total overflowed
    /// the signed 64-bit range.
    #[error("ironwood-migration running total overflowed i64 at height {height}")]
    RunningTotalOverflow {
        /// Height whose accumulation overflowed.
        height: u32,
    },
}
