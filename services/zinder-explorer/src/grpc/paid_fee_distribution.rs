//! `ExplorerQuery.PaidFeeDistribution` handler.
//!
//! Maps exact miner-collected fee frequencies from the derive projection
//! without computing percentiles or product-specific buckets.

use tonic::{Request, Response, Status};
use zinder_derive::{
    DeriveStore, PaidFeeDistribution as DerivedPaidFeeDistribution,
    PaidFeeDistributionBackfillCoverage, PaidFeeDistributionConsumer,
    PaidFeeDistributionDay as DerivedPaidFeeDistributionDay,
    PaidFeeFrequency as DerivedPaidFeeFrequency,
};
use zinder_proto::capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1;
use zinder_proto::v1::explorer::{
    PaidFeeDistributionCoverage, PaidFeeDistributionDay, PaidFeeDistributionRequest,
    PaidFeeDistributionResponse, PaidFeeFrequency,
};
use zinder_proto::v1::wallet::{self, LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness_from_snapshot,
    indexed_tip_matches_chain_epoch,
};

/// Executes one `ExplorerQuery.PaidFeeDistribution` request.
pub(crate) async fn handle_paid_fee_distribution(
    derive_store: &DeriveStore,
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

    for _ in 0..MAX_EPOCH_STABILIZATION_ATTEMPTS {
        let (chain_epoch, visible_tip_height) = fetch_current_chain_epoch(wallet_client).await?;
        let (distribution, coverage, freshness) = {
            let snapshot = derive_store.read_snapshot();
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
                visible_tip_height,
                indexed_tip_matches,
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

fn map_distribution(distribution: DerivedPaidFeeDistribution) -> Vec<PaidFeeDistributionDay> {
    distribution.days.into_iter().map(map_day).collect()
}

fn map_day(day: DerivedPaidFeeDistributionDay) -> PaidFeeDistributionDay {
    PaidFeeDistributionDay {
        day_start_unix_seconds: day.day_start_unix_seconds,
        frequencies: day.frequencies.into_iter().map(map_frequency).collect(),
        unavailable_transaction_count: day.unavailable_transaction_count,
    }
}

fn map_frequency(frequency: DerivedPaidFeeFrequency) -> PaidFeeFrequency {
    PaidFeeFrequency {
        paid_fee_zat: frequency.paid_fee_zat,
        transaction_count: frequency.transaction_count,
    }
}

fn map_coverage(
    coverage: PaidFeeDistributionBackfillCoverage,
    visible_tip_height: u32,
    indexed_tip_matches: bool,
) -> PaidFeeDistributionCoverage {
    PaidFeeDistributionCoverage {
        complete_from_height: Some(coverage.complete_from_height.value()),
        complete_through_height: Some(coverage.complete_through_height.value()),
        complete_from_time_unix_seconds: Some(coverage.complete_from_time_unix_seconds),
        complete_through_time_unix_seconds: Some(coverage.complete_through_time_unix_seconds),
        requested_range_complete: indexed_tip_matches
            && coverage.complete_from_height.value() <= 1
            && coverage.complete_through_height.value() >= visible_tip_height,
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
    use zinder_core::BlockHeight;

    #[test]
    fn rejects_empty_or_reversed_time_ranges() {
        for (start, end) in [(10, 10), (11, 10)] {
            let outcome = validate_time_range(start, end);
            assert!(matches!(outcome, Err(error) if error.code() == Code::InvalidArgument));
        }
    }

    #[test]
    fn mapping_preserves_exact_frequency_order_and_unavailable_count() {
        let days = map_distribution(DerivedPaidFeeDistribution {
            days: vec![DerivedPaidFeeDistributionDay {
                day_start_unix_seconds: 1_700_006_400,
                frequencies: vec![
                    DerivedPaidFeeFrequency {
                        paid_fee_zat: 10_000,
                        transaction_count: 4,
                    },
                    DerivedPaidFeeFrequency {
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
            200,
            true,
        );
        assert!(complete.requested_range_complete);

        let missing_earlier_heights = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(2),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            true,
        );
        assert!(!missing_earlier_heights.requested_range_complete);

        let ends_after_lagging_coverage = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(199),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            true,
        );
        assert!(!ends_after_lagging_coverage.requested_range_complete);

        let mismatched_indexed_tip = map_coverage(
            PaidFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            false,
        );
        assert!(!mismatched_indexed_tip.requested_range_complete);
    }
}
