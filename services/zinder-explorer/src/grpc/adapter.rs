//! `ExplorerQuery` gRPC adapter.
//!
//! Serves [`ExplorerQuery::ServerInfo`] (advertising [`EXPLORER_SERVER_INFO_V1`])
//! and the materialized-view-backed explorer surfaces. Handlers that need canonical
//! wallet-plane reads (transaction detail, block views, search, mempool
//! activity, value pools) compose them through a `WalletQuery` channel.
//!
//! The builder admits an optional `WalletQuery` dependency before producing
//! the adapter. The finalized adapter retains only the admitted channel and
//! one immutable capability set shared by discovery and operational surfaces.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status, service::interceptor::InterceptedService};
use zinder_core::{Network, NetworkUpgradeActivations, wire::encode_zinder_native_chain_name};
use zinder_proto::capabilities::{self, EXPLORER_SERVER_INFO_V1};
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
    BearerToken, BearerTokenServerInterceptor, RpcMetricNames, RpcOutcome, describe_rpc_metrics,
    record_rpc_request,
};
use zinder_source::NodeSource;

/// Metric pair the `ExplorerQuery` adapter emits per request.
const EXPLORER_RPC_METRICS: RpcMetricNames = RpcMetricNames::for_service(
    "zinder_explorer_request_duration_seconds",
    "zinder_explorer_request_total",
);

