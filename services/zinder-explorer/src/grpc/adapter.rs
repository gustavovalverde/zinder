//! `ExplorerQuery` gRPC adapter.
//!
//! Serves [`ExplorerQuery::ServerInfo`] (advertising
//! [`EXPLORER_SERVER_INFO_V1`]) and
//! [`ExplorerQuery::TransparentAddressBalance`]. Balance reads compute at
//! request time: confirmed totals are summed from canonical
//! transparent UTXO artifacts (via `WalletQuery`) and the mempool overlay is
//! composed from the live mempool point lookups (also via `WalletQuery`).
//! The explorer plane owns no balance column family; the wire shape is the
//! durable contract.
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
use tonic::{Code, Request, Response, Status, service::interceptor::InterceptedService};
use zinder_core::{Network, wire::encode_zinder_native_chain_name};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_EVENT_COUNTS_V1, EXPLORER_MEMPOOL_SUMMARY_V1,
    EXPLORER_PAYMENT_DISCLOSURE_VERIFY_V1, EXPLORER_SEARCH_V1, EXPLORER_SERVER_INFO_V1,
    EXPLORER_TRANSACTION_DETAIL_V1, EXPLORER_TRANSACTION_FEES_V1, EXPLORER_TRANSACTION_RECENT_V1,
    EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1, EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
    EXPLORER_VALUE_POOL_SUMMARY_V1,
};
use zinder_proto::v1::{
    explorer::{
        BlockDetailRequest, BlockDetailResponse, BlockSummariesInRangeRequest,
        BlockSummariesInRangeResponse, ExplorerServerInfo, FeeSummaryRequest, FeeSummaryResponse,
        MempoolActivityRequest, MempoolActivityResponse, MempoolEventCountsRequest,
        MempoolEventCountsResponse, MempoolSummaryRequest, MempoolSummaryResponse,
        RecentTransactionsRequest, SearchRequest, SearchResponse, ServerInfoRequest,
        ServerInfoResponse, TransactionDetailRequest, TransactionDetailResponse,
        TransparentAddressActivityRequest, TransparentAddressActivityResponse,
        ValuePoolSummaryRequest, ValuePoolSummaryResponse, VerifyPaymentDisclosureRequest,
        VerifyPaymentDisclosureResponse,
        explorer_query_server::{ExplorerQuery, ExplorerQueryServer},
    },
    ops,
    wallet::{
        self, TransparentAddressBalanceRequest, TransparentAddressBalanceResponse,
        wallet_query_client::WalletQueryClient,
    },
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenConnectError, BearerTokenServerInterceptor,
    RpcMetricNames, RpcOutcome, connect_zinder_grpc, describe_rpc_metrics, record_rpc_request,
};

/// Metric pair the `ExplorerQuery` adapter emits per request.
const EXPLORER_RPC_METRICS: RpcMetricNames = RpcMetricNames::for_service(
    "zinder_explorer_request_duration_seconds",
    "zinder_explorer_request_total",
);

