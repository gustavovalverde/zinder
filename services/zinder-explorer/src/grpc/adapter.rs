//! `ExplorerQuery` gRPC adapter.
//!
//! Serves [`ExplorerQuery::ServerInfo`] (advertising [`EXPLORER_SERVER_INFO_V1`])
//! and the derive-backed explorer surfaces. Handlers that need canonical
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

use tokio::sync::OnceCell;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status, service::interceptor::InterceptedService};
use zinder_core::{
    BlockHeight, Network, NetworkUpgradeActivations, wire::encode_zinder_native_chain_name,
};
use zinder_proto::capabilities::{
    CapabilitySurface, EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1,
    EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1, EXPLORER_PAID_FEE_DISTRIBUTION_V1,
    EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_HISTORY_V1, EXPLORER_TRANSACTION_HISTORY_V2,
    EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1, EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
    EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1, ExplorerReadiness, capabilities_for_surface,
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
        ValuePoolSummaryResponse, VerifyPaymentDisclosureRequest, VerifyPaymentDisclosureResponse,
        explorer_query_server::{ExplorerQuery, ExplorerQueryServer},
    },
    ops,
    wallet::wallet_query_client::WalletQueryClient,
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenConnectError, BearerTokenServerInterceptor,
    RpcMetricNames, RpcOutcome, connect_zinder_grpc, describe_rpc_metrics, record_rpc_request,
};
use zinder_source::NodeSource;

/// Metric pair the `ExplorerQuery` adapter emits per request.
const EXPLORER_RPC_METRICS: RpcMetricNames = RpcMetricNames::for_service(
    "zinder_explorer_request_duration_seconds",
    "zinder_explorer_request_total",
);

/// First canonical artifact schema that contains transaction-intrinsic balances.
const MINIMUM_INTRINSIC_VALUE_BALANCE_HISTORY_SCHEMA_VERSION: u16 = 15;
use super::block_activity::handle_block_activity_distribution;
use super::block_view::{
    handle_block_detail, handle_block_production_in_time_range, handle_block_production_series,
    handle_block_summaries_in_range, handle_block_transactions,
};
use super::chain_reorg_history::handle_chain_reorg_history;
use super::commitment_root_search::handle_commitment_root_search;
use super::conventional_fee_distribution::handle_conventional_fee_distribution;
use super::displaced_block::{handle_displaced_block_detail, handle_displaced_block_history};
use super::error::ExplorerError;
use super::fee_summary::handle_fee_summary;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
    read_derive_status, spawn_upstream_observation_probe_task,
};
use super::mempool::{handle_mempool_activity, handle_mempool_snapshot, handle_mempool_summary};
use super::mempool_event_counts::handle_mempool_event_counts;
use super::migration::{
    handle_migration_cohorts, handle_migration_denominations, handle_migration_overview,
};
use super::network_upgrade_status::handle_network_upgrade_status;
use super::overview_snapshot::handle_overview_snapshot;
use super::paid_fee_distribution::handle_paid_fee_distribution;
use super::recent_transactions::{RecentTransactionsStream, handle_recent_transactions};
use super::search::handle_search;
use super::transaction_component_summary::handle_transaction_component_summary;
use super::transaction_detail::{TransactionDetailContext, handle_transaction_detail};
use super::transaction_history::transaction_history;
use super::transparent_address_activity::{
    TransparentAddressActivityContext, handle_transparent_address_activity,
};
use super::transparent_address_deltas::handle_transparent_address_deltas;
use super::transparent_address_ranking::handle_transparent_address_ranking;
use super::utxo_set_summary::handle_utxo_set_summary;
use super::value_pool_balance_history::handle_value_pool_balance_history;
use super::value_pool_flow::{
    handle_value_pool_flow_amount_threshold_summary, handle_value_pool_flow_events_in_range,
    handle_value_pool_flow_history, handle_value_pool_flow_rounded_amount_summary,
    handle_value_pool_flow_summary,
};
use super::value_pool_summary::handle_value_pool_summary;
use zinder_derive::{DeriveStore, TRANSACTION_HISTORY_CONSUMER_NAME};
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

