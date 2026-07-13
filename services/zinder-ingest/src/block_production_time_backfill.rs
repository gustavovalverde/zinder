//! Background full-history backfill for the block-production time index.

use std::time::Duration;

use prost::Message as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::BlockHeight;
use zinder_core::wire::{
    decode_height_key_ascending, decode_rpc_block_hash_hex, encode_height_key_ascending,
};
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BlockProductionTimeBackfillCoverage, BlockProductionTimeConsumer,
    BlockProductionTimeRow, DeriveStore,
};
use zinder_proto::v1::explorer::BlockSummaryRecord;
use zinder_runtime::Readiness;

use crate::{
    IngestError, derive_consumers::derive_projection_write_guard,
    ingest_loop::wait_until_tip_follow_or_cancelled,
};

const BACKFILL_BATCH_BLOCKS: u32 = 4_096;
const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Runs the full-history time-index backfill, deferring its writes until
/// canonical ingest reaches tip follow so bulk catch-up owns the storage budget.
#[must_use = "await the handle during shutdown"]
pub fn spawn_block_production_time_backfill_task(
    derive_store: DeriveStore,
    readiness: Readiness,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut completion_logged = false;
        loop {
            if wait_until_tip_follow_or_cancelled(&readiness, &cancel).await {
                return;
            }
            let wait_before_next_attempt = match backfill_next_batch(&derive_store) {
                Ok(BackfillProgress::Advanced { from, through }) => {
                    completion_logged = false;
                    tracing::info!(
                        target: "zinder::ingest",
                        event = "block_production_time_backfill_progress",
                        from_height = from.value(),
                        through_height = through.value(),
                        "block-production time coverage advanced"
                    );
                    false
                }
                Ok(BackfillProgress::Complete { through }) => {
                    if !completion_logged {
                        tracing::info!(
                            target: "zinder::ingest",
                            event = "block_production_time_backfill_completed",
                            through_height = through.map(BlockHeight::value),
                            "block-production time index covers the startup canonical history"
                        );
                        completion_logged = true;
                    }
                    true
                }
                Err(error) => {
                    completion_logged = false;
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "block_production_time_backfill_retry",
                        error = %error,
                        retry_delay_seconds = BACKFILL_RETRY_INTERVAL.as_secs(),
                        "block-production time backfill failed; durable coverage was not advanced"
                    );
                    true
                }
            };

            if !wait_before_next_attempt {
                tokio::task::yield_now().await;
                if cancel.is_cancelled() {
                    return;
                }
                continue;
            }

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(BACKFILL_RETRY_INTERVAL) => {}
            }
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackfillProgress {
    Advanced {
        from: BlockHeight,
        through: BlockHeight,
    },
    Complete {
        through: Option<BlockHeight>,
    },
}

fn backfill_next_batch(derive_store: &DeriveStore) -> Result<BackfillProgress, IngestError> {
    let Some(tail) = BlockProductionTimeConsumer::tail_coverage(derive_store)? else {
        return Ok(BackfillProgress::Complete { through: None });
    };
    let Some(target_height) = tail
        .boundary_height
        .value()
        .checked_sub(1)
        .map(BlockHeight::new)
    else {
        return Ok(BackfillProgress::Complete { through: None });
    };
    let coverage = BlockProductionTimeConsumer::backfill_coverage(derive_store)?;
    if coverage.is_some_and(|coverage| coverage.complete_through_height >= target_height) {
        return Ok(BackfillProgress::Complete {
            through: coverage.map(|coverage| coverage.complete_through_height),
        });
    }

    let first_height = first_block_summary_height(derive_store)?.ok_or_else(|| {
        IngestError::DeriveDispatch(
            "block-production time backfill has a tail boundary but no block summaries".to_owned(),
        )
    })?;
    let from_height = coverage
        .and_then(|coverage| coverage.complete_through_height.next())
        .unwrap_or(first_height);
    if from_height > target_height {
        return Ok(BackfillProgress::Complete {
            through: coverage.map(|coverage| coverage.complete_through_height),
        });
    }
    let through_height = BlockHeight::new(
        from_height
            .value()
            .saturating_add(BACKFILL_BATCH_BLOCKS.saturating_sub(1))
            .min(target_height.value()),
    );
    let rows = read_block_production_rows(derive_store, from_height, through_height)?;
    let first_row = rows.first().ok_or_else(|| {
        IngestError::DeriveDispatch(
            "block-production time backfill decoded an empty block-summary batch".to_owned(),
        )
    })?;
    let last_row = rows.last().ok_or_else(|| {
        IngestError::DeriveDispatch(
            "block-production time backfill decoded an empty block-summary batch".to_owned(),
        )
    })?;
    let next_coverage = BlockProductionTimeBackfillCoverage::new(
        coverage.map_or(first_row.block_height, |coverage| {
            coverage.complete_from_height
        }),
        last_row.block_height,
        coverage.map_or(first_row.block_time_unix_seconds, |coverage| {
            coverage.complete_from_time_unix_seconds
        }),
        last_row.block_time_unix_seconds,
    );
    let _write_guard = derive_projection_write_guard();
    BlockProductionTimeConsumer::write_backfill_rows(derive_store, &rows, next_coverage)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    Ok(BackfillProgress::Advanced {
        from: from_height,
        through: through_height,
    })
}

