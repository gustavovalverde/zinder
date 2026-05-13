//! `ExplorerQuery` gRPC adapter.
//!
//! Serves [`ExplorerQuery::ServerInfo`] (advertising
//! [`DERIVE_EXPLORER_SERVER_INFO_V1`]) and
//! [`ExplorerQuery::TransparentAddressBalance`]. Balance reads compute at
//! request time per ADR-0014: confirmed totals are summed from canonical
//! transparent UTXO artifacts (via `WalletQuery`) and the mempool overlay is
//! composed from the live mempool point lookups (also via `WalletQuery`).
//! The derive plane owns no balance column family; the wire shape is the
//! durable contract.

use tonic::{Request, Response, Status, service::interceptor::InterceptedService};
use zinder_proto::capabilities::{
    DERIVE_EXPLORER_SERVER_INFO_V1, DERIVE_EXPLORER_TRANSPARENT_BALANCE_V1,
};
use zinder_proto::v1::{
    explorer::{
        ExplorerServerInfo, ServerInfoRequest, ServerInfoResponse,
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
    connect_authenticated_channel,
};

/// Settings the binary populates before constructing the adapter.
#[derive(Clone, Debug)]
pub struct ExplorerServerInfoSettings {
    /// Network the consumer mirrors, e.g. `zcash-mainnet`.
    pub network: String,
}

impl Default for ExplorerServerInfoSettings {
    fn default() -> Self {
        Self {
            network: "zcash-regtest".to_owned(),
        }
    }
}

/// Server adapter implementing `ExplorerQuery` for `zinder-derive`.
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
}

impl ExplorerQueryGrpcAdapter {
    /// Creates a new explorer-query adapter without a federated balance path.
    #[must_use]
    pub const fn new(settings: ExplorerServerInfoSettings) -> Self {
        Self {
            settings,
            wallet_query_endpoint: None,
            wallet_query_bearer_token: None,
            bearer_token: None,
        }
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

    /// Wraps the adapter into a tonic [`ExplorerQueryServer`] ready to be
    /// added to a `tonic::transport::Server` builder.
    #[must_use]
    pub fn into_server(
        self,
    ) -> InterceptedService<ExplorerQueryServer<Self>, BearerTokenServerInterceptor> {
        let interceptor = BearerTokenServerInterceptor::new(self.bearer_token.clone());
        ExplorerQueryServer::with_interceptor(self, interceptor)
    }

    fn advertised_capabilities(&self) -> Vec<String> {
        let mut capabilities = vec![DERIVE_EXPLORER_SERVER_INFO_V1.to_owned()];
        if self.wallet_query_endpoint.is_some() {
            capabilities.push(DERIVE_EXPLORER_TRANSPARENT_BALANCE_V1.to_owned());
        }
        capabilities
    }
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
                    network: self.settings.network.clone(),
                    service_name: env!("CARGO_PKG_NAME").to_owned(),
                    service_version: env!("CARGO_PKG_VERSION").to_owned(),
                    capabilities: self.advertised_capabilities(),
                }),
                vendor: "Zinder".to_owned(),
            }),
        }))
    }

    async fn transparent_address_balance(
        &self,
        request: Request<TransparentAddressBalanceRequest>,
    ) -> Result<Response<TransparentAddressBalanceResponse>, Status> {
        let endpoint = self.wallet_query_endpoint.as_deref().ok_or_else(|| {
            Status::unavailable(
                "TransparentAddressBalance requires a wallet_query_endpoint; \
                 configure --wallet-query-endpoint",
            )
        })?;
        let request_inner = request.into_inner();
        if request_inner.addresses.is_empty() {
            return Err(Status::invalid_argument("addresses list must not be empty"));
        }

        let mut client =
            connect_wallet_query(endpoint, self.wallet_query_bearer_token.as_ref()).await?;
        compute_transparent_address_balance(&mut client, request_inner)
            .await
            .map(Response::new)
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

async fn connect_wallet_query(
    endpoint: &str,
    bearer_token: Option<&BearerToken>,
) -> Result<WalletQueryClient<AuthenticatedChannel>, Status> {
    let channel = connect_authenticated_channel(endpoint, bearer_token)
        .await
        .map_err(connect_error_to_status)?;
    Ok(WalletQueryClient::new(channel))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "BearerTokenConnectError is moved out of the Result by the caller; the helper takes ownership"
)]
fn connect_error_to_status(error: BearerTokenConnectError) -> Status {
    Status::unavailable(format!("WalletQuery endpoint unreachable: {error}"))
}
