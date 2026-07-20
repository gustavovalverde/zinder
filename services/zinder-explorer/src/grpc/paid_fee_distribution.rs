//! `ExplorerQuery.PaidFeeDistribution` handler.
//!
//! Maps exact miner-collected fee frequencies from the materialized view
//! without computing percentiles or product-specific buckets.

use tonic::{Request, Response, Status};
use zinder_core::{BlockHeight, CanonicalHistoryBounds};
use zinder_materialized_views::{
    MaterializedViewStore, PaidFeeDistribution as ProjectedPaidFeeDistribution,
    PaidFeeDistributionBackfillCoverage, PaidFeeDistributionConsumer,
    PaidFeeDistributionDay as ProjectedPaidFeeDistributionDay,
    PaidFeeFrequency as ProjectedPaidFeeFrequency,
};
use zinder_proto::capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1;
use zinder_proto::v1::explorer::{
    PaidFeeDistributionCoverage, PaidFeeDistributionDay, PaidFeeDistributionRequest,
    PaidFeeDistributionResponse, PaidFeeFrequency,
};
use zinder_proto::v1::wallet::{self, LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;
use zinder_store::SecondaryChainStore;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness_from_snapshot,
    indexed_tip_matches_chain_epoch,
};

/// Executes one `ExplorerQuery.PaidFeeDistribution` request.
pub(crate) async fn handle_paid_fee_distribution(
    materialized_view_store: &MaterializedViewStore,
    canonical_store: &SecondaryChainStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<PaidFeeDistributionRequest>,
) -> Result<Response<PaidFeeDistributionResponse>, Status> {
    const MAX_EPOCH_STABILIZATION_ATTEMPTS: usize = 3;

    let request = request.into_inner();
    validate_time_range(
        request.start_time_unix_seconds,
        request.end_time_unix_seconds,
    )?;
    canonical_store
        .try_catch_up()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let canonical_history_bounds = canonical_store
        .current_chain_epoch_reader()
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .canonical_history_bounds();

    for _ in 0..MAX_EPOCH_STABILIZATION_ATTEMPTS {
        let (chain_epoch, visible_tip_height) = fetch_current_chain_epoch(wallet_client).await?;
        let (distribution, coverage, freshness) = {
            let snapshot = materialized_view_store.read_snapshot();
            let distribution = PaidFeeDistributionConsumer::distribution_in_time_range_snapshot(
                &snapshot,
                request.start_time_unix_seconds,
                request.end_time_unix_seconds,
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
            let coverage = PaidFeeDistributionConsumer::coverage_snapshot(&snapshot)
                .map_err(|error| ExplorerError::internal(error.to_string()))?
                .ok_or_else(|| {
                    ExplorerError::not_materialized(
                        "paid-fee distribution has no materialized projection coverage",
                    )
                })?;
            let freshness = build_explorer_freshness_from_snapshot(
                &snapshot,
                EXPLORER_PAID_FEE_DISTRIBUTION_V1,
                Some(chain_epoch.clone()),
                0,
            )?;
            drop(snapshot);
            (distribution, coverage, freshness)
        };
        let (observed_chain_epoch, _) = fetch_current_chain_epoch(wallet_client).await?;
        if observed_chain_epoch != chain_epoch {
            continue;
        }
        let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
        let indexed_tip_matches = indexed_tip_matches_chain_epoch(&freshness, &chain_epoch);

        return Ok(Response::new(PaidFeeDistributionResponse {
            freshness: Some(freshness),
            days: map_distribution(distribution),
            coverage: Some(map_coverage(
                coverage,
                CoverageEvaluation {
                    visible_tip_height,
                    indexed_tip_matches,
                    requested_start_time_unix_seconds: request.start_time_unix_seconds,
                    requested_end_time_unix_seconds: request.end_time_unix_seconds,
                    canonical_history_bounds,
                },
            )),
        }));
    }

    Err(ExplorerError::upstream_unreachable(
        "chain epoch changed while reading the paid-fee distribution",
    )
    .into())
}

fn validate_time_range(start: i64, end: i64) -> Result<(), Status> {
    if start >= end {
        return Err(ExplorerError::invalid_request(
            "start_time_unix_seconds must be less than end_time_unix_seconds",
        )
        .into());
    }
    Ok(())
}

async fn fetch_current_chain_epoch(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<(wallet::ChainEpoch, u32), Status> {
    let chain_epoch = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner()
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("LatestBlockResponse.chain_view.chain_epoch missing")
        })?;
    let visible_tip_height = chain_epoch
        .visible_tip
        .as_ref()
        .map(|tip| tip.height)
        .ok_or_else(|| ExplorerError::internal("ChainEpoch.visible_tip missing"))?;
    Ok((chain_epoch, visible_tip_height))
}

fn map_distribution(distribution: ProjectedPaidFeeDistribution) -> Vec<PaidFeeDistributionDay> {
    distribution.days.into_iter().map(map_day).collect()
}

fn map_day(day: ProjectedPaidFeeDistributionDay) -> PaidFeeDistributionDay {
    PaidFeeDistributionDay {
        day_start_unix_seconds: day.day_start_unix_seconds,
        frequencies: day.frequencies.into_iter().map(map_frequency).collect(),
        unavailable_transaction_count: day.unavailable_transaction_count,
    }
}

fn map_frequency(frequency: ProjectedPaidFeeFrequency) -> PaidFeeFrequency {
    PaidFeeFrequency {
        paid_fee_zat: frequency.paid_fee_zat,
        transaction_count: frequency.transaction_count,
    }
}

