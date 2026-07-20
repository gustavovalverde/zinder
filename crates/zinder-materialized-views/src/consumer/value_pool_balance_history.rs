//! Canonical chain value-pool balance snapshots by UTC calendar day.
//!
//! The primary table retains every block snapshot, ordered by UTC day and then
//! descending block height. A metadata table maps each day to its highest
//! canonical height and each height to its exact primary key. This keeps
//! product-facing reads bounded while preserving every live-tail candidate
//! needed to expose the prior same-day point after a reorg.

use std::collections::BTreeMap;

use rust_rocksdb::WriteBatch;
use zinder_core::wire::{
    decode_height_key_ascending, decode_height_key_descending, decode_internal_block_hash,
    encode_height_key_ascending, encode_height_key_descending, encode_internal_block_hash,
};
use zinder_core::{BlockHash, BlockHeight, ValuePoolBalance};

use crate::consumer::{
    BlockCommitContext, BlockKeyedConsumer, MaterializedViewConsumerCtx,
    MaterializedViewConsumerError, MaterializedViewConsumerName, MaterializedViewConsumerSchema,
};
use crate::{MaterializedViewStore, MaterializedViewStoreColumnFamily, MaterializedViewStoreError};

/// Every authoritative per-block value-pool snapshot.
pub const VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILY: &str = "value_pool_balance_history";
/// Per-height rewind keys, per-day winners, and independent coverage records.
pub const VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY: &str =
    "value_pool_balance_history_metadata";
/// Column families owned by this consumer.
pub const VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILIES: &[&str] = &[
    VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILY,
    VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
];
/// Stable consumer identity persisted in materialized-view metadata and cursor rows.
pub const VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME: MaterializedViewConsumerName =
    MaterializedViewConsumerName::from_static("value_pool_balance_history");
/// Initial consumer-local schema.
pub const VALUE_POOL_BALANCE_HISTORY_SCHEMA: MaterializedViewConsumerSchema =
    MaterializedViewConsumerSchema::new(
        VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME,
        1,
        VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILIES,
    );

const TIME_KEY_LEN: usize = size_of::<i64>();
const HEIGHT_KEY_LEN: usize = size_of::<u32>();
const BLOCK_HASH_LEN: usize = 32;
const PRIMARY_KEY_LEN: usize = TIME_KEY_LEN + HEIGHT_KEY_LEN + BLOCK_HASH_LEN;
const DAY_INDEX_PREFIX: u8 = b'd';
const HEIGHT_INDEX_PREFIX: u8 = b'h';
const BACKFILL_COVERAGE_KEY: &[u8] = b"backfill_v1";
const TAIL_COVERAGE_KEY: &[u8] = b"live_tail_v1";
const SNAPSHOT_VERSION: u8 = 1;
const SNAPSHOT_HEADER_LEN: usize = 1 + HEIGHT_KEY_LEN + BLOCK_HASH_LEN + TIME_KEY_LEN + 4;
const BACKFILL_COVERAGE_LEN: usize = 2 * HEIGHT_KEY_LEN;
const TAIL_COVERAGE_LEN: usize = 1 + 2 * HEIGHT_KEY_LEN;
const SECONDS_PER_DAY: i64 = 86_400;

/// One authoritative value-pool snapshot at a canonical block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValuePoolBalancePoint {
    /// Height of the canonical block that supplied the snapshot.
    pub block_height: BlockHeight,
    /// Hash of the canonical block that supplied the snapshot.
    pub block_hash: BlockHash,
    /// Block time as Unix seconds.
    pub block_time_unix_seconds: i64,
    /// Upstream list-shaped pool values, preserved in source order.
    pub pools: Vec<ValuePoolBalance>,
}

/// The canonical highest-height point for one UTC calendar day.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValuePoolBalanceDay {
    /// Inclusive Unix-second start of the UTC calendar day.
    pub day_start_unix_seconds: i64,
    /// Highest canonical block-height snapshot on that day.
    pub point: ValuePoolBalancePoint,
}

/// Durable contiguous historical range materialized by backfill.
///
/// Coverage is intentionally height-only: block timestamps do not establish
/// canonical ordering or completeness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValuePoolBalanceBackfillCoverage {
    /// First completely materialized height.
    pub complete_from_height: BlockHeight,
    /// Last completely materialized height.
    pub complete_through_height: BlockHeight,
}

impl ValuePoolBalanceBackfillCoverage {
    /// Creates a contiguous historical coverage record.
    #[must_use]
    pub const fn new(
        complete_from_height: BlockHeight,
        complete_through_height: BlockHeight,
    ) -> Self {
        Self {
            complete_from_height,
            complete_through_height,
        }
    }
}

/// Durable contiguous live-tail interval.
///
/// Like historical coverage, this record is based only on canonical heights.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValuePoolBalanceTailCoverage {
    /// First height owned by the live tail.
    pub boundary_height: BlockHeight,
    /// Last contiguous live-tail height, absent before the first block.
    pub complete_through_height: Option<BlockHeight>,
}

impl ValuePoolBalanceTailCoverage {
    /// Creates an empty live tail at `boundary_height`.
    #[must_use]
    pub const fn from_boundary(boundary_height: BlockHeight) -> Self {
        Self {
            boundary_height,
            complete_through_height: None,
        }
    }
}

