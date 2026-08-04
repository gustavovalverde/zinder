//! `ExplorerQuery` gRPC adapter.
//!
//! Serves [`ExplorerQuery::ServerInfo`] (advertising [`EXPLORER_SERVER_INFO_V1`])
//! and the materialized-view-backed explorer surfaces. Handlers that need canonical
//! wallet-plane reads (transaction detail, block views, search, mempool
//! activity, value pools) compose them through a `WalletQuery` channel.
//!
//! The adapter holds a single cached `WalletQuery` channel and reuses it
//! across requests. The first request that needs the channel pays the
//! handshake cost; later requests just clone the cached
//! [`AuthenticatedChannel`] (a `tonic` `Channel` is internally pooled and
//! clone-cheap) so the explorer never opens one HTTP/2 connection per
//! request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status, service::interceptor::InterceptedService};
use zinder_core::{Network, NetworkUpgradeActivations, wire::encode_zinder_native_chain_name};
use zinder_proto::capabilities::{
    CapabilitySurface, EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
    EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1, EXPLORER_PAID_FEE_DISTRIBUTION_V1,
    EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,
    EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2, EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
    ExplorerReadiness, capabilities_for_surface,
};
use zinder_proto::v1::{
    explorer::{
        BlockActivityDistributionRequest, BlockActivityDistributionResponse, BlockDetailRequest,
        BlockDetailResponse, BlockProductionInTimeRangeRequest, BlockProductionInTimeRangeResponse,
        BlockProductionSeriesRequest, BlockProductionSeriesResponse, BlockSummariesInRangeRequest,
        BlockSummariesInRangeResponse, BlockTransactionsResponse, ChainReorgHistoryRequest,
        ChainReorgHistoryResponse, CommitmentRootSearchRequest, CommitmentRootSearchResponse,
        ConventionalFeeDistributionRequest, ConventionalFeeDistributionResponse,
        DisplacedBlockDetailRequest, DisplacedBlockDetailResponse, DisplacedBlockHistoryRequest,
        DisplacedBlockHistoryResponse, ExplorerServerInfo, FeeSummaryRequest, FeeSummaryResponse,
        MempoolActivityRequest, MempoolActivityResponse, MempoolEventCountsRequest,
        MempoolEventCountsResponse, MempoolSnapshotRequest, MempoolSnapshotResponse,
        MempoolSummaryRequest, MempoolSummaryResponse, MigrationCohortsRequest,
        MigrationCohortsResponse, MigrationDenominationsRequest, MigrationDenominationsResponse,
        MigrationOverviewRequest, MigrationOverviewResponse, NetworkUpgradeStatusRequest,
        NetworkUpgradeStatusResponse, OverviewSnapshotRequest, OverviewSnapshotResponse,
        PaidFeeDistributionRequest, PaidFeeDistributionResponse, RecentTransactionsRequest,
        SearchRequest, SearchResponse, ServerInfoRequest, ServerInfoResponse,
        TransactionComponentSummaryRequest, TransactionComponentSummaryResponse,
        TransactionDetailRequest, TransactionDetailResponse, TransactionHistoryRequest,
        TransactionHistoryResponse, TransparentAddressActivityRequest,
        TransparentAddressActivityResponse, TransparentAddressDeltasRequest,
        TransparentAddressDeltasResponse, TransparentAddressRankingRequest,
        TransparentAddressRankingResponse, UtxoSetSummaryRequest, UtxoSetSummaryResponse,
        ValuePoolBalanceHistoryRequest, ValuePoolBalanceHistoryResponse,
        ValuePoolFlowAmountThresholdSummaryRequest, ValuePoolFlowAmountThresholdSummaryResponse,
        ValuePoolFlowEventsInRangeRequest, ValuePoolFlowEventsInRangeResponse,
        ValuePoolFlowHistoryRequest, ValuePoolFlowHistoryResponse,
        ValuePoolFlowRoundedAmountSummaryRequest, ValuePoolFlowRoundedAmountSummaryResponse,
        ValuePoolFlowSummaryRequest, ValuePoolFlowSummaryResponse, ValuePoolSummaryRequest,
        ValuePoolSummaryResponse,
        explorer_query_server::{ExplorerQuery, ExplorerQueryServer},
    },
    ops,
    wallet::wallet_query_client::WalletQueryClient,
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenServerInterceptor, RpcMetricNames, RpcOutcome,
    describe_rpc_metrics, record_rpc_request,
};
use zinder_source::NodeSource;

/// Metric pair the `ExplorerQuery` adapter emits per request.
const EXPLORER_RPC_METRICS: RpcMetricNames = RpcMetricNames::for_service(
    "zinder_explorer_request_duration_seconds",
    "zinder_explorer_request_total",
);