use super::block_view::{handle_block_detail, handle_block_summaries_in_range};
use super::fee_summary::handle_fee_summary;
use super::mempool::{handle_mempool_activity, handle_mempool_summary};
use super::mempool_event_counts::handle_mempool_event_counts;
use super::recent_transactions::{RecentTransactionsStream, handle_recent_transactions};
use super::search::handle_search;
use super::transaction_detail::handle_transaction_detail;
use super::transparent_address_activity::handle_transparent_address_activity;
use super::value_pool_summary::handle_value_pool_summary;
use zinder_derive::DeriveStore;

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
#[derive(Clone, Debug)]
pub struct ExplorerQueryGrpcAdapter {
    settings: ExplorerServerInfoSettings,
    wallet_query_endpoint: Option<String>,
    wallet_query_bearer_token: Option<BearerToken>,
    bearer_token: Option<BearerToken>,
    derive_store: Option<DeriveStore>,
    wallet_channel: Arc<OnceCell<AuthenticatedChannel>>,
    prevout_resolution_online: bool,
    payment_disclosure_verifier_online: bool,
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
            derive_store: None,
            wallet_channel: Arc::new(OnceCell::new()),
            prevout_resolution_online: false,
            payment_disclosure_verifier_online: false,
        }
    }

    /// Wires the consumer-side derive store so block-view RPCs can read the
    /// materialized `BlockSummary` records.
    #[must_use]
    pub fn with_derive_store(mut self, store: DeriveStore) -> Self {
        self.derive_store = Some(store);
        self
    }

    /// Configures the `WalletQuery` endpoint the balance handler reads from.
    ///
    /// The same endpoint serves canonical transparent UTXOs and the live
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

    /// Marks transparent prevout resolution as online so the adapter
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

    /// Wraps the adapter into a tonic [`ExplorerQueryServer`] ready to be
    /// added to a `tonic::transport::Server` builder.
    #[must_use]
    pub fn into_server(
        self,
    ) -> InterceptedService<ExplorerQueryServer<Self>, BearerTokenServerInterceptor> {
        let interceptor = BearerTokenServerInterceptor::new(self.bearer_token.clone());
        ExplorerQueryServer::with_interceptor(self, interceptor)
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
        let derive_store_online = self.derive_store.is_some();
        let prevout_resolution_online = self.prevout_resolution_online && wallet_query_online;
        let payment_disclosure_verifier_online = self.payment_disclosure_verifier_online;

        let mut capabilities = vec![EXPLORER_SERVER_INFO_V1];
        if wallet_query_online {
            capabilities.push(EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1);
            capabilities.push(EXPLORER_TRANSACTION_DETAIL_V1);
            capabilities.push(EXPLORER_SEARCH_V1);
            capabilities.push(EXPLORER_MEMPOOL_SUMMARY_V1);
            capabilities.push(EXPLORER_MEMPOOL_ACTIVITY_V1);
            capabilities.push(EXPLORER_FEE_SUMMARY_V1);
            capabilities.push(EXPLORER_VALUE_POOL_SUMMARY_V1);
        }
        if derive_store_online && wallet_query_online {
            capabilities.push(EXPLORER_BLOCK_SUMMARY_V1);
            capabilities.push(EXPLORER_BLOCK_DETAIL_V1);
            capabilities.push(EXPLORER_MEMPOOL_EVENT_COUNTS_V1);
            capabilities.push(EXPLORER_TRANSACTION_RECENT_V1);
            capabilities.push(EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1);
            // ADR-0018: TRANSACTION_FEES_V1 surfaces paid_fee_zat on
            // TransactionDetail / RecentTransactions / MempoolActivity,
            // all of which need the transparent-prevouts upstream to be
            // online. Advertise only when both gates are satisfied.
            if prevout_resolution_online {
                capabilities.push(EXPLORER_TRANSACTION_FEES_V1);
            }
        }
        if payment_disclosure_verifier_online {
            capabilities.push(EXPLORER_PAYMENT_DISCLOSURE_VERIFY_V1);
        }
        capabilities
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
        Ok(Response::new(ServerInfoResponse {
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
                }),
                vendor: "Zinder".to_owned(),
            }),
        }))
    }

    async fn transparent_address_balance(
        &self,
        request: Request<TransparentAddressBalanceRequest>,
    ) -> Result<Response<TransparentAddressBalanceResponse>, Status> {
        const OP: OperationNames = OperationNames {
            method: "TransparentAddressBalance",
            metric: "transparent_address_balance",
        };
        let started = Instant::now();
        let outcome = async {
            self.require_wallet_endpoint(OP.method)?;
            let request_inner = request.into_inner();
            if request_inner.addresses.is_empty() {
                return Err(Status::invalid_argument("addresses list must not be empty"));
            }
            let mut client = self.wallet_client(OP.method).await?;
            compute_transparent_address_balance(&mut client, request_inner)
                .await
                .map(Response::new)
        }
        .await;
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
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
                self.derive_store.as_ref(),
                self.settings.network,
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
            handle_block_summaries_in_range(derive_store, &mut client, request).await
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
            handle_block_detail(derive_store, &mut client, request).await
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
            handle_mempool_summary(&mut client, self.settings.network, request).await
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
            handle_mempool_activity(&mut client, self.settings.network, request).await
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
                Status::unavailable("TransparentAddressActivity requires a derive store")
            })?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_transparent_address_activity(
                derive_store,
                &mut client,
                self.settings.network,
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
            let mut client = self.wallet_client(OP.method).await?;
            handle_fee_summary(&mut client, self.settings.network, request).await
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
            handle_value_pool_summary(&mut client, request).await
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
            let derive_store = self
                .derive_store
                .as_ref()
                .ok_or_else(|| Status::unavailable("MempoolEventCounts requires a derive store"))?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_mempool_event_counts(derive_store, &mut client, request).await
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
            let derive_store = self
                .derive_store
                .as_ref()
                .ok_or_else(|| Status::unavailable("RecentTransactions requires a derive store"))?;
            let mut client = self.wallet_client(OP.method).await?;
            handle_recent_transactions(derive_store, &mut client, request).await
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
            Err(Status::failed_precondition(
                "VerifyPaymentDisclosure is wired but no ZIP-311 verifier is bundled in this build",
            ))
        } else {
            Err(Status::unimplemented(
                "VerifyPaymentDisclosure is disabled on this server; consumer must fall back to local verification",
            ))
        };
        record_explorer_request(OP.metric, started.elapsed(), outcome.as_ref().err());
        outcome
    }
}