fn first_block_summary_height(
    derive_store: &DeriveStore,
) -> Result<Option<BlockHeight>, IngestError> {
    derive_store
        .range_iterate_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(0)),
            &encode_height_key_ascending(BlockHeight::new(u32::MAX)),
            1,
        )?
        .first()
        .map(|(key, _)| {
            decode_height_key_ascending(key)
                .map_err(|error| IngestError::DeriveDispatch(error.to_string()))
        })
        .transpose()
}

fn read_block_production_rows(
    derive_store: &DeriveStore,
    from_height: BlockHeight,
    through_height: BlockHeight,
) -> Result<Vec<BlockProductionTimeRow>, IngestError> {
    let expected_count =
        usize::try_from(u64::from(through_height.value()) - u64::from(from_height.value()) + 1)
            .unwrap_or(usize::MAX);
    let entries = derive_store.range_iterate_consumer(
        BLOCK_SUMMARY_COLUMN_FAMILY,
        &encode_height_key_ascending(from_height),
        &encode_height_key_ascending(through_height),
        expected_count,
    )?;
    if entries.len() != expected_count {
        return Err(IngestError::DeriveDispatch(format!(
            "block-production time backfill expected {expected_count} block summaries, found {}",
            entries.len(),
        )));
    }
    entries
        .into_iter()
        .map(|(key, payload)| block_production_row(&key, &payload))
        .collect()
}

