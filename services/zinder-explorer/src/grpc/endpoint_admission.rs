//! Startup admission for one concrete `ExplorerQuery` endpoint composition.

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use zinder_core::{Network, wire::encode_zinder_native_chain_name};
use zinder_proto::{
    CONTRACT_REVISION,
    capabilities::WALLET_READ_SERVER_INFO_V2,
    v1::wallet::{ServerInfoRequest, ServerInfoResponse, wallet_query_client::WalletQueryClient},
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenConnectError, BearerTokenError,
    connect_zinder_grpc,
};

/// Maximum wall time allowed for connection plus `ServerInfo` admission.
const WALLET_QUERY_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);

/// A native wallet endpoint admitted before the explorer binds any listener.
///
/// Construction proves the endpoint is reachable through the shared
/// authenticated transport, serves the expected network and compiled contract,
/// and advertises its own discovery method. The connected channel and normalized
/// capability set are immutable for the process lifetime.
#[derive(Clone)]
pub(super) struct AdmittedWalletQueryEndpoint {
    channel: AuthenticatedChannel,
    capabilities: Arc<[String]>,
}

impl AdmittedWalletQueryEndpoint {
    /// Connects to and admits one native wallet endpoint.
    pub(super) async fn admit(
        endpoint: &str,
        bearer_token: Option<&BearerToken>,
        expected_network: Network,
    ) -> Result<Self, ExplorerEndpointAdmissionError> {
        Self::admit_with_timeout(
            endpoint,
            bearer_token,
            expected_network,
            WALLET_QUERY_ADMISSION_TIMEOUT,
        )
        .await
    }

    async fn admit_with_timeout(
        endpoint: &str,
        bearer_token: Option<&BearerToken>,
        expected_network: Network,
        timeout: Duration,
    ) -> Result<Self, ExplorerEndpointAdmissionError> {
        tokio::time::timeout(timeout, async {
            let channel = connect_zinder_grpc(endpoint, bearer_token)
                .await
                .map_err(ExplorerEndpointAdmissionError::from_wallet_connect_error)?;
            let response = WalletQueryClient::new(channel.clone())
                .server_info(ServerInfoRequest {})
                .await
                .map_err(ExplorerEndpointAdmissionError::WalletServerInfo)?
                .into_inner();
            let capabilities = validate_server_info(response, expected_network)?;
            Ok(Self {
                channel,
                capabilities,
            })
        })
        .await
        .map_err(|_| ExplorerEndpointAdmissionError::WalletAdmissionTimedOut { timeout })?
    }

    /// Builds a clone-cheap wallet client over the already admitted channel.
    pub(crate) fn wallet_client(&self) -> WalletQueryClient<AuthenticatedChannel> {
        WalletQueryClient::new(self.channel.clone())
    }

    /// Returns the normalized upstream capability identifiers.
    pub(crate) fn capability_identifiers(&self) -> &[String] {
        &self.capabilities
    }
}