use super::block_activity::query_block_activity_distribution;
use super::block_view::{
    BlockTransactionsContext, query_block_detail, query_block_production_in_time_range,
    query_block_production_series, query_block_summaries_in_range, query_block_transactions,
};
use super::chain_reorg_history::query_chain_reorg_history;
use super::commitment_root_search::{CommitmentRootSearchContext, query_commitment_root_search};
use super::conventional_fee_distribution::query_conventional_fee_distribution;
use super::displaced_block::{query_displaced_block_detail, query_displaced_block_history};
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
use super::recent_transactions::{
    RecentTransactionsContext, RecentTransactionsStream, query_recent_transactions,
};
use super::search::query_search;
use super::transaction_component_summary::query_transaction_component_summary;
use super::transaction_detail::{TransactionDetailContext, query_transaction_detail};
use super::transaction_history::{
    TransactionHistoryContext, TransactionHistoryMaterializedViewReadApi,
    TransactionHistoryMaterializedViewReader, transaction_history,
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
use super::{
    endpoint_admission::{
        AdmittedWalletQueryEndpoint, ExplorerEndpointAdmissionError, ExplorerWalletQueryHealthError,
    },
    endpoint_capabilities::ExplorerEndpointCapabilities,
};
use zinder_materialized_views::{
    MaterializedViewStore, TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
    TransparentAddressRankingConsumer,
};
use zinder_store::SecondaryChainStore;

/// Settings the binary populates before constructing the adapter.
#[derive(Clone, Copy, Debug)]
pub struct ExplorerEndpointMetadata {
    /// Network the consumer mirrors.
    pub network: Network,
}

impl Default for ExplorerEndpointMetadata {
    fn default() -> Self {
        Self {
            network: Network::ZcashRegtest,
        }
    }
}

/// Exact dependency input for an operator-built `zinder-explorer` binary endpoint.
///
/// Both the binary and production-shaped contract proof use this composition
/// root. Adding a binary dependency therefore changes one shared input shape
/// before capability admission can succeed.
pub struct ExplorerQueryEndpointComposition {
    /// Network identity served by the endpoint.
    pub metadata: ExplorerEndpointMetadata,
    /// Read-only explorer materialized-view store, when configured.
    pub materialized_view_store: Option<MaterializedViewStore>,
    /// Node-advertised network-upgrade activation evidence, when configured.
    pub network_upgrade_activations: Option<Arc<NetworkUpgradeActivations>>,
    /// Native wallet endpoint used by federated explorer reads, when configured.
    pub wallet_query_endpoint: Option<String>,
    /// Shared-secret bearer token sent to the native wallet endpoint.
    pub wallet_query_bearer_token: Option<BearerToken>,
    /// Shared-secret bearer token required by the explorer endpoint.
    pub bearer_token: Option<BearerToken>,
}

impl ExplorerQueryEndpointComposition {
    /// Admits every configured dependency and freezes the endpoint contract.
    pub async fn compose(self) -> Result<ExplorerQueryGrpcAdapter, ExplorerEndpointAdmissionError> {
        let mut builder = ExplorerQueryGrpcAdapter::builder(self.metadata);
        if let Some(materialized_view_store) = self.materialized_view_store {
            builder = builder.with_materialized_view_store(materialized_view_store);
        }
        if let Some(network_upgrade_activations) = self.network_upgrade_activations {
            builder = builder.with_network_upgrade_activations(network_upgrade_activations);
        }
        if let Some(wallet_query_endpoint) = self.wallet_query_endpoint {
            builder = builder.with_wallet_query_endpoint(wallet_query_endpoint);
        }
        if let Some(wallet_query_bearer_token) = self.wallet_query_bearer_token {
            builder = builder.with_wallet_query_bearer_token(wallet_query_bearer_token);
        }
        if let Some(bearer_token) = self.bearer_token {
            builder = builder.with_bearer_token(bearer_token);
        }
        builder.build().await
    }
}

/// Lower-level explorer gRPC adapter builder for library and focused test compositions.
///
/// Capability discovery is unavailable until [`Self::build`] admits the
/// configured wallet dependency and freezes the composed contract. The
/// operator-built binary and its production-shaped proof use
/// [`ExplorerQueryEndpointComposition`] as their shared composition root.
pub struct ExplorerQueryGrpcAdapterBuilder {
    metadata: ExplorerEndpointMetadata,
    wallet_query_endpoint: Option<String>,
    wallet_query_bearer_token: Option<BearerToken>,
    bearer_token: Option<BearerToken>,
    canonical_store: Option<SecondaryChainStore>,
    materialized_view_store: Option<MaterializedViewStore>,
    transaction_history_materialized_view_reader:
        Option<Arc<dyn TransactionHistoryMaterializedViewReadApi>>,
    network_upgrade_activations: Option<Arc<NetworkUpgradeActivations>>,
    upstream_observation_cache: UpstreamObservationCache,
}

impl ExplorerQueryGrpcAdapterBuilder {
    fn new(metadata: ExplorerEndpointMetadata) -> Self {
        Self {
            metadata,
            wallet_query_endpoint: None,
            wallet_query_bearer_token: None,
            bearer_token: None,
            canonical_store: None,
            materialized_view_store: None,
            transaction_history_materialized_view_reader: None,
            network_upgrade_activations: None,
            upstream_observation_cache: UpstreamObservationCache::empty(),
        }
    }

    /// Wires the consumer-side materialized-view store.
    #[must_use]
    pub fn with_materialized_view_store(mut self, store: MaterializedViewStore) -> Self {
        self.transaction_history_materialized_view_reader = Some(Arc::new(
            TransactionHistoryMaterializedViewReader::new(store.clone()),
        ));
        self.materialized_view_store = Some(store);
        self
    }

    /// Wires the canonical secondary store so explorer handlers can read typed
    /// block and transaction facts without requesting raw blobs from the
    /// wallet plane.
    #[must_use]
    pub fn with_canonical_store(mut self, store: SecondaryChainStore) -> Self {
        self.canonical_store = Some(store);
        self
    }

    /// Wires the successfully fetched network-upgrade activation table.
    #[must_use]
    pub fn with_network_upgrade_activations(
        mut self,
        activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        self.network_upgrade_activations = Some(activations);
        self
    }

    /// Configures the native `WalletQuery` dependency admitted during build.
    #[must_use]
    pub fn with_wallet_query_endpoint(mut self, endpoint: String) -> Self {
        self.wallet_query_endpoint = Some(endpoint);
        self
    }

    /// Attaches a shared-secret bearer token to outbound `WalletQuery` calls.
    #[must_use]
    pub fn with_wallet_query_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.wallet_query_bearer_token = Some(bearer_token);
        self
    }

    /// Wires a shared-secret bearer token into the explorer-query adapter.
    ///
    /// When set, every gRPC request must carry an `authorization: Bearer
    /// <token>` metadata header that matches `bearer_token`. When unset,
    /// localhost-only deployments stay open by default.
    #[must_use]
    pub fn with_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.bearer_token = Some(bearer_token);
        self
    }

    /// Admits dependencies and freezes the exact endpoint capability set.
    pub async fn build(self) -> Result<ExplorerQueryGrpcAdapter, ExplorerEndpointAdmissionError> {
        if self.wallet_query_endpoint.is_none() && self.wallet_query_bearer_token.is_some() {
            return Err(ExplorerEndpointAdmissionError::WalletAuthorizationRequiresEndpoint);
        }
        if let Some(materialized_view_store) = self.materialized_view_store.as_ref()
            && materialized_view_store.network() != self.metadata.network
        {
            return Err(
                ExplorerEndpointAdmissionError::MaterializedViewStoreNetworkMismatch {
                    expected: self.metadata.network,
                    actual: materialized_view_store.network(),
                },
            );
        }
        if let Some(canonical_store) = self.canonical_store.as_ref() {
            let canonical_network = canonical_store
                .network()
                .ok_or(ExplorerEndpointAdmissionError::CanonicalStoreNetworkUnspecified)?;
            if canonical_network != self.metadata.network {
                return Err(
                    ExplorerEndpointAdmissionError::CanonicalStoreNetworkMismatch {
                        expected: self.metadata.network,
                        actual: canonical_network,
                    },
                );
            }
        }
        if let Some(activations) = self.network_upgrade_activations.as_deref()
            && activations.network() != self.metadata.network
        {
            return Err(
                ExplorerEndpointAdmissionError::NetworkUpgradeActivationsNetworkMismatch {
                    expected: self.metadata.network,
                    actual: activations.network(),
                },
            );
        }
        let has_active_transparent_address_ranking_generation =
            has_active_transparent_address_ranking_generation(
                self.materialized_view_store.as_ref(),
            )?;
        let wallet_endpoint = match self.wallet_query_endpoint.as_deref() {
            Some(endpoint) => Some(
                AdmittedWalletQueryEndpoint::admit(
                    endpoint,
                    self.wallet_query_bearer_token.as_ref(),
                    self.metadata.network,
                )
                .await?,
            ),
            None => None,
        };
        let endpoint_capabilities = ExplorerEndpointCapabilities::derive(
            self.canonical_store.as_ref(),
            self.materialized_view_store.as_ref(),
            self.network_upgrade_activations.as_deref(),
            wallet_endpoint.as_ref(),
            has_active_transparent_address_ranking_generation,
        );
        Ok(ExplorerQueryGrpcAdapter {
            metadata: self.metadata,
            wallet_endpoint,
            bearer_token: self.bearer_token,
            canonical_store: self.canonical_store,
            materialized_view_store: self.materialized_view_store,
            transaction_history_materialized_view_reader: self
                .transaction_history_materialized_view_reader,
            network_upgrade_activations: self.network_upgrade_activations,
            endpoint_capabilities,
            upstream_observation_cache: self.upstream_observation_cache,
        })
    }
}