/// First canonical artifact schema that contains transaction-intrinsic balances.
const MINIMUM_INTRINSIC_VALUE_BALANCE_HISTORY_SCHEMA_VERSION: u16 = 15;
use super::block_activity::query_block_activity_distribution;
use super::block_view::{
    query_block_detail, query_block_production_in_time_range, query_block_production_series,
    query_block_summaries_in_range, query_block_transactions,
};
use super::chain_reorg_history::query_chain_reorg_history;
use super::commitment_root_search::query_commitment_root_search;
use super::conventional_fee_distribution::query_conventional_fee_distribution;
use super::displaced_block::{query_displaced_block_detail, query_displaced_block_history};
use super::endpoint_admission::{AdmittedWalletQueryEndpoint, ExplorerEndpointAdmissionError};
use super::error::ExplorerError;
use super::fee_summary::query_fee_summary;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
    read_materialized_view_status, spawn_upstream_observation_probe_task,
};
use super::mempool::{query_mempool_activity, query_mempool_snapshot, query_mempool_summary};
use super::mempool_event_counts::query_mempool_event_counts;
use super::migration::{
    query_migration_cohorts, query_migration_denominations, query_migration_overview,
};
use super::network_upgrade_status::query_network_upgrade_status;
use super::overview_snapshot::query_overview_snapshot;
use super::paid_fee_distribution::query_paid_fee_distribution;
use super::recent_transactions::{RecentTransactionsStream, query_recent_transactions};
use super::search::query_search;
use super::transaction_component_summary::query_transaction_component_summary;
use super::transaction_detail::{TransactionDetailContext, query_transaction_detail};
use super::transaction_history::{
    TransactionHistoryContext, TransactionHistoryMaterializedViewReadApi,
    TransactionHistoryMaterializedViewReadError, TransactionHistoryMaterializedViewReader,
    TransactionHistoryMaterializedViewReadiness, transaction_history,
};
use super::transparent_address_activity::{
    TransparentAddressActivityContext, query_transparent_address_activity,
};
use super::transparent_address_deltas::query_transparent_address_deltas;
use super::transparent_address_ranking::query_transparent_address_ranking;
use super::utxo_set_summary::query_utxo_set_summary;
use super::value_pool_balance_history::query_value_pool_balance_history;
use super::value_pool_flow::{
    query_value_pool_flow_amount_threshold_summary, query_value_pool_flow_events_in_range,
    query_value_pool_flow_history, query_value_pool_flow_rounded_amount_summary,
    query_value_pool_flow_summary,
};
use super::value_pool_summary::query_value_pool_summary;
use zinder_materialized_views::{MaterializedViewPreset, MaterializedViewStore};
use zinder_store::SecondaryChainStore;

/// Settings the binary populates before constructing the adapter.
#[derive(Clone, Copy, Debug)]
pub struct ExplorerServerInfoSettings {
    /// Network the consumer mirrors.
    pub network: Network,
}

impl Default for ExplorerServerInfoSettings {
    fn default() -> Self {
        Self {
            network: Network::ZcashRegtest,
        }
    }
}

/// Fully admitted Explorer composition prior to exposing a listener.
///
/// The operator-built Explorer composition freezes one Wallet endpoint and
/// one materialized-view identity as its row-serving authority. Optional Node
/// configuration is deliberately absent: it is freshness observation only.
#[derive(Clone)]
pub struct ExplorerQueryEndpointComposition {
    settings: ExplorerServerInfoSettings,
    materialized_view_store: MaterializedViewStore,
    wallet_query_endpoint: String,
    wallet_query_bearer_token: Option<BearerToken>,
    bearer_token: Option<BearerToken>,
    canonical_store: Option<SecondaryChainStore>,
    prevout_resolution_online: bool,
}

impl ExplorerQueryEndpointComposition {
    /// Starts a composition that cannot serve until Wallet admission succeeds.
    #[must_use]
    pub fn new(
        settings: ExplorerServerInfoSettings,
        materialized_view_store: MaterializedViewStore,
        wallet_query_endpoint: String,
    ) -> Self {
        Self {
            settings,
            materialized_view_store,
            wallet_query_endpoint,
            wallet_query_bearer_token: None,
            bearer_token: None,
            canonical_store: None,
            prevout_resolution_online: false,
        }
    }

    /// Wires an optional canonical secondary for handlers that need it.
    #[must_use]
    pub fn with_canonical_store(mut self, store: SecondaryChainStore) -> Self {
        self.canonical_store = Some(store);
        self
    }