/// Materializes canonical chain value-pool snapshots and daily winners.
pub struct ValuePoolBalanceHistoryConsumer {
    pending_height_keys: BTreeMap<BlockHeight, Option<[u8; PRIMARY_KEY_LEN]>>,
    pending_primary_rows: BTreeMap<[u8; PRIMARY_KEY_LEN], Option<()>>,
    track_live_tail: bool,
}

impl Default for ValuePoolBalanceHistoryConsumer {
    fn default() -> Self {
        Self::new()
    }
}

impl ValuePoolBalanceHistoryConsumer {
    /// Builds an empty consumer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending_height_keys: BTreeMap::new(),
            pending_primary_rows: BTreeMap::new(),
            track_live_tail: true,
        }
    }

    /// Reads durable historical coverage, when backfill has started.
    pub fn backfill_coverage(
        store: &MaterializedViewStore,
    ) -> Result<Option<ValuePoolBalanceBackfillCoverage>, MaterializedViewStoreError> {
        store
            .get_consumer(
                VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
                BACKFILL_COVERAGE_KEY,
            )?
            .map(|bytes| {
                decode_backfill_coverage(&bytes).map_err(|error| store_decode_error(&error))
            })
            .transpose()
    }

    /// Reads the seeded live-tail boundary and contiguous endpoint.
    pub fn tail_coverage(
        store: &MaterializedViewStore,
    ) -> Result<Option<ValuePoolBalanceTailCoverage>, MaterializedViewStoreError> {
        store
            .get_consumer(
                VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
                TAIL_COVERAGE_KEY,
            )?
            .map(|bytes| decode_tail_coverage(&bytes).map_err(|error| store_decode_error(&error)))
            .transpose()
    }

    /// Initializes or widens the startup tail without deleting snapshot rows.
    pub fn widen_tail_boundary_for_startup(
        store: &MaterializedViewStore,
        boundary_height: BlockHeight,
    ) -> Result<bool, ValuePoolBalanceHistoryConsumerError> {
        if Self::tail_coverage(store)?.is_some_and(|tail| boundary_height >= tail.boundary_height) {
            return Ok(false);
        }
        store.put_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            TAIL_COVERAGE_KEY,
            &encode_tail_coverage(ValuePoolBalanceTailCoverage::from_boundary(boundary_height)),
        )?;
        Ok(true)
    }

    /// Moves the live-tail boundary while preserving already materialized rows.
    ///
    /// Advancing the boundary transfers older rows to historical ownership;
    /// lowering it prepares a wider replaceable interval. Snapshot rows are
    /// retained because they continue to serve daily history reads.
    pub fn move_tail_boundary_for_sync(
        store: &MaterializedViewStore,
        boundary_height: BlockHeight,
    ) -> Result<bool, ValuePoolBalanceHistoryConsumerError> {
        let existing = Self::tail_coverage(store)?;
        if existing.is_some_and(|tail| tail.boundary_height == boundary_height) {
            return Ok(false);
        }
        let complete_through_height = existing
            .and_then(|tail| tail.complete_through_height)
            .filter(|through| *through >= boundary_height);
        store.put_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            TAIL_COVERAGE_KEY,
            &encode_tail_coverage(ValuePoolBalanceTailCoverage {
                boundary_height,
                complete_through_height,
            }),
        )?;
        Ok(true)
    }

    /// Atomically reverts stale live-tail heights and appends replacements.
    pub fn reconcile_tail(
        &mut self,
        store: &MaterializedViewStore,
        reverted_heights_descending: &[BlockHeight],
        replacement_blocks: &[BlockCommitContext],
    ) -> Result<(), MaterializedViewConsumerError> {
        if reverted_heights_descending
            .windows(2)
            .any(|pair| pair[0] <= pair[1])
            || replacement_blocks
                .windows(2)
                .any(|pair| pair[0].height >= pair[1].height)
        {
            return Err(Box::new(
                ValuePoolBalanceHistoryConsumerError::CoverageDiscontinuous,
            ));
        }
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store,
            batch: &mut batch,
        };
        self.begin_batch(&mut ctx)?;
        for height in reverted_heights_descending {
            self.revert_block(*height, &mut ctx)?;
        }
        for block in replacement_blocks {
            self.apply_block(block, &mut ctx)?;
        }
        self.finish_batch(&mut ctx)?;
        store.write_consumer_batch(VALUE_POOL_BALANCE_HISTORY_SCHEMA.name, ctx.batch)?;
        Ok(())
    }

    /// Atomically writes sparse daily candidates from one contiguous historical scan.
    ///
    /// `blocks` contains the highest-height candidate observed for each UTC
    /// day in the newly covered height interval. Coverage describes every
    /// canonical height the caller scanned, including heights that do not
    /// need their own persisted snapshot.
    pub fn write_backfill_batch(
        &mut self,
        store: &MaterializedViewStore,
        blocks: &[BlockCommitContext],
        next_coverage: ValuePoolBalanceBackfillCoverage,
    ) -> Result<(), MaterializedViewConsumerError> {
        validate_backfill_batch(store, blocks, next_coverage)?;
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store,
            batch: &mut batch,
        };
        self.begin_batch(&mut ctx)?;
        self.track_live_tail = false;
        for block in blocks {
            self.apply_block(block, &mut ctx)?;
        }
        self.finish_batch(&mut ctx)?;
        let metadata_cf =
            store.consumer_column_family(VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY)?;
        ctx.batch.put_cf(
            &metadata_cf,
            BACKFILL_COVERAGE_KEY,
            encode_backfill_coverage(next_coverage),
        );
        store.write_consumer_batch(VALUE_POOL_BALANCE_HISTORY_SCHEMA.name, ctx.batch)?;
        Ok(())
    }

    /// Atomically seeds already-visible live-tail blocks without moving a cursor.
    pub fn write_tail_seed_batch(
        &mut self,
        store: &MaterializedViewStore,
        blocks: &[BlockCommitContext],
    ) -> Result<(), MaterializedViewConsumerError> {
        validate_tail_seed_batch(store, blocks)?;
        let mut batch = WriteBatch::default();
        let mut ctx = MaterializedViewConsumerCtx {
            store,
            batch: &mut batch,
        };
        self.begin_batch(&mut ctx)?;
        for block in blocks {
            self.apply_block(block, &mut ctx)?;
        }
        self.finish_batch(&mut ctx)?;
        store.write_consumer_batch(VALUE_POOL_BALANCE_HISTORY_SCHEMA.name, ctx.batch)?;
        Ok(())
    }

    /// Reads at most `day_cap` daily winners in newest UTC-day-first order.
    pub fn read_newest_days(
        store: &MaterializedViewStore,
        day_cap: usize,
    ) -> Result<Vec<ValuePoolBalanceDay>, ValuePoolBalanceHistoryConsumerError> {
        Self::read_days_before(store, None, day_cap)
    }

    /// Reads daily winners strictly older than an optional UTC-day cursor.
    pub fn read_days_before(
        store: &MaterializedViewStore,
        before_day_start_unix_seconds: Option<i64>,
        day_cap: usize,
    ) -> Result<Vec<ValuePoolBalanceDay>, ValuePoolBalanceHistoryConsumerError> {
        if day_cap == 0 {
            return Ok(Vec::new());
        }
        if let Some(day) = before_day_start_unix_seconds
            && utc_day_start(day) != day
        {
            return Err(ValuePoolBalanceHistoryConsumerError::InvalidDayStart {
                day_start_unix_seconds: day,
            });
        }
        let start =
            before_day_start_unix_seconds.map_or_else(day_index_range_start, encode_day_index_key);
        let rows = store.range_iterate_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            &start,
            &day_index_range_end(),
            day_cap.saturating_add(1),
        )?;
        rows.into_iter()
            .filter(|(key, _)| key.as_slice() != start)
            .take(day_cap)
            .map(|(key, metadata_value)| {
                let day = decode_day_index_key(&key)?;
                let primary_key = decode_primary_key_reference(&metadata_value)?;
                if decode_primary_key(&primary_key)?.0 != day {
                    return Err(ValuePoolBalanceHistoryConsumerError::DayIndexMismatch);
                }
                let point = read_point(store, &primary_key)?;
                Ok(ValuePoolBalanceDay {
                    day_start_unix_seconds: day,
                    point,
                })
            })
            .collect()
    }

    /// Reads the highest canonical height snapshot for exactly one UTC day.
    pub fn point_for_utc_day(
        store: &MaterializedViewStore,
        day_start_unix_seconds: i64,
    ) -> Result<Option<ValuePoolBalancePoint>, ValuePoolBalanceHistoryConsumerError> {
        if utc_day_start(day_start_unix_seconds) != day_start_unix_seconds {
            return Err(ValuePoolBalanceHistoryConsumerError::InvalidDayStart {
                day_start_unix_seconds,
            });
        }
        let Some(metadata_value) = store.get_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            &encode_day_index_key(day_start_unix_seconds),
        )?
        else {
            return Ok(None);
        };
        let primary_key = decode_primary_key_reference(&metadata_value)?;
        if decode_primary_key(&primary_key)?.0 != day_start_unix_seconds {
            return Err(ValuePoolBalanceHistoryConsumerError::DayIndexMismatch);
        }
        read_point(store, &primary_key).map(Some)
    }

    /// Reads the exact canonical snapshot retained for one block height.
    pub fn point_at_height(
        store: &MaterializedViewStore,
        height: BlockHeight,
    ) -> Result<Option<ValuePoolBalancePoint>, ValuePoolBalanceHistoryConsumerError> {
        let Some(metadata_value) = store.get_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            &encode_height_index_key(height),
        )?
        else {
            return Ok(None);
        };
        let primary_key = decode_primary_key_reference(&metadata_value)?;
        if decode_primary_key(&primary_key)?.1 != height {
            return Err(ValuePoolBalanceHistoryConsumerError::HeightIndexMismatch {
                requested_height: height.value(),
            });
        }
        read_point(store, &primary_key).map(Some)
    }

    /// Looks up exact UTC calendar-day points before the day containing `end_time`.
    ///
    /// Callers can pass `[1, 7, 30]` to build fixed-period deltas without
    /// treating timestamps as a canonical ordering signal.
    pub fn points_days_before(
        store: &MaterializedViewStore,
        end_time_unix_seconds: i64,
        day_offsets: &[u32],
    ) -> Result<Vec<Option<ValuePoolBalancePoint>>, ValuePoolBalanceHistoryConsumerError> {
        let end_day = utc_day_start(end_time_unix_seconds);
        day_offsets
            .iter()
            .map(|offset| {
                let seconds = i64::from(*offset)
                    .checked_mul(SECONDS_PER_DAY)
                    .ok_or(ValuePoolBalanceHistoryConsumerError::DayOffsetOverflow)?;
                let day = end_day
                    .checked_sub(seconds)
                    .ok_or(ValuePoolBalanceHistoryConsumerError::DayOffsetOverflow)?;
                Self::point_for_utc_day(store, day)
            })
            .collect()
    }

    /// Decodes one stored snapshot after validating it against its primary key.
    pub fn decode_point(
        key: &[u8],
        payload: &[u8],
    ) -> Result<ValuePoolBalancePoint, ValuePoolBalanceHistoryConsumerError> {
        let (day, height, hash) = decode_primary_key(key)?;
        let point = decode_snapshot(payload)?;
        if point.block_height != height
            || point.block_hash != hash
            || utc_day_start(point.block_time_unix_seconds) != day
        {
            return Err(ValuePoolBalanceHistoryConsumerError::PrimaryKeyMismatch);
        }
        Ok(point)
    }

    fn refresh_day_index(
        &self,
        day_start_unix_seconds: i64,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let metadata_cf = ctx
            .store
            .consumer_column_family(VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY)?;
        let day_key = encode_day_index_key(day_start_unix_seconds);
        match self.primary_key_for_day_after_batch(day_start_unix_seconds, ctx.store)? {
            Some(primary_key) => ctx.batch.put_cf(&metadata_cf, day_key, primary_key),
            None => ctx.batch.delete_cf(&metadata_cf, day_key),
        }
        Ok(())
    }

    fn primary_key_for_day_after_batch(
        &self,
        day_start_unix_seconds: i64,
        store: &MaterializedViewStore,
    ) -> Result<Option<[u8; PRIMARY_KEY_LEN]>, ValuePoolBalanceHistoryConsumerError> {
        let (start, end) = primary_key_range_for_day(day_start_unix_seconds);
        let mut winner = store
            .range_iterate_consumer(
                VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILY,
                &start,
                &end,
                usize::MAX,
            )?
            .into_iter()
            .filter_map(|(key, _)| {
                let key: [u8; PRIMARY_KEY_LEN] = key.try_into().ok()?;
                self.pending_primary_rows
                    .get(&key)
                    .is_none_or(Option::is_some)
                    .then_some(key)
            })
            .min();
        for (key, present) in &self.pending_primary_rows {
            if present.is_some() && decode_primary_key(key)?.0 == day_start_unix_seconds {
                winner = Some(winner.map_or(*key, |current| current.min(*key)));
            }
        }
        Ok(winner)
    }

    fn height_primary_key(
        &self,
        height: BlockHeight,
        store: &MaterializedViewStore,
    ) -> Result<Option<[u8; PRIMARY_KEY_LEN]>, ValuePoolBalanceHistoryConsumerError> {
        if let Some(key) = self.pending_height_keys.get(&height) {
            return Ok(*key);
        }
        let Some(metadata_value) = store.get_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            &encode_height_index_key(height),
        )?
        else {
            return Ok(None);
        };
        let primary_key = decode_primary_key_reference(&metadata_value)?;
        if decode_primary_key(&primary_key)?.1 != height {
            return Err(ValuePoolBalanceHistoryConsumerError::HeightIndexMismatch {
                requested_height: height.value(),
            });
        }
        Ok(Some(primary_key))
    }
}