/// Server adapter implementing `ExplorerQuery` for `zinder-explorer`.
///
/// Construct with [`ExplorerQueryGrpcAdapter::new`] and chain
/// [`ExplorerQueryGrpcAdapter::with_wallet_query_endpoint`] to enable the
/// balance compute path. Without the endpoint the balance method returns
/// `UNAVAILABLE` and `ServerInfo` omits the corresponding capability.
#[derive(Clone)]
pub struct ExplorerQueryGrpcAdapter {
    settings: ExplorerServerInfoSettings,
    wallet_query_endpoint: Option<String>,
    wallet_query_bearer_token: Option<BearerToken>,
    bearer_token: Option<BearerToken>,
    canonical_store: Option<SecondaryChainStore>,
    derive_store: Option<DeriveStore>,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    wallet_channel: Arc<OnceCell<AuthenticatedChannel>>,
    prevout_resolution_online: bool,
    payment_disclosure_verifier_online: bool,
    upstream_observation_cache: UpstreamObservationCache,
}

impl ExplorerQueryGrpcAdapter {
    /// Creates a new explorer-query adapter without a federated balance path.
    #[must_use]
    pub fn new(settings: ExplorerServerInfoSettings) -> Self {
        Self {
            settings,
            wallet_query_endpoint: None,
            wallet_query_bearer_token: None,
            bearer_token: None,
            canonical_store: None,
            derive_store: None,
            network_upgrade_activations: Arc::new(NetworkUpgradeActivations::empty(
                settings.network,
            )),
            wallet_channel: Arc::new(OnceCell::new()),
            prevout_resolution_online: false,
            payment_disclosure_verifier_online: false,
            upstream_observation_cache: UpstreamObservationCache::empty(),
        }
    }

    /// Wires the consumer-side derive store so block-view RPCs can read the
    /// materialized `BlockSummary` records.
    #[must_use]
    pub fn with_derive_store(mut self, store: DeriveStore) -> Self {
        self.derive_store = Some(store);
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

    /// Wires the node-advertised network-upgrade activation table the
    /// `NetworkUpgradeStatus` handler projects onto the wire.
    #[must_use]
    pub fn with_network_upgrade_activations(
        mut self,
        activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        self.network_upgrade_activations = activations;
        self
    }

    /// Configures the `WalletQuery` endpoint the balance handler reads from.
    ///
    /// The same endpoint serves canonical transparent outputs and the live
    /// mempool point lookups composed into the balance response.
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

    /// Marks transparent-output resolution as online so the adapter
    /// advertises the per-tx paid-fee capability.
    ///
    /// The binary sets this flag once it opens a derive store with the bundled
    /// `TransactionFeesConsumer` column families. The flag is the single
    /// source of truth for whether paid-fee fields appear in
    /// `TransactionDetail` and `MempoolActivity` responses; downstream
    /// handlers branch on presence of materialized rows rather than re-reading
    /// this flag.
    #[must_use]
    pub const fn with_prevout_resolution_online(mut self, online: bool) -> Self {
        self.prevout_resolution_online = online;
        self
    }

    /// Marks the hosted payment-disclosure verifier as online so the adapter
    /// advertises `EXPLORER_PAYMENT_DISCLOSURE_VERIFY_V1`.
    ///
    /// Operator-opt-in (default off): the consumer sees no capability and
    /// falls back to local verification. Wiring the verifier is the binary's
    /// responsibility; the flag only controls advertisement.
    #[must_use]
    pub const fn with_payment_disclosure_verifier_online(mut self, online: bool) -> Self {
        self.payment_disclosure_verifier_online = online;
        self
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
        let wallet_query_online = self.wallet_query_endpoint.is_some();
        let readiness = ExplorerReadiness {
            wallet_query_online,
            canonical_store_online: self.canonical_store.is_some(),
            derive_store_online: self.derive_store.is_some(),
            prevout_resolution_online: self.prevout_resolution_online && wallet_query_online,
            payment_disclosure_verifier_online: self.payment_disclosure_verifier_online,
        };
        let ranking_active = self.derive_store.as_ref().is_some_and(|store| {
            zinder_derive::TransparentAddressRankingConsumer::active_metadata(store)
                .ok()
                .flatten()
                .is_some()
        });
        let conventional_fee_distribution_covered =
            self.derive_store.as_ref().is_some_and(|store| {
                zinder_derive::ConventionalFeeDistributionConsumer::coverage(store)
                    .ok()
                    .flatten()
                    .is_some()
            });
        let paid_fee_distribution_covered = self.derive_store.as_ref().is_some_and(|store| {
            zinder_derive::PaidFeeDistributionConsumer::coverage(store)
                .ok()
                .flatten()
                .is_some()
        });
        let block_production_time_covered =
            block_production_time_projection_available(self.derive_store.as_ref());
        let transaction_intrinsic_value_balances_available =
            self.canonical_store.as_ref().is_some_and(|store| {
                store.try_catch_up().is_ok()
                    && store.current_chain_epoch().is_ok_and(|chain_epoch| {
                        chain_epoch.is_some_and(intrinsic_value_balance_schema_supported)
                    })
            });
        let (transaction_history_v1_available, transaction_history_v2_available) =
            transaction_history_projection_capabilities(self.derive_store.as_ref());
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
                spec.string != EXPLORER_PAID_FEE_DISTRIBUTION_V1 || paid_fee_distribution_covered
            })
            .filter(|spec| {
                spec.string != EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1
                    || block_production_time_covered
            })
            .filter(|spec| {
                spec.string != EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1
                    || transaction_intrinsic_value_balances_available
            })
            .filter(|spec| {
                spec.string != EXPLORER_TRANSACTION_HISTORY_V1 || transaction_history_v1_available
            })
            .filter(|spec| {
                spec.string != EXPLORER_TRANSACTION_HISTORY_V2 || transaction_history_v2_available
            })
            .map(|spec| spec.string)
            .collect()
    }
}

