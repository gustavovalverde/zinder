//! No-wipe startup construction of the transparent-address ranking projection.

use zinder_core::{BlockHash, BlockHeight, ChainEpoch};
use zinder_materialized_views::{
    MaterializedViewStore, TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
    TransparentAddressDeltasConsumer, TransparentAddressRankingConsumer,
    TransparentAddressRankingCoverage, TransparentAddressRankingSnapshotPlan,
    TransparentAddressRankingSnapshotRow, TransparentAddressSummary,
};
use zinder_store::{ChainEventHistoryRequest, PrimaryChainStore, StreamCursorTokenV1};

use crate::{
    IngestError,
    materialized_view_consumers::{
        read_current_block_context_batch, unanimous_existing_block_consumer_cursor,
    },
};

const SNAPSHOT_WRITE_BATCH_ROWS: usize = 2_048;
const TAIL_SEED_BATCH_BLOCKS: u32 = 64;

/// Result of attempting to construct the optional ranking projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransparentAddressRankingBootstrapOutcome {
    /// The projection was already current or became current during this call.
    Ready,
    /// Existing historical sources cannot prove complete lifetime statistics.
    SourceCoverageIncomplete,
    /// No canonical chain or unanimous materialized-view boundary exists yet, or materialized-view
    /// replay still trails the canonical event tail.
    ChainNotReady,
}

enum BootstrapBoundary {
    Ready,
    NotReady,
    Build {
        cursor_bytes: Vec<u8>,
        chain_epoch: ChainEpoch,
    },
}

struct SnapshotBuildInputs {
    rows: Vec<TransparentAddressRankingSnapshotRow>,
    base_block_hash: BlockHash,
    target_block_hash: BlockHash,
    chain_epoch: ChainEpoch,
    cursor_bytes: Vec<u8>,
}

/// Builds the ranking projection without rewriting canonical Zinder data.
pub async fn bootstrap_transparent_address_ranking(
    chain_store: &PrimaryChainStore,
    materialized_view_store: &MaterializedViewStore,
) -> Result<TransparentAddressRankingBootstrapOutcome, IngestError> {
    let (cursor_bytes, chain_epoch) =
        match bootstrap_boundary(chain_store, materialized_view_store)? {
            BootstrapBoundary::Ready => {
                return Ok(TransparentAddressRankingBootstrapOutcome::Ready);
            }
            BootstrapBoundary::NotReady => {
                return Ok(TransparentAddressRankingBootstrapOutcome::ChainNotReady);
            }
            BootstrapBoundary::Build {
                cursor_bytes,
                chain_epoch,
            } => (cursor_bytes, chain_epoch),
        };
    let (snapshot, base_block_hash, target_block_hash, lifetime) =
        read_snapshot_sources(chain_store, materialized_view_store).await?;
    let base_height = snapshot.summarized_height;
    if !lifetime.source_coverage.contiguous_from_height_1
        || lifetime.source_coverage.first_height != Some(BlockHeight::new(1))
        || lifetime.source_coverage.last_height != Some(base_height)
    {
        tracing::warn!(
            target: "zinder::ingest",
            event = "transparent_address_ranking_source_incomplete",
            base_height = base_height.value(),
            first_height = lifetime.source_coverage.first_height.map(BlockHeight::value),
            last_height = lifetime.source_coverage.last_height.map(BlockHeight::value),
            indexed_height_count = lifetime.source_coverage.row_count,
            "transparent-address lifetime delta coverage is incomplete; ranking remains unavailable"
        );
        return Ok(TransparentAddressRankingBootstrapOutcome::SourceCoverageIncomplete);
    }

    let Some(rows) = reconcile_snapshot_rows(snapshot.balances_by_script_hash, lifetime.summaries)
    else {
        tracing::warn!(
            target: "zinder::ingest",
            event = "transparent_address_ranking_balance_reconciliation_failed",
            base_height = base_height.value(),
            "transparent-address lifetime deltas do not reconcile with the settled UTXO snapshot; ranking remains unavailable"
        );
        return Ok(TransparentAddressRankingBootstrapOutcome::SourceCoverageIncomplete);
    };
    build_and_activate_snapshot(
        chain_store,
        materialized_view_store,
        SnapshotBuildInputs {
            rows,
            base_block_hash,
            target_block_hash,
            chain_epoch,
            cursor_bytes,
        },
    )
    .await?;
    Ok(TransparentAddressRankingBootstrapOutcome::Ready)
}