impl BlockKeyedConsumer for ValuePoolBalanceHistoryConsumer {
    fn name(&self) -> MaterializedViewConsumerName {
        VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME
    }

    fn begin_batch(
        &mut self,
        _ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        self.pending_height_keys.clear();
        self.pending_primary_rows.clear();
        Ok(())
    }

    fn apply_block(
        &mut self,
        block: &BlockCommitContext,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        if self.height_primary_key(block.height, ctx.store)?.is_some() {
            return Err(Box::new(
                ValuePoolBalanceHistoryConsumerError::DuplicateHeight {
                    height: block.height.value(),
                },
            ));
        }
        let pools = block.block_value_pool_balances().ok_or_else(|| {
            Box::new(ValuePoolBalanceHistoryConsumerError::MissingValuePools {
                height: block.height.value(),
            }) as MaterializedViewConsumerError
        })?;
        let point = ValuePoolBalancePoint {
            block_height: block.height,
            block_hash: block.block_hash,
            block_time_unix_seconds: block.block_time_unix_seconds,
            pools: pools.as_ref().clone(),
        };
        let primary_key = encode_primary_key(&point);
        let snapshot = encode_snapshot(&point)?;
        let primary_cf = ctx
            .store
            .consumer_column_family(VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILY)?;
        let metadata_cf = ctx
            .store
            .consumer_column_family(VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY)?;
        ctx.batch.put_cf(&primary_cf, primary_key, snapshot);
        ctx.batch.put_cf(
            &metadata_cf,
            encode_height_index_key(block.height),
            primary_key,
        );
        self.pending_primary_rows.insert(primary_key, Some(()));
        self.pending_height_keys
            .insert(block.height, Some(primary_key));
        self.refresh_day_index(utc_day_start(block.block_time_unix_seconds), ctx)?;
        Ok(())
    }