fn has_active_transparent_address_ranking_generation(
    materialized_view_store: Option<&MaterializedViewStore>,
) -> Result<bool, ExplorerEndpointAdmissionError> {
    let Some(materialized_view_store) = materialized_view_store
        .filter(|store| store.has_consumer(TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME))
    else {
        return Ok(false);
    };
    TransparentAddressRankingConsumer::active_metadata(materialized_view_store)
        .map(|metadata| metadata.is_some())
        .map_err(ExplorerEndpointAdmissionError::TransparentAddressRankingMetadataRead)
}

/// Server adapter implementing the admitted `ExplorerQuery` contract.
#[derive(Clone)]
pub struct ExplorerQueryGrpcAdapter {
    metadata: ExplorerEndpointMetadata,
    wallet_endpoint: Option<AdmittedWalletQueryEndpoint>,
    bearer_token: Option<BearerToken>,
    canonical_store: Option<SecondaryChainStore>,
    materialized_view_store: Option<MaterializedViewStore>,
    transaction_history_materialized_view_reader:
        Option<Arc<dyn TransactionHistoryMaterializedViewReadApi>>,
    network_upgrade_activations: Option<Arc<NetworkUpgradeActivations>>,
    endpoint_capabilities: ExplorerEndpointCapabilities,
    upstream_observation_cache: UpstreamObservationCache,
}