impl ExplorerQueryGrpcAdapter {
    fn require_derive_store(&self, method: &'static str) -> Result<&DeriveStore, Status> {
        self.derive_store.as_ref().ok_or_else(|| {
            Status::unavailable(format!(
                "{method} requires the BlockSummary derive view; configure \
                 --storage-path and start the explorer with the consumer wired"
            ))
        })
    }

    fn require_wallet_endpoint(&self, method: &'static str) -> Result<&str, Status> {
        self.wallet_query_endpoint.as_deref().ok_or_else(|| {
            Status::unavailable(format!(
                "{method} requires a wallet_query_endpoint; configure \
                 --wallet-query-endpoint"
            ))
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

/// Hard cap on the number of addresses one balance request may sum across.
///
/// Shape C reads canonical UTXOs and the mempool overlay per address, so an
/// unbounded list would let one request fan out into thousands of
/// `WalletQuery` round-trips. The cap mirrors the bounded-page rule used by
/// the rest of the transparent-address surface. `u32` matches the
/// `address_count` field on the response so the bound check happens in the
/// wire type's native width.
const MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES: u32 = 256;

async fn compute_transparent_address_balance(
    client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: TransparentAddressBalanceRequest,
) -> Result<TransparentAddressBalanceResponse, Status> {
    let address_count = u32::try_from(request.addresses.len())
        .ok()
        .filter(|count| *count <= MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES)
        .ok_or_else(|| {
            Status::invalid_argument(format!(
                "addresses list of {} exceeds the per-request cap of {}",
                request.addresses.len(),
                MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES,
            ))
        })?;
    let at_epoch = request.at_epoch.clone();

    let mut confirmed_zat: u64 = 0;
    let mut unconfirmed_delta_zat: i64 = 0;
    let mut chain_epoch: Option<wallet::ChainEpoch> = None;

    for address_lookup in request.addresses {
        let utxos_response = client
            .transparent_address_utxos(Request::new(wallet::TransparentAddressUtxosRequest {
                address: Some(address_lookup.clone()),
                max_entries: None,
                from_cursor: Vec::new(),
                at_epoch: at_epoch.clone(),
                start_height: 0,
            }))
            .await?
            .into_inner();
        if chain_epoch.is_none() {
            chain_epoch.clone_from(&utxos_response.chain_epoch);
        }

        let mempool_outputs = client
            .transparent_mempool_outputs_by_address(Request::new(
                wallet::TransparentMempoolOutputsByAddressRequest {
                    address: Some(address_lookup),
                    max_entries: None,
                },
            ))
            .await?
            .into_inner();
        if chain_epoch.is_none() {
            chain_epoch.clone_from(&mempool_outputs.chain_epoch);
        }

        for utxo in &utxos_response.utxos {
            confirmed_zat = confirmed_zat.saturating_add(utxo.value_zat);
        }
        for output in &mempool_outputs.outputs {
            unconfirmed_delta_zat =
                unconfirmed_delta_zat.saturating_add(value_zat_to_signed(output.value_zat)?);
        }

        for utxo in &utxos_response.utxos {
            let utxo_outpoint = utxo.outpoint.clone().ok_or_else(|| {
                Status::data_loss("TransparentAddressUtxo.outpoint missing in WalletQuery response")
            })?;
            let spend_response = client
                .transparent_mempool_spend_by_outpoint(Request::new(
                    wallet::TransparentMempoolSpendByOutpointRequest {
                        outpoint: Some(utxo_outpoint),
                    },
                ))
                .await?
                .into_inner();
            if spend_response.spend.is_some() {
                unconfirmed_delta_zat =
                    unconfirmed_delta_zat.saturating_sub(value_zat_to_signed(utxo.value_zat)?);
            }
        }
    }

    let chain_epoch = chain_epoch.ok_or_else(|| {
        Status::internal("WalletQuery did not return a chain epoch for any address")
    })?;
    Ok(TransparentAddressBalanceResponse {
        confirmed_zat,
        unconfirmed_delta_zat,
        address_count,
        chain_epoch: Some(chain_epoch),
    })
}

/// Converts a wire `u64` Zatoshi value to the signed accumulator width.
///
/// Zcash's hardcoded supply cap (`MAX_MONEY = 21,000,000 * 10^8` zat) fits
/// well inside `i64::MAX`, so a `u64` value that does not fit is upstream
/// data corruption and surfaces as `data_loss` rather than silent saturation.
fn value_zat_to_signed(value_zat: u64) -> Result<i64, Status> {
    i64::try_from(value_zat).map_err(|_| {
        Status::data_loss(format!(
            "WalletQuery returned value_zat {value_zat} exceeding i64::MAX"
        ))
    })
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "BearerTokenConnectError is moved out of the Result by the caller; the helper takes ownership"
)]
fn connect_error_to_status(error: BearerTokenConnectError) -> Status {
    Status::unavailable(format!("WalletQuery endpoint unreachable: {error}"))
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
