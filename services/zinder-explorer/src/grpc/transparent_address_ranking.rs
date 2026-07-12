//! `ExplorerQuery.TransparentAddressRanking` handler.
//!
//! Reads one bounded page from the atomically active ranking generation and
//! attaches the canonical wallet-plane epoch used for freshness reporting.

use tonic::{Request, Response, Status};
use zinder_derive::{
    DeriveStore, TransparentAddressRankingConsumer,
    TransparentAddressRankingCoverage as DerivedTransparentAddressRankingCoverage,
    TransparentAddressRankingEntry as DerivedTransparentAddressRankingEntry,
    TransparentAddressRankingPage,
};
use zinder_proto::capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1;
use zinder_proto::v1::explorer::{
    TransparentAddressRankingCoverage, TransparentAddressRankingEntry,
    TransparentAddressRankingRequest, TransparentAddressRankingResponse,
    TransparentAddressScriptTypeSummary, TransparentScriptType,
};
use zinder_proto::v1::wallet::{LatestBlockRequest, wallet_query_client::WalletQueryClient};
use zinder_runtime::AuthenticatedChannel;

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

const DEFAULT_RANKING_LIMIT: u32 = 100;
const MAX_RANKING_LIMIT: u32 = 500;

/// Executes one bounded transparent-address ranking read.
pub(crate) async fn handle_transparent_address_ranking(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<TransparentAddressRankingRequest>,
) -> Result<Response<TransparentAddressRankingResponse>, Status> {
    let request = request.into_inner();
    let limit = clamp_max_entries(request.limit, DEFAULT_RANKING_LIMIT, MAX_RANKING_LIMIT);
    let page = TransparentAddressRankingConsumer::page(
        derive_store,
        request.offset,
        usize::try_from(limit).unwrap_or(usize::MAX),
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?
    .ok_or_else(|| {
        ExplorerError::not_materialized(
            "transparent-address ranking has no active materialized generation",
        )
    })?;
    let chain_epoch = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner()
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("LatestBlockResponse.chain_view.chain_epoch missing")
        })?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(map_page(page, freshness)?))
}

fn map_page(
    page: TransparentAddressRankingPage,
    freshness: zinder_proto::v1::explorer::ExplorerFreshness,
) -> Result<TransparentAddressRankingResponse, Status> {
    let metadata = page.metadata;
    Ok(TransparentAddressRankingResponse {
        freshness: Some(freshness),
        entries: page
            .entries
            .into_iter()
            .map(map_entry)
            .collect::<Result<_, _>>()?,
        positive_address_count: metadata.positive_address_count,
        total_positive_balance_zat: metadata.total_positive_balance_zat,
        top_10_balance_zat: metadata.top_10_balance_zat,
        top_100_balance_zat: metadata.top_100_balance_zat,
        coverage: Some(map_coverage(metadata.coverage)),
        script_type_summaries: vec![
            TransparentAddressScriptTypeSummary {
                script_type: TransparentScriptType::P2pkh as i32,
                positive_address_count: metadata.p2pkh.positive_address_count,
                total_positive_balance_zat: metadata.p2pkh.total_positive_balance_zat,
            },
            TransparentAddressScriptTypeSummary {
                script_type: TransparentScriptType::P2sh as i32,
                positive_address_count: metadata.p2sh.positive_address_count,
                total_positive_balance_zat: metadata.p2sh.total_positive_balance_zat,
            },
        ],
    })
}

fn map_entry(
    entry: DerivedTransparentAddressRankingEntry,
) -> Result<TransparentAddressRankingEntry, Status> {
    let summary = entry.summary;
    let script_pub_key = summary.script_pub_key.ok_or_else(|| {
        ExplorerError::internal("ranked transparent-address summary has no script_pub_key")
    })?;
    Ok(TransparentAddressRankingEntry {
        rank: entry.rank,
        script_pub_key,
        balance_zat: summary.balance_zat,
        total_received_zat: Some(summary.total_received_zat),
        total_sent_zat: Some(summary.total_sent_zat),
        distinct_transaction_count: Some(summary.distinct_transaction_count),
        first_seen_unix_seconds: minimum_optional(
            summary.first_seen_unix_seconds,
            summary.snapshot_first_seen_unix_seconds,
        ),
        last_seen_unix_seconds: maximum_optional(
            summary.last_seen_unix_seconds,
            summary.snapshot_last_seen_unix_seconds,
        ),
    })
}

fn minimum_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timestamp), None) | (None, Some(timestamp)) => Some(timestamp),
        (None, None) => None,
    }
}

fn maximum_optional(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(timestamp), None) | (None, Some(timestamp)) => Some(timestamp),
        (None, None) => None,
    }
}