    /// Attaches the optional bearer token sent to the admitted Wallet endpoint.
    #[must_use]
    pub fn with_wallet_query_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.wallet_query_bearer_token = Some(bearer_token);
        self
    }

    /// Attaches the optional bearer token enforced on Explorer traffic.
    #[must_use]
    pub fn with_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.bearer_token = Some(bearer_token);
        self
    }

    /// Declares that the local materialized-view workload resolves prevouts.
    #[must_use]
    pub const fn with_prevout_resolution_online(mut self, online: bool) -> Self {
        self.prevout_resolution_online = online;
        self
    }

    /// Admits the mandatory Wallet and materialized-view construction identity.
    pub async fn compose(self) -> Result<ExplorerQueryGrpcAdapter, ExplorerEndpointAdmissionError> {
        let wallet_endpoint = AdmittedWalletQueryEndpoint::admit(
            &self.wallet_query_endpoint,
            self.wallet_query_bearer_token.as_ref(),
            self.settings.network,
        )
        .await?;
        let materialized_view_identity = self.materialized_view_store.construction_identity();
        wallet_endpoint.require_matching_materialized_view_identity(materialized_view_identity)?;
        let network_upgrade_activations = wallet_endpoint
            .network_upgrade_activations(materialized_view_identity)
            .await?;
        let materialized_view_preset = self
            .materialized_view_store
            .effective_materialized_view_preset();
        let transaction_history_materialized_view_reader = Arc::new(
            TransactionHistoryMaterializedViewReader::new(self.materialized_view_store.clone()),
        );

        Ok(ExplorerQueryGrpcAdapter {
            settings: self.settings,
            wallet_endpoint,
            bearer_token: self.bearer_token,
            canonical_store: self.canonical_store,
            materialized_view_store: self.materialized_view_store,
            materialized_view_preset,
            transaction_history_materialized_view_reader,
            network_upgrade_activations,
            prevout_resolution_online: self.prevout_resolution_online,
            upstream_observation_cache: UpstreamObservationCache::empty(),
        })
    }
}

/// Server adapter implementing `ExplorerQuery` for `zinder-explorer`.
///
/// Construct it only through [`ExplorerQueryEndpointComposition::compose`],
/// which admits the Wallet endpoint and materialized-view identity before any
/// Explorer metadata or row read becomes reachable.
#[derive(Clone)]
pub struct ExplorerQueryGrpcAdapter {
    settings: ExplorerServerInfoSettings,
    wallet_endpoint: AdmittedWalletQueryEndpoint,
    bearer_token: Option<BearerToken>,
    canonical_store: Option<SecondaryChainStore>,
    materialized_view_store: MaterializedViewStore,
    materialized_view_preset: MaterializedViewPreset,
    transaction_history_materialized_view_reader:
        Arc<dyn TransactionHistoryMaterializedViewReadApi>,
    network_upgrade_activations: Option<NetworkUpgradeActivations>,
    prevout_resolution_online: bool,
    upstream_observation_cache: UpstreamObservationCache,
}

impl ExplorerQueryGrpcAdapter {
    /// Rechecks the frozen Wallet endpoint evidence without rediscovery.
    pub async fn check_wallet_endpoint_health(
        &self,
    ) -> Result<(), super::endpoint_admission::ExplorerWalletQueryHealthError> {
        self.wallet_endpoint.check_health().await
    }

    /// Spawns the background task that refreshes the cached
    /// [`zinder_source::UpstreamHealthSnapshot`] every `poll_interval`
    /// and seeds the shared cache the freshness builders read.
    ///
    /// Returns the spawned [`JoinHandle`]: the caller (binary entry point)
    /// awaits or drops it during shutdown. Without this call the cache
    /// stays empty and every `chain_view.upstream_tip` axis is `None`;
    /// that is the documented "probe has not fired yet" state per
    /// ADR-0011.
    #[must_use]
    pub fn spawn_upstream_observation_probe<Source>(
        &self,
        source: Arc<Source>,
        poll_interval: Duration,
        cancel: CancellationToken,
    ) -> JoinHandle<()>
    where
        Source: NodeSource + 'static,
    {
        spawn_upstream_observation_probe_task(
            source,
            self.upstream_observation_cache.clone(),
            poll_interval,
            cancel,
        )
    }

    /// Wraps the adapter into a tonic [`ExplorerQueryServer`] ready to be
    /// added to a `tonic::transport::Server` builder.
    #[must_use]
    pub fn into_server(
        self,
    ) -> InterceptedService<ExplorerQueryServer<Self>, BearerTokenServerInterceptor> {
        let interceptor = BearerTokenServerInterceptor::new(self.bearer_token.clone());
        let server = ExplorerQueryServer::new(self)
            .max_decoding_message_size(zinder_runtime::MAX_DECODING_MESSAGE_BYTES);
        InterceptedService::new(server, interceptor)
    }