    fn revert_block(
        &mut self,
        height: BlockHeight,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        let Some(primary_key) = self.height_primary_key(height, ctx.store)? else {
            return Ok(());
        };
        let (day, indexed_height, _) = decode_primary_key(&primary_key)?;
        if indexed_height != height {
            return Err(Box::new(
                ValuePoolBalanceHistoryConsumerError::HeightIndexMismatch {
                    requested_height: height.value(),
                },
            ));
        }
        let primary_cf = ctx
            .store
            .consumer_column_family(VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILY)?;
        let metadata_cf = ctx
            .store
            .consumer_column_family(VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY)?;
        ctx.batch.delete_cf(&primary_cf, primary_key);
        ctx.batch
            .delete_cf(&metadata_cf, encode_height_index_key(height));
        self.pending_primary_rows.insert(primary_key, None);
        self.pending_height_keys.insert(height, None);
        self.refresh_day_index(day, ctx)?;
        Ok(())
    }

    fn finish_batch(
        &mut self,
        ctx: &mut MaterializedViewConsumerCtx<'_>,
    ) -> Result<(), MaterializedViewConsumerError> {
        if self.track_live_tail {
            stage_tail_coverage(ctx, &self.pending_height_keys)?;
        }
        self.pending_height_keys.clear();
        self.pending_primary_rows.clear();
        self.track_live_tail = true;
        Ok(())
    }
}