fn map_coverage(
    coverage: DerivedTransparentAddressRankingCoverage,
) -> TransparentAddressRankingCoverage {
    TransparentAddressRankingCoverage {
        balance_complete_through_height: coverage.balance_complete_through_height.value(),
        history_complete_from_height: coverage
            .history_complete_from_height
            .map(zinder_core::BlockHeight::value),
        history_complete_through_height: coverage
            .history_complete_through_height
            .map(zinder_core::BlockHeight::value),
        lifetime_statistics_complete: coverage.lifetime_statistics_complete,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use zinder_core::{BlockHeight, TransparentAddressScriptHash};
    use zinder_derive::{TransparentAddressRankingMetadata, TransparentAddressSummary};

    use super::*;

    fn ranking_page_fixture() -> TransparentAddressRankingPage {
        TransparentAddressRankingPage {
            entries: vec![DerivedTransparentAddressRankingEntry {
                rank: 3,
                address_script_hash: TransparentAddressScriptHash::from_bytes([7; 32]),
                summary: TransparentAddressSummary {
                    script_pub_key: Some(vec![0x76, 0xa9, 0x14]),
                    balance_zat: 90,
                    total_received_zat: 150,
                    total_sent_zat: 60,
                    distinct_transaction_count: 4,
                    first_seen_unix_seconds: Some(1_700_000_000),
                    last_seen_unix_seconds: Some(1_800_000_000),
                    snapshot_first_seen_unix_seconds: Some(1_600_000_000),
                    snapshot_last_seen_unix_seconds: Some(1_900_000_000),
                },
            }],
            metadata: TransparentAddressRankingMetadata {
                generation: 9,
                positive_address_count: 120,
                total_positive_balance_zat: 1_000,
                top_10_balance_zat: 600,
                top_100_balance_zat: 950,
                p2pkh: zinder_derive::TransparentAddressScriptTypeTotals {
                    positive_address_count: 80,
                    total_positive_balance_zat: 700,
                },
                p2sh: zinder_derive::TransparentAddressScriptTypeTotals {
                    positive_address_count: 40,
                    total_positive_balance_zat: 300,
                },
                coverage: DerivedTransparentAddressRankingCoverage {
                    balance_complete_through_height: BlockHeight::new(200),
                    history_complete_from_height: Some(BlockHeight::new(10)),
                    history_complete_through_height: Some(BlockHeight::new(200)),
                    lifetime_statistics_complete: false,
                },
            },
        }
    }

    #[test]
    fn ranking_page_mapping_preserves_order_aggregates_and_coverage() -> Result<(), Status> {
        let freshness = zinder_proto::v1::explorer::ExplorerFreshness {
            capability_version: EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1.to_owned(),
            ..Default::default()
        };
        let response = map_page(ranking_page_fixture(), freshness)?;

        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].rank, 3);
        assert_eq!(response.entries[0].script_pub_key, vec![0x76, 0xa9, 0x14]);
        assert_eq!(response.entries[0].balance_zat, 90);
        assert_eq!(response.entries[0].total_received_zat, Some(150));
        assert_eq!(response.entries[0].total_sent_zat, Some(60));
        assert_eq!(response.entries[0].distinct_transaction_count, Some(4));
        assert_eq!(
            response.entries[0].first_seen_unix_seconds,
            Some(1_600_000_000)
        );
        assert_eq!(
            response.entries[0].last_seen_unix_seconds,
            Some(1_900_000_000)
        );
        assert_eq!(response.positive_address_count, 120);
        assert_eq!(response.total_positive_balance_zat, 1_000);
        assert_eq!(response.top_10_balance_zat, 600);
        assert_eq!(response.top_100_balance_zat, 950);
        assert_eq!(response.script_type_summaries.len(), 2);
        assert_eq!(
            response.script_type_summaries[0],
            TransparentAddressScriptTypeSummary {
                script_type: TransparentScriptType::P2pkh as i32,
                positive_address_count: 80,
                total_positive_balance_zat: 700,
            }
        );
        assert_eq!(
            response.script_type_summaries[1],
            TransparentAddressScriptTypeSummary {
                script_type: TransparentScriptType::P2sh as i32,
                positive_address_count: 40,
                total_positive_balance_zat: 300,
            }
        );
        assert_eq!(
            response.coverage,
            Some(TransparentAddressRankingCoverage {
                balance_complete_through_height: 200,
                history_complete_from_height: Some(10),
                history_complete_through_height: Some(200),
                lifetime_statistics_complete: false,
            })
        );
        Ok(())
    }

    #[test]
    fn ranking_limit_uses_default_and_hard_cap() {
        assert_eq!(
            clamp_max_entries(0, DEFAULT_RANKING_LIMIT, MAX_RANKING_LIMIT),
            DEFAULT_RANKING_LIMIT
        );
        assert_eq!(
            clamp_max_entries(
                MAX_RANKING_LIMIT + 1,
                DEFAULT_RANKING_LIMIT,
                MAX_RANKING_LIMIT,
            ),
            MAX_RANKING_LIMIT
        );
    }

    #[test]
    fn ranked_entry_without_a_script_fails_closed() -> Result<(), Status> {
        let result = map_entry(DerivedTransparentAddressRankingEntry {
            rank: 1,
            address_script_hash: TransparentAddressScriptHash::from_bytes([8; 32]),
            summary: TransparentAddressSummary {
                script_pub_key: None,
                balance_zat: 1,
                total_received_zat: 1,
                total_sent_zat: 0,
                distinct_transaction_count: 1,
                first_seen_unix_seconds: None,
                last_seen_unix_seconds: None,
                snapshot_first_seen_unix_seconds: None,
                snapshot_last_seen_unix_seconds: None,
            },
        });

        let Err(error) = result else {
            return Err(Status::internal("ranked row without a script was accepted"));
        };
        assert_eq!(error.code(), tonic::Code::Internal);
        Ok(())
    }
}