impl ExplorerQueryGrpcAdapter {
    fn require_method_capability(
        &self,
        capability: &'static str,
        method: &'static str,
    ) -> Result<(), Status> {
        if self.endpoint_capabilities.contains(capability) {
            return Ok(());
        }
        Err(ExplorerError::unsupported(format!(
            "{method} is not part of this endpoint's admitted capability contract"
        ))
        .into())
    }

    /// Starts a lower-level adapter build for library or focused test use.
    ///
    /// Call [`ExplorerQueryGrpcAdapterBuilder::build`] before exposing
    /// discovery or request handling. Operator-built binary compositions use
    /// [`ExplorerQueryEndpointComposition`] instead.
    #[must_use]
    pub fn builder(metadata: ExplorerEndpointMetadata) -> ExplorerQueryGrpcAdapterBuilder {
        ExplorerQueryGrpcAdapterBuilder::new(metadata)
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

    /// Returns the exact immutable capability strings advertised by this
    /// process.
    #[must_use]
    pub fn advertised_capabilities(&self) -> Arc<[&'static str]> {
        self.endpoint_capabilities.shared_identifiers()
    }

    /// Returns whether this composition admitted a native `WalletQuery` dependency.
    #[must_use]
    pub fn has_wallet_query_dependency(&self) -> bool {
        self.wallet_endpoint.is_some()
    }

    /// Checks the admitted `WalletQuery` channel and its frozen contract.
    ///
    /// A composition with no `WalletQuery` dependency is healthy by
    /// construction and returns `Ok(())`.
    pub async fn check_wallet_query_health(&self) -> Result<(), ExplorerWalletQueryHealthError> {
        match self.wallet_endpoint.as_ref() {
            Some(wallet_endpoint) => wallet_endpoint.check_health().await,
            None => Ok(()),
        }
    }
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
                self.materialized_view_store.as_ref(),
                EXPLORER_SERVER_INFO_V1,
                None,
                0,
            )?,
        )
        .await;
        let materialized_view_status =
            read_materialized_view_status(self.materialized_view_store.as_ref())?;
        Ok(Response::new(ServerInfoResponse {
            freshness: Some(freshness),
            info: Some(ExplorerServerInfo {
                common: Some(ops::ServerInfo {
                    network: encode_zinder_native_chain_name(self.metadata.network).to_owned(),
                    service_name: env!("CARGO_PKG_NAME").to_owned(),
                    service_version: env!("CARGO_PKG_VERSION").to_owned(),
                    build_git_commit: zinder_runtime::BUILD_GIT_COMMIT.to_owned(),
                    capabilities: self
                        .advertised_capabilities()
                        .iter()
                        .copied()
                        .map(str::to_owned)
                        .collect(),
                    contract_revision: zinder_proto::CONTRACT_REVISION,
                    materialized_view_preset: self.materialized_view_store.as_ref().map_or_else(
                        String::new,
                        |store| {
                            store
                                .effective_materialized_view_preset()
                                .as_str()
                                .to_owned()
                        },
                    ),
                    materialized_view_identities: self
                        .materialized_view_store
                        .as_ref()
                        .map_or_else(Vec::new, |store| {
                            store
                                .declared_consumer_names()
                                .map(|name| name.as_str().to_owned())
                                .collect()
                        }),
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
                OP.method,
            )?;
            let fallback_activations = NetworkUpgradeActivations::empty(self.metadata.network);
            let network_upgrade_activations = self
                .network_upgrade_activations
                .as_deref()
                .unwrap_or(&fallback_activations);
            let mut client = self.wallet_client(OP.method)?;
            query_transaction_detail(
                &mut client,
                TransactionDetailContext {
                    chain_store: self.canonical_store.as_ref(),
                    materialized_view_store: self.materialized_view_store.as_ref(),
                    network: self.metadata.network,
                    network_upgrade_activations,
                    upstream_observation_cache: &self.upstream_observation_cache,
                    include_transaction_fees: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_TRANSACTION_FEES_V1),
                    include_intrinsic_value_balances: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1),
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
            self.require_method_capability(capabilities::EXPLORER_BLOCK_SUMMARY_V2, OP.method)?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
                OP.method,
            )?;
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
            self.require_method_capability(
                capabilities::EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
                OP.method,
            )?;
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
            self.require_method_capability(
                capabilities::EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(capabilities::EXPLORER_BLOCK_DETAIL_V1, OP.method)?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_block_transactions(
                BlockTransactionsContext {
                    chain_store: canonical_store,
                    materialized_view_store,
                    upstream_observation_cache: &self.upstream_observation_cache,
                    include_fee_projected_input_values: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_TRANSACTION_FEES_V1),
                    include_final_note_commitment_roots: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1),
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
            self.require_method_capability(capabilities::EXPLORER_SEARCH_V1, OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_search(
                self.materialized_view_store.as_ref(),
                &mut client,
                self.metadata.network,
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
            self.require_method_capability(
                capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            let network_upgrade_activations =
                self.network_upgrade_activations.as_deref().ok_or_else(|| {
                    ExplorerError::internal(
                        "CommitmentRootSearch was admitted without Sapling activation evidence",
                    )
                })?;
            query_commitment_root_search(
                CommitmentRootSearchContext {
                    materialized_view_store,
                    canonical_store,
                    activations: network_upgrade_activations,
                    upstream_observation_cache: &self.upstream_observation_cache,
                    include_displaced_root_results: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1),
                },
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
            self.require_method_capability(capabilities::EXPLORER_MEMPOOL_SUMMARY_V2, OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_mempool_summary(
                self.materialized_view_store.as_ref(),
                &mut client,
                self.metadata.network,
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
            self.require_method_capability(capabilities::EXPLORER_MEMPOOL_SNAPSHOT_V1, OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_mempool_snapshot(
                self.materialized_view_store.as_ref(),
                &mut client,
                self.metadata.network,
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
            self.require_method_capability(capabilities::EXPLORER_MEMPOOL_ACTIVITY_V1, OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_mempool_activity(
                self.materialized_view_store.as_ref(),
                &mut client,
                self.metadata.network,
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
                OP.method,
            )?;
            let materialized_view_store =
                self.materialized_view_store.as_ref().ok_or_else(|| {
                    ExplorerError::dependency_not_configured(
                        "TransparentAddressActivity requires a materialized-view store",
                    )
                })?;
            let mut wallet_client = self
                .canonical_store
                .is_none()
                .then(|| self.wallet_client(OP.method))
                .transpose()?;
            query_transparent_address_activity(
                TransparentAddressActivityContext {
                    materialized_view_store,
                    canonical_store: self.canonical_store.as_ref(),
                    network: self.metadata.network,
                    upstream_observation_cache: &self.upstream_observation_cache,
                },
                wallet_client.as_mut(),
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
                OP.method,
            )?;
            let materialized_view_store =
                self.materialized_view_store.as_ref().ok_or_else(|| {
                    ExplorerError::dependency_not_configured(
                        "TransparentAddressDeltas requires a materialized-view store",
                    )
                })?;
            let mut client = self.wallet_client(OP.method)?;
            query_transparent_address_deltas(
                materialized_view_store,
                &mut client,
                self.metadata.network,
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
            self.require_method_capability(capabilities::EXPLORER_FEE_SUMMARY_V1, OP.method)?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_PAID_FEE_DISTRIBUTION_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_SUMMARY_V1,
                OP.method,
            )?;
            let mut client = self.wallet_client(OP.method)?;
            query_value_pool_summary(
                self.materialized_view_store.as_ref(),
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
            self.require_method_capability(
                capabilities::EXPLORER_NETWORK_UPGRADE_STATUS_V1,
                OP.method,
            )?;
            let network_upgrade_activations =
                self.network_upgrade_activations.as_deref().ok_or_else(|| {
                    ExplorerError::internal(
                        "NetworkUpgradeStatus was admitted without activation-table evidence",
                    )
                })?;
            let mut client = self.wallet_client(OP.method)?;
            query_network_upgrade_status(
                self.materialized_view_store.as_ref(),
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(capabilities::EXPLORER_UTXO_SET_SUMMARY_V1, OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_utxo_set_summary(
                self.materialized_view_store.as_ref(),
                &mut client,
                self.endpoint_capabilities
                    .contains(capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1),
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
            self.require_method_capability(
                capabilities::EXPLORER_CHAIN_REORG_HISTORY_V1,
                OP.method,
            )?;
            let materialized_view_store =
                self.materialized_view_store.as_ref().ok_or_else(|| {
                    ExplorerError::dependency_not_configured(
                        "ChainReorgHistory requires a materialized-view store",
                    )
                })?;
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
            self.require_method_capability(
                capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
                OP.method,
            )?;
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
            self.require_method_capability(
                capabilities::EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1,
                OP.method,
            )?;
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
            self.require_method_capability(
                capabilities::EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
                OP.method,
            )?;
            let materialized_view_store =
                self.materialized_view_store.as_ref().ok_or_else(|| {
                    ExplorerError::dependency_not_configured(
                        "MempoolEventCounts requires a materialized-view store",
                    )
                })?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
                OP.method,
            )?;
            let materialized_view_reader =
                self.require_transaction_history_materialized_view_reader()?;
            let mut client = self.wallet_client(OP.method)?;
            transaction_history(
                TransactionHistoryContext {
                    materialized_view_reader: Arc::clone(materialized_view_reader),
                    materialized_view_store: self.materialized_view_store.as_ref(),
                    chain_store: self.canonical_store.as_ref(),
                    upstream_observation_cache: &self.upstream_observation_cache,
                    include_transaction_fees: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_TRANSACTION_FEES_V1),
                    include_intrinsic_value_balances: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1),
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
            self.require_method_capability(
                capabilities::EXPLORER_TRANSACTION_RECENT_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_recent_transactions(
                RecentTransactionsContext {
                    materialized_view_store,
                    chain_store: self.canonical_store.as_ref(),
                    upstream_observation_cache: &self.upstream_observation_cache,
                    include_transaction_fees: self
                        .endpoint_capabilities
                        .contains(capabilities::EXPLORER_TRANSACTION_FEES_V1),
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
            self.require_method_capability(capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1, OP.method)?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
            query_overview_snapshot(
                materialized_view_store,
                &mut client,
                self.metadata.network,
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
            self.require_method_capability(
                capabilities::EXPLORER_MIGRATION_OVERVIEW_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(capabilities::EXPLORER_MIGRATION_COHORTS_V1, OP.method)?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
            self.require_method_capability(
                capabilities::EXPLORER_MIGRATION_DENOMINATIONS_V1,
                OP.method,
            )?;
            let materialized_view_store = self.require_materialized_view_store(OP.method)?;
            let mut client = self.wallet_client(OP.method)?;
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
    ) -> Result<&Arc<dyn TransactionHistoryMaterializedViewReadApi>, Status> {
        self.transaction_history_materialized_view_reader
            .as_ref()
            .ok_or_else(|| {
                ExplorerError::dependency_not_configured(
                    "TransactionHistory requires a materialized-view reader",
                )
                .into()
            })
    }

    fn require_materialized_view_store(
        &self,
        method: &'static str,
    ) -> Result<&MaterializedViewStore, Status> {
        self.materialized_view_store.as_ref().ok_or_else(|| {
            Status::from(ExplorerError::dependency_not_configured(format!(
                "{method} requires a materialized-view store with its declared consumers; \
                 configure --storage-path"
            )))
        })
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

    /// Returns a client over the channel admitted during adapter construction.
    fn wallet_client(
        &self,
        method: &'static str,
    ) -> Result<WalletQueryClient<zinder_runtime::AuthenticatedChannel>, Status> {
        self.wallet_endpoint
            .as_ref()
            .map(AdmittedWalletQueryEndpoint::wallet_client)
            .ok_or_else(|| {
                ExplorerError::internal(format!(
                    "{method} was admitted without its WalletQuery dependency"
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

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use tempfile::{TempDir, tempdir};
    use zinder_core::{BlockHash, BlockHeight};
    use zinder_materialized_views::{
        MaterializedViewStoreOptions, TRANSPARENT_ADDRESS_RANKING_METADATA_COLUMN_FAMILY,
        TRANSPARENT_ADDRESS_RANKING_SCHEMA, TransparentAddressRankingCoverage,
        TransparentAddressRankingSnapshotPlan,
    };
    use zinder_store::RocksDbResourceBudget;

    use super::*;

    fn ranking_store() -> Result<(TempDir, MaterializedViewStore), Box<dyn std::error::Error>> {
        ranking_store_for_network(Network::ZcashRegtest)
    }

    fn ranking_store_for_network(
        network: Network,
    ) -> Result<(TempDir, MaterializedViewStore), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            network,
            MaterializedViewStoreOptions {
                consumers: &[TRANSPARENT_ADDRESS_RANKING_SCHEMA],
                sync_writes: false,
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        Ok((directory, store))
    }

    fn activate_empty_ranking_generation(
        store: &MaterializedViewStore,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let height = BlockHeight::new(1);
        let block_hash = BlockHash::from_bytes([0x11; 32]);
        TransparentAddressRankingConsumer::initialize_snapshot_generation(
            store,
            TransparentAddressRankingSnapshotPlan {
                generation: 1,
                base_height: height,
                base_block_hash: block_hash,
                target_height: height,
                target_block_hash: block_hash,
                expected_summary_count: 0,
                base_coverage: TransparentAddressRankingCoverage {
                    balance_complete_through_height: height,
                    history_complete_from_height: Some(height),
                    history_complete_through_height: Some(height),
                    lifetime_statistics_complete: true,
                },
            },
        )?;
        TransparentAddressRankingConsumer::finalize_snapshot_base(store, 1)?;
        TransparentAddressRankingConsumer::activate_snapshot_generation_at_cursor(
            store,
            1,
            b"ranking-admission-test-cursor",
        )?;
        Ok(())
    }

    fn require_omitted<T>(
        outcome: Result<Response<T>, Status>,
        method: &'static str,
    ) -> Result<Status, Box<dyn std::error::Error>> {
        outcome
            .err()
            .ok_or_else(|| std::io::Error::other(format!("{method} unexpectedly succeeded")).into())
    }

    #[test]
    fn ranking_admission_reports_an_absent_active_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, store) = ranking_store()?;

        assert!(!has_active_transparent_address_ranking_generation(Some(
            &store
        ))?);
        Ok(())
    }

    #[test]
    fn ranking_admission_reports_a_present_active_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, store) = ranking_store()?;
        activate_empty_ranking_generation(&store)?;

        assert!(has_active_transparent_address_ranking_generation(Some(
            &store
        ))?);
        Ok(())
    }

    #[tokio::test]
    async fn ranking_admission_fails_on_malformed_active_metadata()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, store) = ranking_store()?;
        activate_empty_ranking_generation(&store)?;
        let mut metadata_keys = Vec::new();
        store.visit_consumer_rows(
            TRANSPARENT_ADDRESS_RANKING_METADATA_COLUMN_FAMILY,
            |key, _payload| {
                metadata_keys.push(key.to_vec());
                Ok(())
            },
        )?;
        assert_eq!(metadata_keys.len(), 1);
        store.put_consumer(
            TRANSPARENT_ADDRESS_RANKING_METADATA_COLUMN_FAMILY,
            &metadata_keys[0],
            b"malformed",
        )?;

        let outcome = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata::default())
            .with_materialized_view_store(store)
            .build()
            .await;
        assert!(matches!(
            outcome,
            Err(ExplorerEndpointAdmissionError::TransparentAddressRankingMetadataRead(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn builder_accepts_same_network_materialized_view_storage()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, store) = ranking_store()?;

        ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata::default())
            .with_materialized_view_store(store)
            .build()
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn builder_rejects_cross_network_storage_before_endpoint_io_or_capability_derivation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, store) = ranking_store_for_network(Network::ZcashTestnet)?;

        let outcome = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata::default())
            .with_materialized_view_store(store)
            .with_wallet_query_endpoint("not a valid endpoint".to_owned())
            .build()
            .await;
        assert!(matches!(
            outcome,
            Err(
                ExplorerEndpointAdmissionError::MaterializedViewStoreNetworkMismatch {
                    expected: Network::ZcashRegtest,
                    actual: Network::ZcashTestnet,
                }
            )
        ));
        Ok(())
    }

    #[tokio::test]
    async fn omitted_methods_fail_before_request_parsing_or_dependency_access()
    -> Result<(), Box<dyn std::error::Error>> {
        let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata::default())
            .build()
            .await?;

        for status in [
            require_omitted(
                adapter
                    .block_summaries_in_range(Request::new(BlockSummariesInRangeRequest::default()))
                    .await,
                "BlockSummariesInRange",
            )?,
            require_omitted(
                adapter
                    .transparent_address_deltas(Request::new(
                        TransparentAddressDeltasRequest::default(),
                    ))
                    .await,
                "TransparentAddressDeltas",
            )?,
            require_omitted(
                adapter
                    .mempool_summary(Request::new(MempoolSummaryRequest::default()))
                    .await,
                "MempoolSummary",
            )?,
            require_omitted(
                adapter
                    .overview_snapshot(Request::new(OverviewSnapshotRequest::default()))
                    .await,
                "OverviewSnapshot",
            )?,
            require_omitted(
                adapter
                    .transaction_detail(Request::new(TransactionDetailRequest::default()))
                    .await,
                "TransactionDetail",
            )?,
        ] {
            assert_eq!(status.code(), Code::Unimplemented);
            assert!(status.message().contains("admitted capability contract"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn discovery_reuses_the_finalized_capability_allocation()
    -> Result<(), Box<dyn std::error::Error>> {
        let adapter = ExplorerQueryGrpcAdapter::builder(ExplorerEndpointMetadata::default())
            .build()
            .await?;
        let first = adapter.advertised_capabilities();
        let second = adapter.advertised_capabilities();
        assert!(Arc::ptr_eq(&first, &second));

        let server_info = adapter
            .server_info(Request::new(ServerInfoRequest {}))
            .await?
            .into_inner()
            .info
            .and_then(|info| info.common)
            .ok_or("explorer ServerInfo omitted common identity")?;
        assert_eq!(
            server_info.capabilities,
            first
                .iter()
                .map(|identifier| (*identifier).to_owned())
                .collect::<Vec<_>>()
        );
        Ok(())
    }
}