fn block_production_time_projection_available(derive_store: Option<&DeriveStore>) -> bool {
    let Some(store) = derive_store else {
        return false;
    };
    let snapshot = store.read_snapshot();
    let coverage = zinder_derive::BlockProductionTimeConsumer::coverage_snapshot(&snapshot)
        .ok()
        .flatten();
    let projection_state = snapshot
        .consumer_projection_state(zinder_derive::BLOCK_PRODUCTION_TIME_CONSUMER_NAME)
        .ok()
        .flatten();
    drop(snapshot);
    coverage
        .zip(projection_state)
        .is_some_and(|(coverage, projection_state)| {
            coverage.complete_from_height.value() <= 1
                && coverage.complete_through_height >= projection_state.projection_tip_height
        })
}

fn transaction_history_projection_capabilities(derive_store: Option<&DeriveStore>) -> (bool, bool) {
    let Some(state) = derive_store.and_then(|store| {
        store
            .consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME)
            .ok()
            .flatten()
    }) else {
        return (false, false);
    };

    let v2_available = state.coverage.is_some_and(|coverage| {
        coverage.complete_from_height == BlockHeight::new(1)
            && coverage.complete_through_height == state.projection_tip_height
            && coverage.complete_through_hash == state.projection_tip_hash
    });
    (true, v2_available)
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
        // bootstrap call, before any derive-backed capability is up.
        // During `bulk_catchup` this is the only explorer response that
        // is guaranteed to succeed; sync-progress UIs depend on it for
        // an honest denominator. `chain_epoch` is left unset here because
        // ServerInfo resolves no canonical follower tip and so makes no
        // snapshot-consistency claim; the derive-plane indexed tip and the
        // upstream observation carry the rest of the freshness signal.
        let freshness = attach_upstream_observation(
            &self.upstream_observation_cache,
            build_explorer_freshness(self.derive_store.as_ref(), EXPLORER_SERVER_INFO_V1, None, 0)?,
        )
        .await;
        let derive_status = read_derive_status(self.derive_store.as_ref())?;
        Ok(Response::new(ServerInfoResponse {
            freshness: Some(freshness),
            info: Some(ExplorerServerInfo {
                common: Some(ops::ServerInfo {
                    network: encode_zinder_native_chain_name(self.settings.network).to_owned(),
                    service_name: env!("CARGO_PKG_NAME").to_owned(),
                    service_version: env!("CARGO_PKG_VERSION").to_owned(),
                    capabilities: self
                        .advertised_capabilities()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    contract_revision: zinder_proto::CONTRACT_REVISION,
                }),
                vendor: "Zinder".to_owned(),
                derive_status,
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_transaction_detail(
                &mut client,
                TransactionDetailContext {
                    chain_store: self.canonical_store.as_ref(),
                    derive_store: self.derive_store.as_ref(),
                    network: self.settings.network,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_block_summaries_in_range(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            handle_block_production_series(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            handle_block_production_in_time_range(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_block_activity_distribution(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_transaction_component_summary(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_transparent_address_ranking(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_block_detail(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_block_transactions(
                canonical_store,
                derive_store,
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_search(
                self.derive_store.as_ref(),
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
            let derive_store = self.require_derive_store(OP.method)?;
            let canonical_store = self.require_canonical_store(OP.method)?;
            handle_commitment_root_search(
                derive_store,
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_mempool_summary(
                self.derive_store.as_ref(),
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_mempool_snapshot(
                self.derive_store.as_ref(),
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_mempool_activity(
                self.derive_store.as_ref(),
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
            let derive_store = self.derive_store.as_ref().ok_or_else(|| {
                ExplorerError::dependency_not_configured(
                    "TransparentAddressActivity requires a derive store",
                )
            })?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_transparent_address_activity(
                TransparentAddressActivityContext {
                    derive_store,
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
            let derive_store = self.derive_store.as_ref().ok_or_else(|| {
                ExplorerError::dependency_not_configured(
                    "TransparentAddressDeltas requires a derive store",
                )
            })?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_transparent_address_deltas(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_fee_summary(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_conventional_fee_distribution(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_paid_fee_distribution(
                derive_store,
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_summary(
                self.derive_store.as_ref(),
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_network_upgrade_status(
                self.derive_store.as_ref(),
                &self.network_upgrade_activations,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_flow_history(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_flow_events_in_range(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_flow_summary(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_flow_amount_threshold_summary(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_flow_rounded_amount_summary(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_value_pool_balance_history(
                derive_store,
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_utxo_set_summary(
                self.derive_store.as_ref(),
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
            let derive_store = self.derive_store.as_ref().ok_or_else(|| {
                ExplorerError::dependency_not_configured(
                    "ChainReorgHistory requires a derive store",
                )
            })?;
            handle_chain_reorg_history(derive_store, &self.upstream_observation_cache, request)
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
            handle_displaced_block_history(
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
            handle_displaced_block_detail(
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
            let derive_store = self.derive_store.as_ref().ok_or_else(|| {
                ExplorerError::dependency_not_configured(
                    "MempoolEventCounts requires a derive store",
                )
            })?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_mempool_event_counts(
                derive_store,
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
            let derive_store = self.derive_store.as_ref().ok_or_else(|| {
                ExplorerError::dependency_not_configured(
                    "TransactionHistory requires a derive store",
                )
            })?;
            let mut client = self.wallet_client(OP.method).await?;
            transaction_history(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_recent_transactions(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_overview_snapshot(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_migration_overview(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_migration_cohorts(
                derive_store,
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
            let derive_store = self.require_derive_store(OP.method)?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_migration_denominations(
                derive_store,
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

    async fn verify_payment_disclosure(
        &self,
        _request: Request<VerifyPaymentDisclosureRequest>,
    ) -> Result<Response<VerifyPaymentDisclosureResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "VerifyPaymentDisclosure",
            metric: "verify_payment_disclosure",
        };
        let started = Instant::now();
        // ZIP-311 verifier opt-in (off by default). Operator-flagged because
        // verification work would otherwise execute on bytes the operator
        // never approved. The handler never logs or spans
        // `request.payment_disclosure_bytes`; that is the redaction
        // contract documented alongside the RPC.
        let outcome: Result<Response<VerifyPaymentDisclosureResponse>, Status> = if self
            .payment_disclosure_verifier_online
        {
            Err(ExplorerError::unsatisfied_precondition(
                "VerifyPaymentDisclosure is wired but no ZIP-311 verifier is bundled in this build",
            )
            .into())
        } else {
            Err(ExplorerError::unsupported(
                "VerifyPaymentDisclosure is disabled on this server; consumer must fall back to local verification",
            )
            .into())
        };
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }
}

impl ExplorerQueryGrpcAdapter {
    fn require_derive_store(&self, method: &'static str) -> Result<&DeriveStore, Status> {
        self.derive_store.as_ref().ok_or_else(|| {
            ExplorerError::dependency_not_configured(format!(
                "{method} requires the BlockSummary derive view; configure \
                 --storage-path and start the explorer with the consumer wired"
            ))
            .into()
        })
    }

    fn require_canonical_store(
        &self,
        method: &'static str,
    ) -> Result<&SecondaryChainStore, Status> {
        self.canonical_store.as_ref().ok_or_else(|| {
            ExplorerError::dependency_not_configured(format!(
                "{method} requires the canonical fact store; configure --storage-path"
            ))
            .into()
        })
    }

    fn require_wallet_endpoint(&self, method: &'static str) -> Result<&str, Status> {
        self.wallet_query_endpoint.as_deref().ok_or_else(|| {
            ExplorerError::dependency_not_configured(format!(
                "{method} requires a wallet_query_endpoint; configure \
                 --wallet-query-endpoint"
            ))
            .into()
        })
    }

    /// Returns a `WalletQueryClient` that shares one cached HTTP/2 channel
    /// across every request handled by this adapter.
    ///
    /// The first call pays the dial cost; subsequent calls clone the cached
    /// channel, which is `tonic::transport::Channel` internally (cheap clone,
    /// transparent HTTP/2 reconnect).
    async fn wallet_client(
        &self,
        method: &'static str,
    ) -> Result<WalletQueryClient<AuthenticatedChannel>, Status> {
        let endpoint = self.require_wallet_endpoint(method)?;
        let token = self.wallet_query_bearer_token.clone();
        let channel = self
            .wallet_channel
            .get_or_try_init(|| async {
                connect_zinder_grpc(endpoint, token.as_ref())
                    .await
                    .map_err(connect_error_to_status)
            })
            .await?;
        Ok(WalletQueryClient::new(channel.clone()))
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "BearerTokenConnectError is moved out of the Result by the caller; the helper takes ownership"
)]
fn connect_error_to_status(error: BearerTokenConnectError) -> Status {
    ExplorerError::upstream_unreachable(format!("WalletQuery endpoint unreachable: {error}")).into()
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

/// Maps a `tonic::Status::code()` to the short string label the BFF and
/// dashboards filter on. Mirrors `zinder-query`'s `query_error_class` so the
/// two services share the same vocabulary.
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
    use zinder_core::{ArtifactSchemaVersion, BlockHash, BlockHeight, ChainEpochId, Network};
    use zinder_derive::{
        CONVENTIONAL_FEE_DISTRIBUTION_SCHEMA, ConsumerProjectionCoverage, ConsumerProjectionState,
        DeriveStoreOptions, PAID_FEE_DISTRIBUTION_SCHEMA,
    };
    use zinder_store::{ChainStoreOptions, RocksDbResourceBudget, SecondaryChainStore};
    use zinder_testkit::{ChainFixture, StoreFixture};

    use super::*;

    fn adapter_with_transaction_history_state(
        state: Option<ConsumerProjectionState>,
        wallet_query_online: bool,
    ) -> Result<(TempDir, ExplorerQueryGrpcAdapter), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let derive_store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: DeriveStore::bundled_consumers(),
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        if let Some(state) = state {
            derive_store.put_consumer_projection_state(TRANSACTION_HISTORY_CONSUMER_NAME, state)?;
        }
        let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings::default())
            .with_derive_store(derive_store);
        let adapter = if wallet_query_online {
            adapter.with_wallet_query_endpoint(String::from("http://127.0.0.1:1"))
        } else {
            adapter
        };
        Ok((tempdir, adapter))
    }

    #[test]
    fn transaction_history_capabilities_follow_projection_state_and_dependencies()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_tempdir, adapter) = adapter_with_transaction_history_state(None, true)?;
        let capabilities = adapter.advertised_capabilities();
        assert!(!capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V1));
        assert!(!capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V2));

        let partial_state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(7),
            projection_tip_height: BlockHeight::new(20),
            projection_tip_hash: BlockHash::from_bytes([0x20; 32]),
            revision: 1,
            coverage: None,
        };
        let (_tempdir, adapter) =
            adapter_with_transaction_history_state(Some(partial_state), true)?;
        let capabilities = adapter.advertised_capabilities();
        assert!(capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V1));
        assert!(!capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V2));

        let complete_state = ConsumerProjectionState {
            projection_epoch_id: ChainEpochId::new(7),
            projection_tip_height: BlockHeight::new(20),
            projection_tip_hash: BlockHash::from_bytes([0x20; 32]),
            revision: 2,
            coverage: Some(ConsumerProjectionCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: BlockHeight::new(20),
                complete_through_hash: BlockHash::from_bytes([0x20; 32]),
            }),
        };
        let (_tempdir, adapter) =
            adapter_with_transaction_history_state(Some(complete_state), true)?;
        let capabilities = adapter.advertised_capabilities();
        assert!(capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V1));
        assert!(capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V2));

        let mismatched_state = ConsumerProjectionState {
            projection_tip_hash: BlockHash::from_bytes([0x21; 32]),
            ..complete_state
        };
        let (_tempdir, adapter) =
            adapter_with_transaction_history_state(Some(mismatched_state), true)?;
        let capabilities = adapter.advertised_capabilities();
        assert!(capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V1));
        assert!(!capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V2));

        let (_tempdir, adapter) =
            adapter_with_transaction_history_state(Some(complete_state), false)?;
        let capabilities = adapter.advertised_capabilities();
        assert!(!capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V1));
        assert!(!capabilities.contains(&EXPLORER_TRANSACTION_HISTORY_V2));
        Ok(())
    }

    #[test]
    fn fee_distribution_capabilities_are_hidden_without_projection_coverage()
    -> Result<(), Box<dyn std::error::Error>> {
        let tempdir = tempdir()?;
        let derive_store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: &[
                    CONVENTIONAL_FEE_DISTRIBUTION_SCHEMA,
                    PAID_FEE_DISTRIBUTION_SCHEMA,
                ],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings::default())
            .with_derive_store(derive_store)
            .with_wallet_query_endpoint(String::from("http://127.0.0.1:1"));

        assert!(
            !adapter
                .advertised_capabilities()
                .contains(&EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1)
        );
        assert!(
            !adapter
                .advertised_capabilities()
                .contains(&EXPLORER_PAID_FEE_DISTRIBUTION_V1)
        );
        Ok(())
    }

    #[test]
    fn intrinsic_value_balance_capability_requires_a_supported_canonical_schema()
    -> Result<(), Box<dyn std::error::Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
        let store_fixture =
            StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
        let canonical_store = SecondaryChainStore::open(
            store_fixture.tempdir_path(),
            store_fixture
                .tempdir_path()
                .join("intrinsic-balance-capability-secondary"),
            ChainStoreOptions::for_local_tests(),
        )?;
        canonical_store.try_catch_up()?;
        let tempdir = tempdir()?;
        let derive_store = DeriveStore::open(
            tempdir.path(),
            DeriveStoreOptions {
                consumers: DeriveStore::bundled_consumers(),
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                sync_writes: false,
            },
        )?;
        let adapter = ExplorerQueryGrpcAdapter::new(ExplorerServerInfoSettings::default())
            .with_derive_store(derive_store)
            .with_canonical_store(canonical_store);

        assert!(
            !adapter
                .advertised_capabilities()
                .contains(&EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1)
        );

        let adapter = adapter.with_wallet_query_endpoint(String::from("http://127.0.0.1:1"));
        assert!(
            adapter
                .advertised_capabilities()
                .contains(&EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1)
        );

        let mut chain_epoch = chain_fixture
            .chain_epoch(ChainEpochId::new(1))
            .ok_or("fixture chain epoch missing")?;
        chain_epoch.artifact_schema_version = ArtifactSchemaVersion::new(13);
        assert!(!intrinsic_value_balance_schema_supported(chain_epoch));
        chain_epoch.artifact_schema_version = ArtifactSchemaVersion::new(14);
        assert!(!intrinsic_value_balance_schema_supported(chain_epoch));
        chain_epoch.artifact_schema_version = ArtifactSchemaVersion::new(15);
        assert!(intrinsic_value_balance_schema_supported(chain_epoch));
        Ok(())
    }
}