fn read_point(
    store: &MaterializedViewStore,
    primary_key: &[u8; PRIMARY_KEY_LEN],
) -> Result<ValuePoolBalancePoint, ValuePoolBalanceHistoryConsumerError> {
    let Some(payload) =
        store.get_consumer(VALUE_POOL_BALANCE_HISTORY_COLUMN_FAMILY, primary_key)?
    else {
        return Err(ValuePoolBalanceHistoryConsumerError::MissingPrimaryRow);
    };
    ValuePoolBalanceHistoryConsumer::decode_point(primary_key, &payload)
}

fn validate_backfill_batch(
    store: &MaterializedViewStore,
    blocks: &[BlockCommitContext],
    next: ValuePoolBalanceBackfillCoverage,
) -> Result<(), MaterializedViewConsumerError> {
    let existing = ValuePoolBalanceHistoryConsumer::backfill_coverage(store)?;
    let scan_from = existing.map_or(next.complete_from_height, |coverage| {
        coverage
            .complete_through_height
            .next()
            .unwrap_or(BlockHeight::new(u32::MAX))
    });
    let starts_contiguously = existing.is_none_or(|coverage| {
        coverage.complete_from_height == next.complete_from_height
            && scan_from <= next.complete_through_height
    });
    if starts_contiguously
        && blocks
            .windows(2)
            .all(|pair| pair[0].height < pair[1].height)
        && blocks
            .iter()
            .all(|block| block.height >= scan_from && block.height <= next.complete_through_height)
    {
        Ok(())
    } else {
        Err(Box::new(
            ValuePoolBalanceHistoryConsumerError::CoverageDiscontinuous,
        ))
    }
}

fn validate_tail_seed_batch(
    store: &MaterializedViewStore,
    blocks: &[BlockCommitContext],
) -> Result<(), MaterializedViewConsumerError> {
    let Some(first) = blocks.first() else {
        return Err(Box::new(
            ValuePoolBalanceHistoryConsumerError::EmptyBackfill,
        ));
    };
    let tail = ValuePoolBalanceHistoryConsumer::tail_coverage(store)?.ok_or_else(|| {
        Box::new(ValuePoolBalanceHistoryConsumerError::CoverageDiscontinuous)
            as MaterializedViewConsumerError
    })?;
    let expected = tail
        .complete_through_height
        .map_or(Some(tail.boundary_height), BlockHeight::next);
    if expected == Some(first.height)
        && blocks
            .windows(2)
            .all(|pair| pair[0].height.next() == Some(pair[1].height))
    {
        Ok(())
    } else {
        Err(Box::new(
            ValuePoolBalanceHistoryConsumerError::CoverageDiscontinuous,
        ))
    }
}

fn stage_tail_coverage(
    ctx: &mut MaterializedViewConsumerCtx<'_>,
    pending: &BTreeMap<BlockHeight, Option<[u8; PRIMARY_KEY_LEN]>>,
) -> Result<(), MaterializedViewConsumerError> {
    let mut tail = if let Some(tail) = ValuePoolBalanceHistoryConsumer::tail_coverage(ctx.store)? {
        tail
    } else {
        let Some(boundary_height) = pending
            .iter()
            .find_map(|(height, key)| key.map(|_| *height))
        else {
            return Ok(());
        };
        ValuePoolBalanceTailCoverage::from_boundary(boundary_height)
    };
    while let Some(through) = tail.complete_through_height {
        if height_present_after_batch(ctx.store, pending, through)? {
            break;
        }
        if through <= tail.boundary_height {
            tail.complete_through_height = None;
            break;
        }
        tail.complete_through_height = Some(BlockHeight::new(through.value() - 1));
    }
    while let Some(candidate) = tail
        .complete_through_height
        .map_or(Some(tail.boundary_height), BlockHeight::next)
    {
        if !height_present_after_batch(ctx.store, pending, candidate)? {
            break;
        }
        tail.complete_through_height = Some(candidate);
    }
    let metadata_cf = ctx
        .store
        .consumer_column_family(VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY)?;
    ctx.batch
        .put_cf(&metadata_cf, TAIL_COVERAGE_KEY, encode_tail_coverage(tail));
    Ok(())
}

fn height_present_after_batch(
    store: &MaterializedViewStore,
    pending: &BTreeMap<BlockHeight, Option<[u8; PRIMARY_KEY_LEN]>>,
    height: BlockHeight,
) -> Result<bool, ValuePoolBalanceHistoryConsumerError> {
    if let Some(key) = pending.get(&height) {
        return Ok(key.is_some());
    }
    Ok(store
        .get_consumer(
            VALUE_POOL_BALANCE_HISTORY_METADATA_COLUMN_FAMILY,
            &encode_height_index_key(height),
        )?
        .is_some())
}

