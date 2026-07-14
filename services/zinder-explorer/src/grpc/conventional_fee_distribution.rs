//! `ExplorerQuery.ConventionalFeeDistribution` handler.
//!
//! Maps exact ZIP-317 conventional-fee frequencies from the derive projection
//! without computing paid fees, percentiles, or product-specific buckets.

use tonic::{Request, Response, Status};
use zinder_core::BlockHeight;
use zinder_derive::{
    ConventionalFeeDistribution as DerivedConventionalFeeDistribution,
    ConventionalFeeDistributionBackfillCoverage, ConventionalFeeDistributionConsumer,
    ConventionalFeeDistributionDay as DerivedConventionalFeeDistributionDay,
    ConventionalFeeFrequency as DerivedConventionalFeeFrequency, DeriveStore,
};
use zinder_proto::capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1;
use zinder_proto::v1::explorer::{
    ConventionalFeeDistributionCoverage, ConventionalFeeDistributionDay,
    ConventionalFeeDistributionRequest, ConventionalFeeDistributionResponse,
    ConventionalFeeFrequency,
};
use zinder_proto::v1::wallet::{self, LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

/// Executes one `ExplorerQuery.ConventionalFeeDistribution` request.
pub(crate) async fn handle_conventional_fee_distribution(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<ConventionalFeeDistributionRequest>,
) -> Result<Response<ConventionalFeeDistributionResponse>, Status> {
    let request = request.into_inner();
    validate_time_range(
        request.start_time_unix_seconds,
        request.end_time_unix_seconds,
    )?;

    let distribution = ConventionalFeeDistributionConsumer::distribution_in_time_range(
        derive_store,
        request.start_time_unix_seconds,
        request.end_time_unix_seconds,
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let coverage = ConventionalFeeDistributionConsumer::coverage(derive_store)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(
                "conventional-fee distribution has no materialized projection coverage",
            )
        })?;
    let (chain_epoch, visible_tip_height) = fetch_current_chain_epoch(wallet_client).await?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(ConventionalFeeDistributionResponse {
        freshness: Some(freshness),
        days: map_distribution(distribution),
        coverage: Some(map_coverage(
            coverage,
            visible_tip_height,
            request.start_time_unix_seconds,
            request.end_time_unix_seconds,
        )),
    }))
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

fn map_distribution(
    distribution: DerivedConventionalFeeDistribution,
) -> Vec<ConventionalFeeDistributionDay> {
    distribution.days.into_iter().map(map_day).collect()
}

fn map_day(day: DerivedConventionalFeeDistributionDay) -> ConventionalFeeDistributionDay {
    ConventionalFeeDistributionDay {
        day_start_unix_seconds: day.day_start_unix_seconds,
        frequencies: day.frequencies.into_iter().map(map_frequency).collect(),
        unavailable_transaction_count: day.unavailable_transaction_count,
    }
}

fn map_frequency(frequency: DerivedConventionalFeeFrequency) -> ConventionalFeeFrequency {
    ConventionalFeeFrequency {
        zip317_conventional_fee_zat: frequency.zip317_conventional_fee_zat,
        transaction_count: frequency.transaction_count,
    }
}

fn map_coverage(
    coverage: ConventionalFeeDistributionBackfillCoverage,
    visible_tip_height: u32,
    requested_start_time_unix_seconds: i64,
    requested_end_time_unix_seconds: i64,
) -> ConventionalFeeDistributionCoverage {
    ConventionalFeeDistributionCoverage {
        complete_from_height: Some(coverage.complete_from_height.value()),
        complete_through_height: Some(coverage.complete_through_height.value()),
        complete_from_time_unix_seconds: Some(coverage.complete_from_time_unix_seconds),
        complete_through_time_unix_seconds: Some(coverage.complete_through_time_unix_seconds),
        requested_range_complete: coverage.complete_from_height == BlockHeight::new(1)
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
    #[test]
    fn rejects_empty_or_reversed_time_ranges() {
        for (start, end) in [(10, 10), (11, 10)] {
            let outcome = validate_time_range(start, end);
            assert!(matches!(outcome, Err(error) if error.code() == Code::InvalidArgument));
        }
    }

    #[test]
    fn mapping_preserves_exact_frequency_order_and_unavailable_count() {
        let days = map_distribution(DerivedConventionalFeeDistribution {
            days: vec![DerivedConventionalFeeDistributionDay {
                day_start_unix_seconds: 1_700_006_400,
                frequencies: vec![
                    DerivedConventionalFeeFrequency {
                        zip317_conventional_fee_zat: 10_000,
                        transaction_count: 4,
                    },
                    DerivedConventionalFeeFrequency {
                        zip317_conventional_fee_zat: 20_000,
                        transaction_count: 2,
                    },
                ],
                unavailable_transaction_count: 3,
            }],
        });

        assert_eq!(days.len(), 1);
        assert_eq!(days[0].frequencies[0].zip317_conventional_fee_zat, 10_000);
        assert_eq!(days[0].frequencies[1].zip317_conventional_fee_zat, 20_000);
        assert_eq!(days[0].unavailable_transaction_count, 3);
    }

    #[test]
    fn coverage_completeness_is_specific_to_the_requested_range() {
        let checkpoint_bounded = map_coverage(
            ConventionalFeeDistributionBackfillCoverage::new(
                BlockHeight::new(100),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            1_699_500_000,
            1_700_100_000,
        );
        assert!(!checkpoint_bounded.requested_range_complete);

        let complete = map_coverage(
            ConventionalFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            1_699_500_000,
            1_700_100_000,
        );
        assert!(complete.requested_range_complete);

        let starts_before_coverage = map_coverage(
            ConventionalFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(200),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            1_698_999_999,
            1_700_100_000,
        );
        assert!(!starts_before_coverage.requested_range_complete);

        let ends_after_lagging_coverage = map_coverage(
            ConventionalFeeDistributionBackfillCoverage::new(
                BlockHeight::new(1),
                BlockHeight::new(199),
                1_699_000_000,
                1_700_000_000,
            ),
            200,
            1_699_500_000,
            1_700_100_000,
        );
        assert!(!ends_after_lagging_coverage.requested_range_complete);
    }
}