#[derive(Clone, Copy)]
struct CoverageEvaluation {
    visible_tip_height: u32,
    indexed_tip_matches: bool,
    requested_start_time_unix_seconds: i64,
    requested_end_time_unix_seconds: i64,
    canonical_history_bounds: CanonicalHistoryBounds,
}

fn map_coverage(
    coverage: PaidFeeDistributionBackfillCoverage,
    evaluation: CoverageEvaluation,
) -> PaidFeeDistributionCoverage {
    let CoverageEvaluation {
        visible_tip_height,
        indexed_tip_matches,
        requested_start_time_unix_seconds,
        requested_end_time_unix_seconds,
        canonical_history_bounds,
    } = evaluation;
    PaidFeeDistributionCoverage {
        complete_from_height: Some(coverage.complete_from_height.value()),
        complete_through_height: Some(coverage.complete_through_height.value()),
        complete_from_time_unix_seconds: Some(coverage.complete_from_time_unix_seconds),
        complete_through_time_unix_seconds: Some(coverage.complete_through_time_unix_seconds),
        requested_range_complete: indexed_tip_matches
            && !canonical_history_bounds.intentionally_excludes(BlockHeight::new(1))
            && requested_start_time_unix_seconds >= coverage.complete_from_time_unix_seconds
            && (requested_end_time_unix_seconds
                <= coverage
                    .complete_through_time_unix_seconds
                    .saturating_add(1)
                || coverage.complete_through_height.value() >= visible_tip_height),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use tonic::Code;
    use zinder_core::{BlockHash, BlockId, CanonicalHistoryBoundsError};

    #[test]
    fn rejects_empty_or_reversed_time_ranges() {
        for (start, end) in [(10, 10), (11, 10)] {
            let outcome = validate_time_range(start, end);
            assert!(matches!(outcome, Err(error) if error.code() == Code::InvalidArgument));
        }
    }

    #[test]
    fn mapping_preserves_exact_frequency_order_and_unavailable_count() {
        let days = map_distribution(ProjectedPaidFeeDistribution {
            days: vec![ProjectedPaidFeeDistributionDay {
                day_start_unix_seconds: 1_700_006_400,
                frequencies: vec![
                    ProjectedPaidFeeFrequency {
                        paid_fee_zat: 10_000,
                        transaction_count: 4,
                    },
                    ProjectedPaidFeeFrequency {
                        paid_fee_zat: 20_000,
                        transaction_count: 2,
                    },
                ],
                unavailable_transaction_count: 3,
            }],
        });

        assert_eq!(days.len(), 1);
        assert_eq!(days[0].frequencies[0].paid_fee_zat, 10_000);
        assert_eq!(days[0].frequencies[1].paid_fee_zat, 20_000);
        assert_eq!(days[0].unavailable_transaction_count, 3);
    }

    #[test]
    fn time_range_completeness_requires_full_height_domain_and_indexed_tip_identity() {
        let complete = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            CoverageEvaluation {
                visible_tip_height: 200,
                indexed_tip_matches: true,
                requested_start_time_unix_seconds: 1_699_500_000,
                requested_end_time_unix_seconds: 1_700_100_000,
                canonical_history_bounds: CanonicalHistoryBounds::complete(),
            },
        );
        assert!(complete.requested_range_complete);

        let missing_earlier_heights = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(2),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            CoverageEvaluation {
                visible_tip_height: 200,
                indexed_tip_matches: true,
                requested_start_time_unix_seconds: 1_698_999_999,
                requested_end_time_unix_seconds: 1_700_100_000,
                canonical_history_bounds: CanonicalHistoryBounds::complete(),
            },
        );
        assert!(!missing_earlier_heights.requested_range_complete);

        let ends_after_lagging_coverage = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(199),
                1_699_000_000,
                1_700_000_000,
            ),
            CoverageEvaluation {
                visible_tip_height: 200,
                indexed_tip_matches: true,
                requested_start_time_unix_seconds: 1_699_500_000,
                requested_end_time_unix_seconds: 1_700_100_000,
                canonical_history_bounds: CanonicalHistoryBounds::complete(),
            },
        );
        assert!(!ends_after_lagging_coverage.requested_range_complete);

        let mismatched_indexed_tip = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            CoverageEvaluation {
                visible_tip_height: 200,
                indexed_tip_matches: false,
                requested_start_time_unix_seconds: 1_699_500_000,
                requested_end_time_unix_seconds: 1_700_100_000,
                canonical_history_bounds: CanonicalHistoryBounds::complete(),
            },
        );
        assert!(!mismatched_indexed_tip.requested_range_complete);
    }

    #[test]
    fn checkpointed_history_never_claims_requested_time_range_is_complete()
    -> Result<(), CanonicalHistoryBoundsError> {
        let coverage = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(501),
                BlockHeight::new(700),
                1_699_000_000,
                1_700_000_000,
            ),
            CoverageEvaluation {
                visible_tip_height: 700,
                indexed_tip_matches: true,
                requested_start_time_unix_seconds: 1_699_500_000,
                requested_end_time_unix_seconds: 1_700_100_000,
                canonical_history_bounds: CanonicalHistoryBounds::checkpointed(BlockId::new(
                    BlockHeight::new(500),
                    BlockHash::from_bytes([1; 32]),
                ))?,
            },
        );

        assert!(!coverage.requested_range_complete);
        Ok(())
    }
}