fn bootstrap_boundary(
    chain_store: &PrimaryChainStore,
    materialized_view_store: &MaterializedViewStore,
) -> Result<BootstrapBoundary, IngestError> {
    let Some(cursor_bytes) = unanimous_existing_block_consumer_cursor(materialized_view_store)?
    else {
        return Ok(BootstrapBoundary::NotReady);
    };
    let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes.clone());
    if !chain_store
        .chain_event_history(ChainEventHistoryRequest::with_default_limit(Some(&cursor)))?
        .is_empty()
    {
        // Materialized-view replay trails the canonical event tail: startup hands residual
        // replay to the always-on tailer, so defer until a boot finds parity.
        return Ok(BootstrapBoundary::NotReady);
    }
    let Some(chain_epoch) = chain_store.current_chain_epoch()? else {
        return Ok(BootstrapBoundary::NotReady);
    };
    if let Some(active) =
        TransparentAddressRankingConsumer::active_metadata(materialized_view_store)?
    {
        let ranking_cursor = materialized_view_store
            .get_chain_event_cursor(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)?;
        if ranking_cursor.as_deref() != Some(cursor_bytes.as_slice())
            || active.coverage.balance_complete_through_height != chain_epoch.visible_tip_height
        {
            // The active ranking lags the canonical boundary; the tailer keeps
            // advancing it, so defer rather than fail startup.
            return Ok(BootstrapBoundary::NotReady);
        }
        return Ok(BootstrapBoundary::Ready);
    }
    if materialized_view_store
        .get_chain_event_cursor(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME)?
        .is_some()
    {
        return Err(IngestError::MaterializedViewDispatch(
            "transparent-address ranking cursor exists without an active generation".to_owned(),
        ));
    }
    Ok(BootstrapBoundary::Build {
        cursor_bytes,
        chain_epoch,
    })
}

async fn read_snapshot_sources(
    chain_store: &PrimaryChainStore,
    materialized_view_store: &MaterializedViewStore,
) -> Result<
    (
        zinder_store::TransparentAddressBalanceSnapshot,
        BlockHash,
        BlockHash,
        zinder_materialized_views::TransparentAddressDeltasLifetimeBootstrap,
    ),
    IngestError,
> {
    let store = chain_store.clone();
    let (snapshot, base_block_hash, target_block_hash) = tokio::task::spawn_blocking(move || {
        let reader = store.current_chain_epoch_reader()?;
        let snapshot = reader.settled_transparent_address_balance_snapshot()?;
        let base_block_hash = required_block_hash(&reader, snapshot.summarized_height)?;
        let target_block_hash =
            required_block_hash(&reader, snapshot.chain_epoch.visible_tip_height)?;
        Ok::<_, IngestError>((snapshot, base_block_hash, target_block_hash))
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })??;
    let materialized_view_store = materialized_view_store.clone();
    let base_height = snapshot.summarized_height;
    let lifetime = tokio::task::spawn_blocking(move || {
        TransparentAddressDeltasConsumer::lifetime_summaries_through(
            &materialized_view_store,
            base_height,
        )
        .map_err(IngestError::from)
    })
    .await
    .map_err(|error| IngestError::BlockingTaskFailed {
        reason: error.to_string(),
    })??;
    Ok((snapshot, base_block_hash, target_block_hash, lifetime))
}

async fn build_and_activate_snapshot(
    chain_store: &PrimaryChainStore,
    materialized_view_store: &MaterializedViewStore,
    inputs: SnapshotBuildInputs,
) -> Result<(), IngestError> {
    let base_height = inputs.chain_epoch.settled_tip_height;
    let expected_summary_count = u64::try_from(inputs.rows.len()).map_err(|_| {
        IngestError::MaterializedViewDispatch(
            "transparent-address ranking snapshot row count exceeds u64".to_owned(),
        )
    })?;
    let generation = TransparentAddressRankingConsumer::build_metadata(materialized_view_store)?
        .map_or(1, |metadata| metadata.generation);
    TransparentAddressRankingConsumer::initialize_snapshot_generation(
        materialized_view_store,
        TransparentAddressRankingSnapshotPlan {
            generation,
            base_height,
            base_block_hash: inputs.base_block_hash,
            target_height: inputs.chain_epoch.visible_tip_height,
            target_block_hash: inputs.target_block_hash,
            expected_summary_count,
            base_coverage: TransparentAddressRankingCoverage {
                balance_complete_through_height: base_height,
                history_complete_from_height: Some(BlockHeight::new(1)),
                history_complete_through_height: Some(base_height),
                lifetime_statistics_complete: true,
            },
        },
    )
    .map_err(ranking_error)?;
    for batch in inputs.rows.chunks(SNAPSHOT_WRITE_BATCH_ROWS) {
        TransparentAddressRankingConsumer::write_snapshot_batch(
            materialized_view_store,
            generation,
            batch,
        )
        .map_err(ranking_error)?;
    }
    TransparentAddressRankingConsumer::finalize_snapshot_base(materialized_view_store, generation)
        .map_err(ranking_error)?;
    seed_visible_tail(
        chain_store,
        materialized_view_store,
        generation,
        base_height,
        inputs.chain_epoch.visible_tip_height,
    )
    .await?;
    let metadata = TransparentAddressRankingConsumer::activate_snapshot_generation_at_cursor(
        materialized_view_store,
        generation,
        &inputs.cursor_bytes,
    )
    .map_err(ranking_error)?;
    tracing::info!(
        target: "zinder::ingest",
        event = "transparent_address_ranking_activated",
        generation = metadata.generation,
        through_height = metadata.coverage.balance_complete_through_height.value(),
        positive_address_count = metadata.positive_address_count,
        total_positive_balance_zat = metadata.total_positive_balance_zat,
        "transparent-address ranking snapshot activated without changing canonical data"
    );
    Ok(())
}

