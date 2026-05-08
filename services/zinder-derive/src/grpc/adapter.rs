//! `ExplorerQuery` gRPC adapter.
//!
//! Slice A serves a single RPC, [`ExplorerQuery::ServerInfo`], which advertises
//! the static capability [`DERIVE_EXPLORER_READY_CAPABILITY`] once the consumer
//! infrastructure is alive. Slice B layers `TransparentAddressBalance` onto the
//! same adapter.

use tonic::{Request, Response, Status};
use zinder_proto::v1::explorer::{
    ExplorerServerCapabilities, ServerInfoRequest, ServerInfoResponse,
    explorer_query_server::{ExplorerQuery, ExplorerQueryServer},
};

/// Capability string advertised once the derive consumer infrastructure is alive.
///
/// Slice A advertises this string unconditionally (since the infrastructure
/// is ready as soon as the binary boots and the explorer gRPC server
/// binds); Slice B will gate it on the balance accumulator's readiness
/// probe.
pub const DERIVE_EXPLORER_READY_CAPABILITY: &str = "derive.explorer.ready_v1";

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
/// The adapter is intentionally cheap to construct so the binary can spin
/// it up before consumer state is wired. Slice B will replace the inherent
/// [`Self::new`] constructor with one that takes a `DeriveStore` reference.
#[derive(Clone, Debug)]
pub struct ExplorerQueryGrpcAdapter {
    settings: ExplorerServerInfoSettings,
}

impl ExplorerQueryGrpcAdapter {
    /// Creates a new explorer-query adapter.
    #[must_use]
    pub const fn new(settings: ExplorerServerInfoSettings) -> Self {
        Self { settings }
    }

    /// Wraps the adapter into a tonic [`ExplorerQueryServer`] ready to be
    /// added to a `tonic::transport::Server` builder.
    #[must_use]
    pub fn into_server(self) -> ExplorerQueryServer<Self> {
        ExplorerQueryServer::new(self)
    }
}

#[tonic::async_trait]
impl ExplorerQuery for ExplorerQueryGrpcAdapter {
    async fn server_info(
        &self,
        _request: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        Ok(Response::new(ServerInfoResponse {
            capabilities: Some(ExplorerServerCapabilities {
                capabilities: vec![DERIVE_EXPLORER_READY_CAPABILITY.to_owned()],
                vendor: "Zinder".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                network: self.settings.network.clone(),
            }),
        }))
    }
}