    fn advertised_capability_readiness(&self) -> ExplorerReadiness {
        let wallet_query_online = true;
        let transaction_history_readiness = self
            .transaction_history_materialized_view_reader
            .readiness()
            .ok();
        let canonical_transaction_history_position =
            self.canonical_store.as_ref().and_then(|store| {
                store.try_catch_up().ok()?;
                let chain_epoch = store.current_chain_epoch().ok().flatten()?;
                Some((
                    chain_epoch.id,
                    chain_epoch.visible_tip_height,
                    chain_epoch.visible_tip_hash,
                ))
            });

        ExplorerReadiness {
            wallet_query_online,
            canonical_store_online: self.canonical_store.is_some(),
            materialized_view_store_online: self.materialized_view_preset
                == MaterializedViewPreset::Explorer,
            prevout_resolution_online: self.prevout_resolution_online && wallet_query_online,
            transaction_history_available: transaction_history_readiness
                .is_some_and(TransactionHistoryMaterializedViewReadiness::is_available),
            transaction_history_complete: transaction_history_readiness.is_some_and(|readiness| {
                readiness.is_complete_at(canonical_transaction_history_position)
            }),
        }
    }

    /// Returns the capability strings the adapter currently advertises.
    ///
    /// Single source of truth for capability gating: `ServerInfo`, the ops
    /// endpoint `/healthz`, and any future advertisement surface all read
    /// from this method so a flag flip in one place reaches every consumer.
    /// Per ADR-0018, each capability lights up only when the upstream
    /// state it depends on is satisfied; the adapter never advertises a
    /// capability whose handler would return `Unavailable`.
    #[must_use]
    pub fn advertised_capabilities(&self) -> Vec<&'static str> {
        let readiness = self.advertised_capability_readiness();
        let ranking_active =
            zinder_materialized_views::TransparentAddressRankingConsumer::active_metadata(
                &self.materialized_view_store,
            )
            .ok()
            .flatten()
            .is_some();
        let conventional_fee_distribution_covered =
            zinder_materialized_views::ConventionalFeeDistributionConsumer::coverage(
                &self.materialized_view_store,
            )
            .ok()
            .flatten()
            .is_some();
        let paid_fee_distribution_covered =
            zinder_materialized_views::PaidFeeDistributionConsumer::coverage(
                &self.materialized_view_store,
            )
            .ok()
            .flatten()
            .is_some();
        let block_production_time_covered =
            block_production_time_materialized_view_available(Some(&self.materialized_view_store));
        let transaction_intrinsic_value_balances_available =
            self.canonical_store.as_ref().is_some_and(|store| {
                store.try_catch_up().is_ok()
                    && store.current_chain_epoch().is_ok_and(|chain_epoch| {
                        chain_epoch.is_some_and(intrinsic_value_balance_schema_supported)
                    })
            });
        capabilities_for_surface(CapabilitySurface::Explorer)
            .filter(|spec| spec.policy.explorer_satisfied(readiness))
            .filter(|spec| {
                !matches!(
                    spec.string,
                    EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2
                        | EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1
                ) || ranking_active
            })
            .filter(|spec| {
                spec.string != EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1
                    || conventional_fee_distribution_covered
            })
            .filter(|spec| {
                spec.string != EXPLORER_PAID_FEE_DISTRIBUTION_V1
                    || (paid_fee_distribution_covered && readiness.canonical_store_online)
            })
            .filter(|spec| {
                spec.string != EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1
                    || block_production_time_covered
            })
            .filter(|spec| {
                spec.string != EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1
                    || transaction_intrinsic_value_balances_available
            })
            .map(|spec| spec.string)
            .collect()
    }
}

fn block_production_time_materialized_view_available(
    materialized_view_store: Option<&MaterializedViewStore>,
) -> bool {
    let Some(store) = materialized_view_store else {
        return false;
    };
    let Ok(snapshot) = store.read_snapshot() else {
        return false;
    };
    let coverage =
        zinder_materialized_views::BlockProductionTimeConsumer::coverage_snapshot(&snapshot)
            .ok()
            .flatten();
    let materialized_view_state = snapshot
        .consumer_state(zinder_materialized_views::BLOCK_PRODUCTION_TIME_CONSUMER_NAME)
        .ok()
        .flatten();
    drop(snapshot);
    coverage
        .zip(materialized_view_state)
        .is_some_and(|(coverage, materialized_view_state)| {
            coverage.complete_from_height.value() <= 1
                && coverage.complete_through_height >= materialized_view_state.tip_height
        })
}

fn intrinsic_value_balance_schema_supported(chain_epoch: zinder_core::ChainEpoch) -> bool {
    chain_epoch.artifact_schema_version.value()
        >= MINIMUM_INTRINSIC_VALUE_BALANCE_HISTORY_SCHEMA_VERSION
}

/// Pair of operation labels used by every `ExplorerQuery` handler.
///
/// `method` mirrors the proto method name in `Status` text so operators see
/// the same identifier the generated `WalletQueryClient` would use. `metric`
/// is the `snake_case` Prometheus label so the `zinder_explorer_request_*`
/// series follows the recorder's naming convention. Colocating both in a
/// single `const` per handler stops the two forms from drifting apart.
struct OperationNames {
    method: &'static str,
    metric: &'static str,
}