fn utc_day_start(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY
}

fn encode_primary_key(point: &ValuePoolBalancePoint) -> [u8; PRIMARY_KEY_LEN] {
    let mut key = [0_u8; PRIMARY_KEY_LEN];
    key[..TIME_KEY_LEN].copy_from_slice(&encode_time_descending(utc_day_start(
        point.block_time_unix_seconds,
    )));
    key[TIME_KEY_LEN..TIME_KEY_LEN + HEIGHT_KEY_LEN]
        .copy_from_slice(&encode_height_key_descending(point.block_height));
    key[TIME_KEY_LEN + HEIGHT_KEY_LEN..]
        .copy_from_slice(&encode_internal_block_hash(point.block_hash));
    key
}

fn decode_primary_key(
    key: &[u8],
) -> Result<(i64, BlockHeight, BlockHash), ValuePoolBalanceHistoryConsumerError> {
    if key.len() != PRIMARY_KEY_LEN {
        return Err(ValuePoolBalanceHistoryConsumerError::MalformedPrimaryKey { bytes: key.len() });
    }
    let day = decode_time_descending(&key[..TIME_KEY_LEN])?;
    let height = decode_height_key_descending(&key[TIME_KEY_LEN..TIME_KEY_LEN + HEIGHT_KEY_LEN])
        .map_err(
            |_| ValuePoolBalanceHistoryConsumerError::MalformedPrimaryKey { bytes: key.len() },
        )?;
    let hash = decode_internal_block_hash(&key[TIME_KEY_LEN + HEIGHT_KEY_LEN..]).map_err(|_| {
        ValuePoolBalanceHistoryConsumerError::MalformedPrimaryKey { bytes: key.len() }
    })?;
    Ok((day, height, hash))
}

fn primary_key_range_for_day(
    day_start_unix_seconds: i64,
) -> ([u8; PRIMARY_KEY_LEN], [u8; PRIMARY_KEY_LEN]) {
    let mut start = [0_u8; PRIMARY_KEY_LEN];
    let mut end = [u8::MAX; PRIMARY_KEY_LEN];
    let day = encode_time_descending(day_start_unix_seconds);
    start[..TIME_KEY_LEN].copy_from_slice(&day);
    end[..TIME_KEY_LEN].copy_from_slice(&day);
    (start, end)
}

fn encode_day_index_key(day_start_unix_seconds: i64) -> [u8; 1 + TIME_KEY_LEN] {
    let mut key = [0_u8; 1 + TIME_KEY_LEN];
    key[0] = DAY_INDEX_PREFIX;
    key[1..].copy_from_slice(&encode_time_descending(day_start_unix_seconds));
    key
}

fn decode_day_index_key(key: &[u8]) -> Result<i64, ValuePoolBalanceHistoryConsumerError> {
    if key.len() != 1 + TIME_KEY_LEN || key[0] != DAY_INDEX_PREFIX {
        return Err(ValuePoolBalanceHistoryConsumerError::MalformedMetadata { bytes: key.len() });
    }
    decode_time_descending(&key[1..])
}

fn day_index_range_start() -> [u8; 1 + TIME_KEY_LEN] {
    [DAY_INDEX_PREFIX, 0, 0, 0, 0, 0, 0, 0, 0]
}

fn day_index_range_end() -> [u8; 1 + TIME_KEY_LEN] {
    [
        DAY_INDEX_PREFIX,
        u8::MAX,
        u8::MAX,
        u8::MAX,
        u8::MAX,
        u8::MAX,
        u8::MAX,
        u8::MAX,
        u8::MAX,
    ]
}

fn encode_height_index_key(height: BlockHeight) -> [u8; 1 + HEIGHT_KEY_LEN] {
    let mut key = [0_u8; 1 + HEIGHT_KEY_LEN];
    key[0] = HEIGHT_INDEX_PREFIX;
    key[1..].copy_from_slice(&encode_height_key_ascending(height));
    key
}

fn decode_primary_key_reference(
    metadata_value: &[u8],
) -> Result<[u8; PRIMARY_KEY_LEN], ValuePoolBalanceHistoryConsumerError> {
    metadata_value.try_into().map_err(
        |_| ValuePoolBalanceHistoryConsumerError::MalformedMetadata {
            bytes: metadata_value.len(),
        },
    )
}

fn encode_snapshot(
    point: &ValuePoolBalancePoint,
) -> Result<Vec<u8>, ValuePoolBalanceHistoryConsumerError> {
    let pool_count = u32::try_from(point.pools.len())
        .map_err(|_| ValuePoolBalanceHistoryConsumerError::TooManyPools)?;
    let mut bytes = Vec::with_capacity(SNAPSHOT_HEADER_LEN + point.pools.len() * 24);
    bytes.push(SNAPSHOT_VERSION);
    bytes.extend_from_slice(&encode_height_key_ascending(point.block_height));
    bytes.extend_from_slice(&encode_internal_block_hash(point.block_hash));
    bytes.extend_from_slice(&point.block_time_unix_seconds.to_be_bytes());
    bytes.extend_from_slice(&pool_count.to_be_bytes());
    for pool in &point.pools {
        let id = pool.id.as_bytes();
        let id_len = u32::try_from(id.len())
            .map_err(|_| ValuePoolBalanceHistoryConsumerError::PoolIdTooLong)?;
        bytes.extend_from_slice(&id_len.to_be_bytes());
        bytes.extend_from_slice(id);
        bytes.push(u8::from(pool.monitored));
        match pool.value_zat {
            Some(pool_value_zat) => {
                bytes.push(1);
                bytes.extend_from_slice(&pool_value_zat.to_be_bytes());
            }
            None => bytes.push(0),
        }
    }
    Ok(bytes)
}