async fn seed_visible_tail(
    chain_store: &PrimaryChainStore,
    materialized_view_store: &MaterializedViewStore,
    generation: u64,
    base_height: BlockHeight,
    target_height: BlockHeight,
) -> Result<(), IngestError> {
    let Some(mut next_height) = base_height.next() else {
        return Err(IngestError::MaterializedViewDispatch(
            "transparent-address ranking base height overflowed".to_owned(),
        ));
    };
    while next_height <= target_height {
        let batch_end = BlockHeight::new(
            next_height
                .value()
                .saturating_add(TAIL_SEED_BATCH_BLOCKS.saturating_sub(1))
                .min(target_height.value()),
        );
        let contexts =
            read_current_block_context_batch(chain_store, next_height, batch_end).await?;
        for context in contexts {
            TransparentAddressRankingConsumer::write_snapshot_tail_block(
                materialized_view_store,
                generation,
                context.as_ref(),
            )
            .map_err(ranking_error)?;
        }
        next_height = batch_end.next().ok_or_else(|| {
            IngestError::MaterializedViewDispatch(
                "transparent-address ranking tail height overflowed".to_owned(),
            )
        })?;
    }
    Ok(())
}

fn reconcile_snapshot_rows(
    mut balances: std::collections::HashMap<
        zinder_core::TransparentAddressScriptHash,
        zinder_store::TransparentAddressBalanceSummary,
    >,
    lifetime_summaries: Vec<zinder_materialized_views::TransparentAddressLifetimeSummary>,
) -> Option<Vec<TransparentAddressRankingSnapshotRow>> {
    let mut rows = Vec::with_capacity(lifetime_summaries.len());
    for lifetime in lifetime_summaries {
        let current = balances.remove(&lifetime.address_script_hash);
        let balance_zat = current.as_ref().map_or(0, |summary| summary.balance_zat);
        if lifetime.validated_balance_zat != Some(balance_zat) {
            return None;
        }
        rows.push(TransparentAddressRankingSnapshotRow {
            address_script_hash: lifetime.address_script_hash,
            summary: TransparentAddressSummary {
                script_pub_key: current.map(|summary| summary.script_pub_key),
                balance_zat,
                total_received_zat: lifetime.received_zat,
                total_sent_zat: lifetime.sent_zat,
                distinct_transaction_count: lifetime.distinct_transaction_count,
                first_seen_unix_seconds: None,
                last_seen_unix_seconds: None,
                snapshot_first_seen_unix_seconds: Some(lifetime.first_block_time_unix_seconds),
                snapshot_last_seen_unix_seconds: Some(lifetime.last_block_time_unix_seconds),
            },
        });
    }
    balances.is_empty().then_some(rows)
}

fn required_block_hash(
    reader: &zinder_store::ChainEpochReader<'_>,
    height: BlockHeight,
) -> Result<BlockHash, IngestError> {
    reader
        .block_header_at(height)?
        .map(|header| header.block_hash)
        .ok_or_else(|| {
            IngestError::MaterializedViewDispatch(format!(
                "transparent-address ranking boundary block {} is unavailable",
                height.value()
            ))
        })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the signature is passed directly to Result::map_err"
)]
fn ranking_error(error: impl ToString) -> IngestError {
    IngestError::MaterializedViewDispatch(error.to_string())
}

#[cfg(test)]
mod tests {
    use zinder_core::TransparentAddressScriptHash;
    use zinder_materialized_views::TransparentAddressLifetimeSummary;
    use zinder_store::TransparentAddressBalanceSummary;

    use super::*;

    #[test]
    fn reconciliation_requires_exact_balance_equality() -> Result<(), IngestError> {
        let script = vec![0x51];
        let hash = TransparentAddressScriptHash::of_script_pub_key(&script);
        let balances = std::collections::HashMap::from([(
            hash,
            TransparentAddressBalanceSummary {
                script_pub_key: script,
                balance_zat: 7,
                utxo_count: 1,
            },
        )]);
        let lifetime = TransparentAddressLifetimeSummary {
            address_script_hash: hash,
            received_zat: 10,
            sent_zat: 3,
            distinct_transaction_count: 2,
            first_block_time_unix_seconds: 100,
            last_block_time_unix_seconds: 200,
            net_balance_zat: 7,
            validated_balance_zat: Some(7),
        };
        let rows =
            reconcile_snapshot_rows(balances.clone(), vec![lifetime.clone()]).ok_or_else(|| {
                IngestError::MaterializedViewDispatch(
                    "matching balance did not reconcile".to_owned(),
                )
            })?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary.balance_zat, 7);

        let mut mismatched = lifetime;
        mismatched.validated_balance_zat = Some(8);
        assert!(reconcile_snapshot_rows(balances, vec![mismatched]).is_none());
        Ok(())
    }
}