/// Failure returned before an explorer endpoint composition may bind.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExplorerEndpointAdmissionError {
    /// Outbound wallet authorization was configured without a wallet endpoint.
    #[error("wallet query authorization requires a configured wallet query endpoint")]
    WalletAuthorizationRequiresEndpoint,

    /// The canonical secondary belongs to another network.
    #[error(
        "canonical secondary network mismatch: explorer expects {expected:?}, store serves {actual:?}"
    )]
    CanonicalStoreNetworkMismatch {
        /// Network configured for this explorer endpoint.
        expected: Network,
        /// Immutable network authenticated by the canonical secondary.
        actual: Network,
    },

    /// The canonical secondary was opened without an admitted network.
    #[error("canonical secondary has no admitted network identity")]
    CanonicalStoreNetworkUnspecified,

    /// The node-advertised activation table belongs to another network.
    #[error(
        "network-upgrade activation table mismatch: explorer expects {expected:?}, table describes {actual:?}"
    )]
    NetworkUpgradeActivationsNetworkMismatch {
        /// Network configured for this explorer endpoint.
        expected: Network,
        /// Network carried by the activation table.
        actual: Network,
    },

    /// The configured endpoint is not a valid tonic endpoint URL.
    #[error("wallet query endpoint URL is invalid: {0}")]
    WalletEndpointInvalid(#[source] tonic::transport::Error),

    /// The configured endpoint could not be reached.
    #[error("wallet query endpoint is unreachable: {0}")]
    WalletEndpointUnreachable(#[source] tonic::transport::Error),

    /// The configured outbound bearer token cannot be encoded as metadata.
    #[error("wallet query bearer token is invalid: {0}")]
    WalletAuthorizationInvalid(#[source] BearerTokenError),

    /// `WalletQuery.ServerInfo` rejected or failed the admission request.
    #[error(
        "wallet query ServerInfo failed with {code:?}: {message}",
        code = .0.code(),
        message = .0.message()
    )]
    WalletServerInfo(#[source] tonic::Status),

    /// Connection or discovery did not complete within the admission bound.
    #[error("wallet query admission timed out after {timeout:?}")]
    WalletAdmissionTimedOut {
        /// Maximum wall time allowed for admission.
        timeout: Duration,
    },

    /// The response omitted the wallet-specific descriptor.
    #[error("wallet query ServerInfo response omitted info")]
    WalletInfoMissing,

    /// The wallet descriptor omitted the common endpoint identity.
    #[error("wallet query ServerInfo response omitted common identity")]
    WalletCommonIdentityMissing,

    /// The wallet endpoint serves another network.
    #[error("wallet query network mismatch: expected {expected}, received {actual}")]
    WalletNetworkMismatch {
        /// Network configured for this explorer process.
        expected: &'static str,
        /// Network advertised by the wallet endpoint.
        actual: String,
    },

    /// The wallet contract predates the explorer's compiled minimum.
    #[error("wallet query contract revision {actual} is older than required revision {minimum}")]
    WalletContractRevisionTooOld {
        /// Minimum revision understood by this explorer binary.
        minimum: u32,
        /// Revision advertised by the wallet endpoint.
        actual: u32,
    },

    /// The endpoint answered discovery without claiming that discovery method.
    #[error("wallet query ServerInfo omitted required capability {WALLET_READ_SERVER_INFO_V2}")]
    WalletServerInfoCapabilityMissing,

    /// A capability identifier was empty or contained whitespace.
    #[error("wallet query ServerInfo capability at index {index} is empty or contains whitespace")]
    WalletCapabilityIdentifierMalformed {
        /// Zero-based position in the received capability list.
        index: usize,
    },
}

impl ExplorerEndpointAdmissionError {
    fn from_wallet_connect_error(error: BearerTokenConnectError) -> Self {
        match error {
            BearerTokenConnectError::InvalidEndpoint(source) => Self::WalletEndpointInvalid(source),
            BearerTokenConnectError::Transport(source) => Self::WalletEndpointUnreachable(source),
            BearerTokenConnectError::Token(source) => Self::WalletAuthorizationInvalid(source),
        }
    }
}

fn validate_server_info(
    response: ServerInfoResponse,
    expected_network: Network,
) -> Result<Arc<[String]>, ExplorerEndpointAdmissionError> {
    let wallet_info = response
        .info
        .ok_or(ExplorerEndpointAdmissionError::WalletInfoMissing)?;
    let common = wallet_info
        .common
        .ok_or(ExplorerEndpointAdmissionError::WalletCommonIdentityMissing)?;
    let expected_network = encode_zinder_native_chain_name(expected_network);
    if common.network != expected_network {
        return Err(ExplorerEndpointAdmissionError::WalletNetworkMismatch {
            expected: expected_network,
            actual: common.network,
        });
    }
    if common.contract_revision < CONTRACT_REVISION {
        return Err(
            ExplorerEndpointAdmissionError::WalletContractRevisionTooOld {
                minimum: CONTRACT_REVISION,
                actual: common.contract_revision,
            },
        );
    }
    if let Some(index) = common
        .capabilities
        .iter()
        .position(|capability| capability.is_empty() || capability.chars().any(char::is_whitespace))
    {
        return Err(ExplorerEndpointAdmissionError::WalletCapabilityIdentifierMalformed { index });
    }
    if !common
        .capabilities
        .iter()
        .any(|capability| capability == WALLET_READ_SERVER_INFO_V2)
    {
        return Err(ExplorerEndpointAdmissionError::WalletServerInfoCapabilityMissing);
    }
    let mut capabilities = common.capabilities;
    capabilities.sort_unstable();
    capabilities.dedup();
    Ok(capabilities.into())
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the admission behavior under test."
    )]

    use tokio::net::TcpListener;
    use zinder_proto::{
        capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1,
        v1::{ops, wallet::WalletServerInfo},
    };

    use super::*;

    fn response_with(
        network: &str,
        contract_revision: u32,
        capabilities: Vec<&str>,
    ) -> ServerInfoResponse {
        ServerInfoResponse {
            info: Some(WalletServerInfo {
                common: Some(ops::ServerInfo {
                    network: network.to_owned(),
                    service_name: "wallet-query-test".to_owned(),
                    service_version: "0.0.0".to_owned(),
                    build_git_commit: "test".to_owned(),
                    contract_revision,
                    capabilities: capabilities.into_iter().map(str::to_owned).collect(),
                    materialized_view_preset: String::new(),
                    materialized_view_identities: Vec::new(),
                }),
                schema_version: 1,
                reorg_window_blocks: 100,
                node: None,
            }),
        }
    }

    #[test]
    fn normalizes_capabilities_after_validating_identity() -> Result<(), Box<dyn std::error::Error>>
    {
        let capabilities = validate_server_info(
            response_with(
                "zcash-regtest",
                CONTRACT_REVISION,
                vec![
                    WALLET_READ_VISIBLE_TIP_BLOCK_V1,
                    WALLET_READ_SERVER_INFO_V2,
                    WALLET_READ_SERVER_INFO_V2,
                ],
            ),
            Network::ZcashRegtest,
        )?;

        assert_eq!(
            capabilities.as_ref(),
            &[
                WALLET_READ_SERVER_INFO_V2.to_owned(),
                WALLET_READ_VISIBLE_TIP_BLOCK_V1.to_owned(),
            ]
        );
        Ok(())
    }

    #[test]
    fn rejects_missing_wallet_descriptor() {
        assert!(matches!(
            validate_server_info(ServerInfoResponse { info: None }, Network::ZcashRegtest),
            Err(ExplorerEndpointAdmissionError::WalletInfoMissing)
        ));
    }

    #[test]
    fn rejects_missing_common_identity() {
        assert!(matches!(
            validate_server_info(
                ServerInfoResponse {
                    info: Some(WalletServerInfo {
                        common: None,
                        schema_version: 1,
                        reorg_window_blocks: 100,
                        node: None,
                    }),
                },
                Network::ZcashRegtest,
            ),
            Err(ExplorerEndpointAdmissionError::WalletCommonIdentityMissing)
        ));
    }

    #[test]
    fn rejects_network_mismatch() {
        assert!(matches!(
            validate_server_info(
                response_with(
                    "zcash-testnet",
                    CONTRACT_REVISION,
                    vec![WALLET_READ_SERVER_INFO_V2],
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerEndpointAdmissionError::WalletNetworkMismatch {
                expected: "zcash-regtest",
                actual,
            }) if actual == "zcash-testnet"
        ));
    }

    #[test]
    fn rejects_contract_revision_below_compiled_minimum() {
        assert!(matches!(
            validate_server_info(
                response_with(
                    "zcash-regtest",
                    CONTRACT_REVISION.saturating_sub(1),
                    vec![WALLET_READ_SERVER_INFO_V2],
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerEndpointAdmissionError::WalletContractRevisionTooOld {
                minimum: CONTRACT_REVISION,
                actual,
            }) if actual == CONTRACT_REVISION.saturating_sub(1)
        ));
    }

    #[test]
    fn rejects_missing_server_info_capability() {
        assert!(matches!(
            validate_server_info(
                response_with("zcash-regtest", CONTRACT_REVISION, Vec::new()),
                Network::ZcashRegtest,
            ),
            Err(ExplorerEndpointAdmissionError::WalletServerInfoCapabilityMissing)
        ));
    }

    #[test]
    fn rejects_empty_or_whitespace_capability_identifier() {
        for malformed in ["", " ", "wallet.read. visible_tip_block_v1"] {
            assert!(matches!(
                validate_server_info(
                    response_with(
                        "zcash-regtest",
                        CONTRACT_REVISION,
                        vec![WALLET_READ_SERVER_INFO_V2, malformed],
                    ),
                    Network::ZcashRegtest,
                ),
                Err(
                    ExplorerEndpointAdmissionError::WalletCapabilityIdentifierMalformed {
                        index: 1
                    }
                )
            ));
        }
    }

    #[tokio::test]
    async fn bounds_a_peer_that_accepts_tcp_without_serving_grpc()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let peer = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await?;
            std::future::pending::<Result<(), std::io::Error>>().await
        });
        let timeout = Duration::from_millis(20);

        let result = AdmittedWalletQueryEndpoint::admit_with_timeout(
            &endpoint,
            None,
            Network::ZcashRegtest,
            timeout,
        )
        .await;
        peer.abort();

        assert!(matches!(
            result,
            Err(ExplorerEndpointAdmissionError::WalletAdmissionTimedOut {
                timeout: actual,
            }) if actual == timeout
        ));
        Ok(())
    }
}