#[allow(
    clippy::too_many_lines,
    reason = "the cursor validates one compact variable-length snapshot atomically"
)]
fn decode_snapshot(
    payload: &[u8],
) -> Result<ValuePoolBalancePoint, ValuePoolBalanceHistoryConsumerError> {
    if payload.len() < SNAPSHOT_HEADER_LEN || payload[0] != SNAPSHOT_VERSION {
        return Err(ValuePoolBalanceHistoryConsumerError::MalformedSnapshot {
            bytes: payload.len(),
        });
    }
    let malformed = || ValuePoolBalanceHistoryConsumerError::MalformedSnapshot {
        bytes: payload.len(),
    };
    let mut offset = 1;
    let height = decode_height_key_ascending(&payload[offset..offset + HEIGHT_KEY_LEN])
        .map_err(|_| malformed())?;
    offset += HEIGHT_KEY_LEN;
    let hash = decode_internal_block_hash(&payload[offset..offset + BLOCK_HASH_LEN])
        .map_err(|_| malformed())?;
    offset += BLOCK_HASH_LEN;
    let block_time_unix_seconds = i64::from_be_bytes(
        payload[offset..offset + TIME_KEY_LEN]
            .try_into()
            .map_err(|_| malformed())?,
    );
    offset += TIME_KEY_LEN;
    let pool_count = u32::from_be_bytes(
        payload[offset..offset + 4]
            .try_into()
            .map_err(|_| malformed())?,
    ) as usize;
    offset += 4;
    let mut pools = Vec::with_capacity(pool_count);
    for _ in 0..pool_count {
        let id_len = u32::from_be_bytes(
            payload
                .get(offset..offset + 4)
                .ok_or_else(malformed)?
                .try_into()
                .map_err(|_| malformed())?,
        ) as usize;
        offset = offset.checked_add(4).ok_or_else(malformed)?;
        let id = String::from_utf8(
            payload
                .get(offset..offset + id_len)
                .ok_or_else(malformed)?
                .to_vec(),
        )
        .map_err(|_| malformed())?;
        offset = offset.checked_add(id_len).ok_or_else(malformed)?;
        let monitored = match *payload.get(offset).ok_or_else(malformed)? {
            0 => false,
            1 => true,
            _ => return Err(malformed()),
        };
        offset = offset.checked_add(1).ok_or_else(malformed)?;
        let value_zat = match *payload.get(offset).ok_or_else(malformed)? {
            0 => None,
            1 => {
                offset = offset.checked_add(1).ok_or_else(malformed)?;
                let decoded_value_zat = u64::from_be_bytes(
                    payload
                        .get(offset..offset + size_of::<i64>())
                        .ok_or_else(malformed)?
                        .try_into()
                        .map_err(|_| malformed())?,
                );
                offset = offset.checked_add(size_of::<i64>()).ok_or_else(malformed)?;
                Some(decoded_value_zat)
            }
            _ => return Err(malformed()),
        };
        if value_zat.is_none() {
            offset = offset.checked_add(1).ok_or_else(malformed)?;
        }
        pools.push(ValuePoolBalance::new(id, monitored, value_zat));
    }
    if offset != payload.len() {
        return Err(malformed());
    }
    Ok(ValuePoolBalancePoint {
        block_height: height,
        block_hash: hash,
        block_time_unix_seconds,
        pools,
    })
}

fn encode_backfill_coverage(
    coverage: ValuePoolBalanceBackfillCoverage,
) -> [u8; BACKFILL_COVERAGE_LEN] {
    let mut bytes = [0_u8; BACKFILL_COVERAGE_LEN];
    bytes[..HEIGHT_KEY_LEN]
        .copy_from_slice(&encode_height_key_ascending(coverage.complete_from_height));
    bytes[HEIGHT_KEY_LEN..].copy_from_slice(&encode_height_key_ascending(
        coverage.complete_through_height,
    ));
    bytes
}

fn decode_backfill_coverage(
    bytes: &[u8],
) -> Result<ValuePoolBalanceBackfillCoverage, ValuePoolBalanceHistoryConsumerError> {
    if bytes.len() != BACKFILL_COVERAGE_LEN {
        return Err(ValuePoolBalanceHistoryConsumerError::MalformedMetadata { bytes: bytes.len() });
    }
    let malformed =
        || ValuePoolBalanceHistoryConsumerError::MalformedMetadata { bytes: bytes.len() };
    let from = decode_height_key_ascending(&bytes[..HEIGHT_KEY_LEN]).map_err(|_| malformed())?;
    let through = decode_height_key_ascending(&bytes[HEIGHT_KEY_LEN..]).map_err(|_| malformed())?;
    if from > through {
        return Err(ValuePoolBalanceHistoryConsumerError::CoverageDiscontinuous);
    }
    Ok(ValuePoolBalanceBackfillCoverage::new(from, through))
}