#[tonic::async_trait]
impl ExplorerQuery for ExplorerQueryGrpcAdapter {
    async fn server_info(
        &self,
        _request: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        // ServerInfo carries the standard ExplorerFreshness envelope per
        // ADR-0011 so consumers can read `chain_view.upstream_tip` from the
        // bootstrap call, before any materialized-view-backed capability is up.
        // During `bulk_catchup` this is the only explorer response that
        // is guaranteed to succeed; sync-progress UIs depend on it for
        // an honest denominator. `chain_epoch` is left unset here because
        // ServerInfo resolves no canonical follower tip and so makes no
        // snapshot-consistency claim; the materialized-view indexed tip and the
        // upstream observation carry the rest of the freshness signal.
        let freshness = attach_upstream_observation(
            &self.upstream_observation_cache,
            build_explorer_freshness(
                Some(&self.materialized_view_store),
                EXPLORER_SERVER_INFO_V1,
                None,
                0,
            )?,
        )
        .await;
        let materialized_view_status =
            read_materialized_view_status(Some(&self.materialized_view_store))?;
        Ok(Response::new(ServerInfoResponse {
            freshness: Some(freshness),
            info: Some(ExplorerServerInfo {
                common: Some(ops::ServerInfo {
                    network: encode_zinder_native_chain_name(self.settings.network).to_owned(),
                    service_name: env!("CARGO_PKG_NAME").to_owned(),
                    service_version: env!("CARGO_PKG_VERSION").to_owned(),
                    build_git_commit: zinder_runtime::BUILD_GIT_COMMIT.to_owned(),
                    capabilities: self
                        .advertised_capabilities()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    contract_revision: zinder_proto::CONTRACT_REVISION,
                    materialized_view_preset: self.materialized_view_preset.as_str().to_owned(),
                    materialized_view_identities: self
                        .materialized_view_preset
                        .consumer_schemas()
                        .iter()
                        .map(|schema| schema.name.as_str().to_owned())
                        .collect(),
                }),
                vendor: "Zinder".to_owned(),
                materialized_view_status,
            }),
        }))
    }