fn block_production_row(key: &[u8], payload: &[u8]) -> Result<BlockProductionTimeRow, IngestError> {
    let key_height = decode_height_key_ascending(key)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let record = BlockSummaryRecord::decode(payload)
        .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?;
    let summary = record.summary.ok_or_else(|| {
        IngestError::DeriveDispatch("BlockSummaryRecord.summary missing".to_owned())
    })?;
    if summary.block_height != key_height.value() {
        return Err(IngestError::DeriveDispatch(format!(
            "block-summary key height {} disagrees with payload height {}",
            key_height.value(),
            summary.block_height,
        )));
    }
    Ok(BlockProductionTimeRow {
        block_time_unix_seconds: summary.block_time_unix_seconds,
        block_height: key_height,
        block_hash: decode_rpc_block_hash_hex(&summary.block_hash)
            .map_err(|error| IngestError::DeriveDispatch(error.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the backfill behavior under test."
    )]

    use zinder_core::BlockHash;
    use zinder_core::wire::encode_rpc_block_hash_hex;
    use zinder_derive::{
        BLOCK_PRODUCTION_TIME_MAX_PAGE_SIZE, BLOCK_PRODUCTION_TIME_SCHEMA, BLOCK_SUMMARY_SCHEMA,
        BlockProductionTimePageRequest, DeriveStoreOptions,
    };
    use zinder_store::RocksDbResourceBudget;

    use super::*;

    fn block_hash(seed: u8) -> BlockHash {
        BlockHash::from_bytes([seed; 32])
    }

    fn summary_record(height: u32, hash_seed: u8, time: i64) -> BlockSummaryRecord {
        BlockSummaryRecord {
            summary: Some(zinder_proto::v1::explorer::BlockSummary {
                block_height: height,
                block_hash: encode_rpc_block_hash_hex(block_hash(hash_seed)),
                block_time_unix_seconds: time,
                ..Default::default()
            }),
            transaction_ids: vec![format!("{hash_seed:02x}").repeat(32)],
            ..Default::default()
        }
    }

    fn open_store() -> eyre::Result<(tempfile::TempDir, DeriveStore)> {
        let tempdir = tempfile::tempdir()?;
        let store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                sync_writes: false,
                consumers: &[BLOCK_SUMMARY_SCHEMA, BLOCK_PRODUCTION_TIME_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        Ok((tempdir, store))
    }

    #[test]
    fn block_production_row_decodes_a_valid_block_summary_record() -> eyre::Result<()> {
        let height = BlockHeight::new(100);
        let record = summary_record(height.value(), 0xa1, 1_774_670_000);

        let row = block_production_row(
            &encode_height_key_ascending(height),
            &record.encode_to_vec(),
        )?;

        assert_eq!(row.block_height, height);
        assert_eq!(row.block_hash, block_hash(0xa1));
        assert_eq!(row.block_time_unix_seconds, 1_774_670_000);
        Ok(())
    }

    #[test]
    fn block_production_row_rejects_malformed_or_inconsistent_summary_records() {
        let height = BlockHeight::new(100);
        let key = encode_height_key_ascending(height);
        let missing_summary = BlockSummaryRecord::default();
        let mismatched_height = summary_record(101, 0xa2, 1_774_670_001);
        let malformed_hash = BlockSummaryRecord {
            summary: Some(zinder_proto::v1::explorer::BlockSummary {
                block_height: height.value(),
                block_hash: "not-a-block-hash".to_owned(),
                block_time_unix_seconds: 1_774_670_003,
                ..Default::default()
            }),
            transaction_ids: vec![format!("{:02x}", 0xa4).repeat(32)],
            ..Default::default()
        };

        for payload in [
            vec![0xff],
            missing_summary.encode_to_vec(),
            mismatched_height.encode_to_vec(),
            malformed_hash.encode_to_vec(),
        ] {
            assert!(block_production_row(&key, &payload).is_err());
        }
    }

    #[test]
    fn backfill_materializes_summary_rows_and_advances_coverage() -> eyre::Result<()> {
        let (_tempdir, store) = open_store()?;
        let first = summary_record(100, 0xb1, 1_774_670_100);
        let second = summary_record(101, 0xb2, 1_774_670_050);
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(100)),
            &first.encode_to_vec(),
        )?;
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(101)),
            &second.encode_to_vec(),
        )?;
        BlockProductionTimeConsumer::initialize_tail_boundary(&store, BlockHeight::new(102))?;

        assert_eq!(
            backfill_next_batch(&store)?,
            BackfillProgress::Advanced {
                from: BlockHeight::new(100),
                through: BlockHeight::new(101),
            }
        );
        assert_eq!(
            BlockProductionTimeConsumer::backfill_coverage(&store)?,
            Some(BlockProductionTimeBackfillCoverage::new(
                BlockHeight::new(100),
                BlockHeight::new(101),
                1_774_670_100,
                1_774_670_050,
            ))
        );
        let page = BlockProductionTimeConsumer::read_page(
            &store,
            BlockProductionTimePageRequest {
                start_time_unix_seconds: 1_774_670_000,
                end_time_unix_seconds: 1_774_670_200,
                after: None,
                maximum_height: None,
                limit: BLOCK_PRODUCTION_TIME_MAX_PAGE_SIZE,
            },
        )?;
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.block_height.value())
                .collect::<Vec<_>>(),
            vec![101, 100]
        );
        assert_eq!(
            backfill_next_batch(&store)?,
            BackfillProgress::Complete {
                through: Some(BlockHeight::new(101)),
            }
        );
        Ok(())
    }
}