fn encode_tail_coverage(coverage: ValuePoolBalanceTailCoverage) -> [u8; TAIL_COVERAGE_LEN] {
    let mut bytes = [0_u8; TAIL_COVERAGE_LEN];
    bytes[1..=HEIGHT_KEY_LEN]
        .copy_from_slice(&encode_height_key_ascending(coverage.boundary_height));
    if let Some(through) = coverage.complete_through_height {
        bytes[0] = 1;
        bytes[1 + HEIGHT_KEY_LEN..].copy_from_slice(&encode_height_key_ascending(through));
    }
    bytes
}

fn decode_tail_coverage(
    bytes: &[u8],
) -> Result<ValuePoolBalanceTailCoverage, ValuePoolBalanceHistoryConsumerError> {
    if bytes.len() != TAIL_COVERAGE_LEN {
        return Err(ValuePoolBalanceHistoryConsumerError::MalformedMetadata { bytes: bytes.len() });
    }
    let malformed =
        || ValuePoolBalanceHistoryConsumerError::MalformedMetadata { bytes: bytes.len() };
    let boundary =
        decode_height_key_ascending(&bytes[1..=HEIGHT_KEY_LEN]).map_err(|_| malformed())?;
    match bytes[0] {
        0 if bytes[1 + HEIGHT_KEY_LEN..].iter().all(|byte| *byte == 0) => {
            Ok(ValuePoolBalanceTailCoverage::from_boundary(boundary))
        }
        1 => {
            let through = decode_height_key_ascending(&bytes[1 + HEIGHT_KEY_LEN..])
                .map_err(|_| malformed())?;
            if through < boundary {
                return Err(ValuePoolBalanceHistoryConsumerError::CoverageDiscontinuous);
            }
            Ok(ValuePoolBalanceTailCoverage {
                boundary_height: boundary,
                complete_through_height: Some(through),
            })
        }
        _ => Err(malformed()),
    }
}

fn encode_time_descending(unix_seconds: i64) -> [u8; TIME_KEY_LEN] {
    (!(unix_seconds.cast_unsigned() ^ (1_u64 << 63))).to_be_bytes()
}

fn decode_time_descending(key: &[u8]) -> Result<i64, ValuePoolBalanceHistoryConsumerError> {
    let bytes: [u8; TIME_KEY_LEN] =
        key.try_into().map_err(
            |_| ValuePoolBalanceHistoryConsumerError::MalformedPrimaryKey { bytes: key.len() },
        )?;
    Ok(((!u64::from_be_bytes(bytes)) ^ (1_u64 << 63)).cast_signed())
}

fn store_decode_error(error: &ValuePoolBalanceHistoryConsumerError) -> MaterializedViewStoreError {
    MaterializedViewStoreError::Decode {
        column_family: MaterializedViewStoreColumnFamily::ConsumerMetadata,
        reason: error.to_string(),
    }
}

/// Consumer-specific failure modes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValuePoolBalanceHistoryConsumerError {
    /// Materialized-view store operation failed.
    #[error(transparent)]
    Store(#[from] MaterializedViewStoreError),
    /// The block did not carry an authoritative chain value-pool snapshot.
    #[error("block {height} is missing chain value-pool facts")]
    MissingValuePools {
        /// Block height missing the required input.
        height: u32,
    },
    /// A block height was applied twice without an intervening revert.
    #[error("value-pool balance history already contains height {height}")]
    DuplicateHeight {
        /// Duplicate canonical height.
        height: u32,
    },
    /// A primary key does not have the expected schema length.
    #[error("value-pool balance primary key has invalid length {bytes}")]
    MalformedPrimaryKey {
        /// Observed byte length.
        bytes: usize,
    },
    /// A snapshot payload is not valid schema-v1 data.
    #[error("value-pool balance snapshot has invalid encoding ({bytes} bytes)")]
    MalformedSnapshot {
        /// Observed byte length.
        bytes: usize,
    },
    /// A metadata key or value is malformed.
    #[error("value-pool balance metadata has invalid encoding ({bytes} bytes)")]
    MalformedMetadata {
        /// Observed byte length.
        bytes: usize,
    },
    /// A primary key and its encoded snapshot disagree.
    #[error("value-pool balance primary key does not match its snapshot")]
    PrimaryKeyMismatch,
    /// A height lookup references a key for another height.
    #[error("value-pool balance height index does not match requested height {requested_height}")]
    HeightIndexMismatch {
        /// Requested height.
        requested_height: u32,
    },
    /// A daily-winner index references a key for another day.
    #[error("value-pool balance day index does not match its primary key")]
    DayIndexMismatch,
    /// A daily-winner index referenced a missing primary row.
    #[error("value-pool balance metadata references a missing primary row")]
    MissingPrimaryRow,
    /// A requested day was not a UTC day boundary.
    #[error("{day_start_unix_seconds} is not a UTC calendar-day start")]
    InvalidDayStart {
        /// Requested Unix second.
        day_start_unix_seconds: i64,
    },
    /// A calendar-day offset cannot be represented in Unix seconds.
    #[error("value-pool balance calendar-day offset overflowed")]
    DayOffsetOverflow,
    /// The list has more entries than the on-disk schema can encode.
    #[error("value-pool balance snapshot contains too many pools")]
    TooManyPools,
    /// A pool identifier exceeds the on-disk schema limit.
    #[error("value-pool balance pool identifier is too long")]
    PoolIdTooLong,
    /// A backfill batch was empty.
    #[error("value-pool balance backfill batch is empty")]
    EmptyBackfill,
    /// Coverage or an ordered batch was not contiguous in height.
    #[error("value-pool balance coverage is not contiguous in height")]
    CoverageDiscontinuous,
}