    async fn transaction_detail(
        &self,
        request: Request<TransactionDetailRequest>,
    ) -> Result<Response<TransactionDetailResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransactionDetail",
            metric: "transaction_detail",
        };
        let started = Instant::now();
        let outcome = async {
            let network_upgrade_activations =
                self.require_admitted_network_upgrade_activations(OP.method)?;
            let mut client = self.wallet_client();
            query_transaction_detail(
                &mut client,
                TransactionDetailContext {
                    chain_store: self.canonical_store.as_ref(),
                    materialized_view_store: Some(&self.materialized_view_store),
                    network: self.settings.network,
                    network_upgrade_activations,
                    upstream_observation_cache: &self.upstream_observation_cache,
                },
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn block_summaries_in_range(
        &self,
        request: Request<BlockSummariesInRangeRequest>,
    ) -> Result<Response<BlockSummariesInRangeResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "BlockSummariesInRange",
            metric: "block_summaries_in_range",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_block_summaries_in_range(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn block_production_series(
        &self,
        request: Request<BlockProductionSeriesRequest>,
    ) -> Result<Response<BlockProductionSeriesResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "BlockProductionSeries",
            metric: "block_production_series",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            query_block_production_series(
                materialized_view_store,
                canonical_store,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn block_production_in_time_range(
        &self,
        request: Request<BlockProductionInTimeRangeRequest>,
    ) -> Result<Response<BlockProductionInTimeRangeResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "BlockProductionInTimeRange",
            metric: "block_production_in_time_range",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            query_block_production_in_time_range(
                materialized_view_store,
                canonical_store,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn block_activity_distribution(
        &self,
        request: Request<BlockActivityDistributionRequest>,
    ) -> Result<Response<BlockActivityDistributionResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "BlockActivityDistribution",
            metric: "block_activity_distribution",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_block_activity_distribution(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn transaction_component_summary(
        &self,
        request: Request<TransactionComponentSummaryRequest>,
    ) -> Result<Response<TransactionComponentSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransactionComponentSummary",
            metric: "transaction_component_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_transaction_component_summary(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn transparent_address_ranking(
        &self,
        request: Request<TransparentAddressRankingRequest>,
    ) -> Result<Response<TransparentAddressRankingResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransparentAddressRanking",
            metric: "transparent_address_ranking",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_transparent_address_ranking(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn block_detail(
        &self,
        request: Request<BlockDetailRequest>,
    ) -> Result<Response<BlockDetailResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "BlockDetail",
            metric: "block_detail",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_block_detail(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn block_transactions(
        &self,
        request: Request<BlockDetailRequest>,
    ) -> Result<Response<BlockTransactionsResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "BlockTransactions",
            metric: "block_transactions",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            let mut client = self.wallet_client();
            query_block_transactions(
                canonical_store,
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<SearchResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "Search",
            metric: "search",
        };
        let started = Instant::now();
        let outcome = async {
            let mut client = self.wallet_client();
            query_search(
                Some(&self.materialized_view_store),
                &mut client,
                self.settings.network,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn commitment_root_search(
        &self,
        request: Request<CommitmentRootSearchRequest>,
    ) -> Result<Response<CommitmentRootSearchResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "CommitmentRootSearch",
            metric: "commitment_root_search",
        };
        let started = Instant::now();
        let outcome = async {
            let network_upgrade_activations =
                self.require_admitted_network_upgrade_activations(OP.method)?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            query_commitment_root_search(
                materialized_view_store,
                canonical_store,
                network_upgrade_activations,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn mempool_summary(
        &self,
        request: Request<MempoolSummaryRequest>,
    ) -> Result<Response<MempoolSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MempoolSummary",
            metric: "mempool_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let mut client = self.wallet_client();
            query_mempool_summary(
                Some(&self.materialized_view_store),
                &mut client,
                self.settings.network,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn mempool_snapshot(
        &self,
        request: Request<MempoolSnapshotRequest>,
    ) -> Result<Response<MempoolSnapshotResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MempoolSnapshot",
            metric: "mempool_snapshot",
        };
        let started = Instant::now();
        let outcome = async {
            let mut client = self.wallet_client();
            query_mempool_snapshot(
                Some(&self.materialized_view_store),
                &mut client,
                self.settings.network,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn mempool_activity(
        &self,
        request: Request<MempoolActivityRequest>,
    ) -> Result<Response<MempoolActivityResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MempoolActivity",
            metric: "mempool_activity",
        };
        let started = Instant::now();
        let outcome = async {
            let mut client = self.wallet_client();
            query_mempool_activity(
                Some(&self.materialized_view_store),
                &mut client,
                self.settings.network,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn transparent_address_activity(
        &self,
        request: Request<TransparentAddressActivityRequest>,
    ) -> Result<Response<TransparentAddressActivityResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransparentAddressActivity",
            metric: "transparent_address_activity",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = &self.materialized_view_store;
            let mut client = self.wallet_client();
            query_transparent_address_activity(
                TransparentAddressActivityContext {
                    materialized_view_store,
                    canonical_store: self.canonical_store.as_ref(),
                    network: self.settings.network,
                    upstream_observation_cache: &self.upstream_observation_cache,
                },
                &mut client,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn transparent_address_deltas(
        &self,
        request: Request<TransparentAddressDeltasRequest>,
    ) -> Result<Response<TransparentAddressDeltasResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransparentAddressDeltas",
            metric: "transparent_address_deltas",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = &self.materialized_view_store;
            let mut client = self.wallet_client();
            query_transparent_address_deltas(
                materialized_view_store,
                &mut client,
                self.settings.network,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn fee_summary(
        &self,
        request: Request<FeeSummaryRequest>,
    ) -> Result<Response<FeeSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "FeeSummary",
            metric: "fee_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_fee_summary(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn conventional_fee_distribution(
        &self,
        request: Request<ConventionalFeeDistributionRequest>,
    ) -> Result<Response<ConventionalFeeDistributionResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ConventionalFeeDistribution",
            metric: "conventional_fee_distribution",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_conventional_fee_distribution(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn paid_fee_distribution(
        &self,
        request: Request<PaidFeeDistributionRequest>,
    ) -> Result<Response<PaidFeeDistributionResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "PaidFeeDistribution",
            metric: "paid_fee_distribution",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            let mut client = self.wallet_client();
            query_paid_fee_distribution(
                materialized_view_store,
                canonical_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_summary(
        &self,
        request: Request<ValuePoolSummaryRequest>,
    ) -> Result<Response<ValuePoolSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolSummary",
            metric: "value_pool_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let mut client = self.wallet_client();
            query_value_pool_summary(
                Some(&self.materialized_view_store),
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn network_upgrade_status(
        &self,
        request: Request<NetworkUpgradeStatusRequest>,
    ) -> Result<Response<NetworkUpgradeStatusResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "NetworkUpgradeStatus",
            metric: "network_upgrade_status",
        };
        let started = Instant::now();
        let outcome = async {
            let network_upgrade_activations =
                self.network_upgrade_activations.as_ref().ok_or_else(|| {
                    ExplorerError::dependency_not_configured(
                        "NetworkUpgradeStatus requires admitted Wallet activation evidence",
                    )
                })?;
            let mut client = self.wallet_client();
            query_network_upgrade_status(
                Some(&self.materialized_view_store),
                network_upgrade_activations,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_flow_history(
        &self,
        request: Request<ValuePoolFlowHistoryRequest>,
    ) -> Result<Response<ValuePoolFlowHistoryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolFlowHistory",
            metric: "value_pool_flow_history",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_value_pool_flow_history(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_flow_events_in_range(
        &self,
        request: Request<ValuePoolFlowEventsInRangeRequest>,
    ) -> Result<Response<ValuePoolFlowEventsInRangeResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolFlowEventsInRange",
            metric: "value_pool_flow_events_in_range",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_value_pool_flow_events_in_range(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_flow_summary(
        &self,
        request: Request<ValuePoolFlowSummaryRequest>,
    ) -> Result<Response<ValuePoolFlowSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolFlowSummary",
            metric: "value_pool_flow_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_value_pool_flow_summary(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_flow_amount_threshold_summary(
        &self,
        request: Request<ValuePoolFlowAmountThresholdSummaryRequest>,
    ) -> Result<Response<ValuePoolFlowAmountThresholdSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolFlowAmountThresholdSummary",
            metric: "value_pool_flow_amount_threshold_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_value_pool_flow_amount_threshold_summary(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_flow_rounded_amount_summary(
        &self,
        request: Request<ValuePoolFlowRoundedAmountSummaryRequest>,
    ) -> Result<Response<ValuePoolFlowRoundedAmountSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolFlowRoundedAmountSummary",
            metric: "value_pool_flow_rounded_amount_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_value_pool_flow_rounded_amount_summary(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn value_pool_balance_history(
        &self,
        request: Request<ValuePoolBalanceHistoryRequest>,
    ) -> Result<Response<ValuePoolBalanceHistoryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ValuePoolBalanceHistory",
            metric: "value_pool_balance_history",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_value_pool_balance_history(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn utxo_set_summary(
        &self,
        request: Request<UtxoSetSummaryRequest>,
    ) -> Result<Response<UtxoSetSummaryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "UtxoSetSummary",
            metric: "utxo_set_summary",
        };
        let started = Instant::now();
        let outcome = async {
            let mut client = self.wallet_client();
            query_utxo_set_summary(
                Some(&self.materialized_view_store),
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn chain_reorg_history(
        &self,
        request: Request<ChainReorgHistoryRequest>,
    ) -> Result<Response<ChainReorgHistoryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "ChainReorgHistory",
            metric: "chain_reorg_history",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = &self.materialized_view_store;
            query_chain_reorg_history(
                materialized_view_store,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn displaced_block_history(
        &self,
        request: Request<DisplacedBlockHistoryRequest>,
    ) -> Result<Response<DisplacedBlockHistoryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "DisplacedBlockHistory",
            metric: "displaced_block_history",
        };
        let started = Instant::now();
        let outcome = async {
            let canonical_store = self.require_canonical_store(OP.method)?;
            query_displaced_block_history(
                canonical_store,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn displaced_block_detail(
        &self,
        request: Request<DisplacedBlockDetailRequest>,
    ) -> Result<Response<DisplacedBlockDetailResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "DisplacedBlockDetail",
            metric: "displaced_block_detail",
        };
        let started = Instant::now();
        let outcome = async {
            let canonical_store = self.require_canonical_store(OP.method)?;
            query_displaced_block_detail(canonical_store, &self.upstream_observation_cache, request)
                .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn mempool_event_counts(
        &self,
        request: Request<MempoolEventCountsRequest>,
    ) -> Result<Response<MempoolEventCountsResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MempoolEventCounts",
            metric: "mempool_event_counts",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = &self.materialized_view_store;
            let mut client = self.wallet_client();
            query_mempool_event_counts(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn transaction_history(
        &self,
        request: Request<TransactionHistoryRequest>,
    ) -> Result<Response<TransactionHistoryResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransactionHistory",
            metric: "transaction_history",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_reader =
                self.require_transaction_history_materialized_view_reader();
            materialized_view_reader
                .readiness()
                .and_then(TransactionHistoryMaterializedViewReadiness::require_available)
                .map_err(TransactionHistoryMaterializedViewReadError::into_status)?;
            let mut client = self.wallet_client();
            transaction_history(
                TransactionHistoryContext {
                    materialized_view_reader: Arc::clone(materialized_view_reader),
                    materialized_view_store: Some(&self.materialized_view_store),
                    chain_store: self.canonical_store.as_ref(),
                    upstream_observation_cache: &self.upstream_observation_cache,
                },
                &mut client,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    type RecentTransactionsStream = RecentTransactionsStream;

    async fn recent_transactions(
        &self,
        request: Request<RecentTransactionsRequest>,
    ) -> Result<Response<Self::RecentTransactionsStream>, Status> {
        const OP: OperationNames = OperationNames {
            method: "RecentTransactions",
            metric: "recent_transactions",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_recent_transactions(
                materialized_view_store,
                self.canonical_store.as_ref(),
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn overview_snapshot(
        &self,
        request: Request<OverviewSnapshotRequest>,
    ) -> Result<Response<OverviewSnapshotResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "OverviewSnapshot",
            metric: "overview_snapshot",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_overview_snapshot(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn migration_overview(
        &self,
        request: Request<MigrationOverviewRequest>,
    ) -> Result<Response<MigrationOverviewResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MigrationOverview",
            metric: "migration_overview",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_migration_overview(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn migration_cohorts(
        &self,
        request: Request<MigrationCohortsRequest>,
    ) -> Result<Response<MigrationCohortsResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MigrationCohorts",
            metric: "migration_cohorts",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_migration_cohorts(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }

    async fn migration_denominations(
        &self,
        request: Request<MigrationDenominationsRequest>,
    ) -> Result<Response<MigrationDenominationsResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "MigrationDenominations",
            metric: "migration_denominations",
        };
        let started = Instant::now();
        let outcome = async {
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client();
            query_migration_denominations(
                materialized_view_store,
                &mut client,
                &self.upstream_observation_cache,
                request,
            )
            .await
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }
}

impl ExplorerQueryGrpcAdapter {
    fn require_transaction_history_materialized_view_reader(
        &self,
    ) -> &Arc<dyn TransactionHistoryMaterializedViewReadApi> {
        &self.transaction_history_materialized_view_reader
    }

    fn require_materialized_view_store(
        &self,
        method: &'static str,
    ) -> Result<&MaterializedViewStore, Status> {
        if self.materialized_view_preset != MaterializedViewPreset::Explorer {
            return Err(ExplorerError::unsupported(format!(
                "{method} is unavailable because the stored materialized-view workload omits \
                 explorer product views"
            ))
            .into());
        }
        Ok(&self.materialized_view_store)
    }

    fn require_canonical_store(
        &self,
        method: &'static str,
    ) -> Result<&SecondaryChainStore, Status> {
        self.canonical_store.as_ref().ok_or_else(|| {
            ExplorerError::dependency_not_configured(format!(
                "{method} requires the canonical store; configure --storage-path"
            ))
            .into()
        })
    }

    /// Returns a client over the channel admitted before Explorer traffic binds.
    fn wallet_client(&self) -> WalletQueryClient<AuthenticatedChannel> {
        self.wallet_endpoint.wallet_client()
    }

    /// Returns the sole activation authority admitted at startup.
    ///
    /// Handlers that decode protocol-dependent data must refuse traffic before
    /// parsing or storage access if Wallet did not advertise and supply this
    /// evidence. Synthesizing an empty table would make a local assumption look
    /// like Wallet authority.
    fn require_admitted_network_upgrade_activations(
        &self,
        method: &'static str,
    ) -> Result<&NetworkUpgradeActivations, Status> {
        self.network_upgrade_activations.as_ref().ok_or_else(|| {
            ExplorerError::dependency_not_configured(format!(
                "{method} requires admitted Wallet network-upgrade activation evidence"
            ))
            .into()
        })
    }
}

/// Registers `# HELP` and `# TYPE` text for every metric this module emits.
///
/// Call once at startup, after `install_metrics_recorder` returns and before
/// the gRPC server records its first request. Delegates to
/// [`describe_rpc_metrics`] so every Zinder service shares one description
/// template.
pub fn describe_request_metrics() {
    describe_rpc_metrics(EXPLORER_RPC_METRICS, "ExplorerQuery");
}

/// Records per-RPC duration + status counters via
/// [`record_rpc_request`].
///
/// Explorer handlers return [`tonic::Status`] directly, so the
/// `error_class` vocabulary is the tonic [`Code`] name. When the explorer
/// grows a typed domain-error type, swap [`status_error_class`] for a
/// domain-aware mapper to keep labels aligned with operator dashboards.
fn record_explorer_request(operation: &'static str, elapsed: Duration, error: Option<&Status>) {
    let outcome = error.map_or(RpcOutcome::Ok, |status| RpcOutcome::Error {
        class: status_error_class(status),
    });
    record_rpc_request(EXPLORER_RPC_METRICS, operation, elapsed, outcome);
}

/// Maps a `tonic::Status::code()` to the short string label operational
/// dashboards filter on. Mirrors `zinder-query`'s `query_error_class` so the
/// query boundaries share one vocabulary.
fn status_error_class(status: &Status) -> &'static str {
    match status.code() {
        Code::Ok => "none",
        Code::Cancelled => "cancelled",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
        Code::Unknown => "unknown",
    }
}
