//! Startup admission for one concrete `ExplorerQuery` endpoint composition.

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use zinder_core::{Network, wire::encode_zinder_native_chain_name};
use zinder_materialized_views::MaterializedViewStoreError;
use zinder_proto::{
    capabilities::WALLET_READ_SERVER_INFO_V2,
    v1::wallet::{ServerInfoRequest, ServerInfoResponse, wallet_query_client::WalletQueryClient},
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenConnectError, BearerTokenError,
    connect_zinder_grpc,
};

/// Maximum wall time allowed for connection plus `ServerInfo` admission.
const WALLET_QUERY_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Oldest `WalletQuery` contract revision this Explorer implementation accepts.
///
/// This is deliberately independent of `zinder_proto::CONTRACT_REVISION`.
/// Explorer-only wire changes must not impose incidental lockstep on an
/// otherwise compatible `WalletQuery` dependency.
const MINIMUM_WALLET_QUERY_CONTRACT_REVISION: u32 = 5;

/// Maximum wall time allowed for one admitted `WalletQuery` health probe.
const WALLET_QUERY_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// A native wallet endpoint admitted before the explorer binds any listener.
///
/// Construction proves the endpoint is reachable through the shared
/// authenticated transport, serves the expected network and a compatible
/// contract revision, and advertises its own discovery method. The connected
/// channel and normalized capability set are immutable for the process lifetime.
#[derive(Clone)]
pub(super) struct AdmittedWalletQueryEndpoint {
    channel: AuthenticatedChannel,
    expected_network: Network,
    capability_identifiers: Arc<[String]>,
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
            let capability_identifiers = validate_server_info(response, expected_network)?;
            Ok(Self {
                channel,
                expected_network,
                capability_identifiers,
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
        &self.capability_identifiers
    }

    /// Verifies that the admitted channel still serves the frozen capabilities.
    pub(super) async fn check_health(&self) -> Result<(), ExplorerWalletQueryHealthError> {
        let response = tokio::time::timeout(
            WALLET_QUERY_HEALTH_TIMEOUT,
            WalletQueryClient::new(self.channel.clone()).server_info(ServerInfoRequest {}),
        )
        .await
        .map_err(|_| ExplorerWalletQueryHealthError::TimedOut {
            timeout: WALLET_QUERY_HEALTH_TIMEOUT,
        })?
        .map_err(ExplorerWalletQueryHealthError::Request)?
        .into_inner();
        validate_wallet_query_health_response(
            &self.capability_identifiers,
            response,
            self.expected_network,
        )
    }
}

/// Failure observed while checking an already admitted `WalletQuery` dependency.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExplorerWalletQueryHealthError {
    /// The traffic-gated `WalletQuery` discovery RPC did not succeed.
    #[error(
        "wallet query health check failed with {code:?}: {message}",
        code = .0.code(),
        message = .0.message()
    )]
    Request(#[source] tonic::Status),

    /// The discovery RPC exceeded its fixed health-check deadline.
    #[error("wallet query health check timed out after {timeout:?}")]
    TimedOut {
        /// Maximum wall time allowed for one health check.
        timeout: Duration,
    },

    /// A successful discovery response no longer satisfies startup admission.
    #[error("wallet query health check returned an incompatible contract: {0}")]
    ContractMismatch(#[source] ExplorerEndpointAdmissionError),

    /// A valid replacement endpoint changed its advertised capability semantics.
    #[error("wallet query health check returned capabilities different from startup admission")]
    ContractChanged,
}

impl ExplorerWalletQueryHealthError {
    /// Returns whether the peer answered successfully with incompatible identity.
    #[must_use]
    pub const fn is_contract_mismatch(&self) -> bool {
        matches!(self, Self::ContractMismatch(_) | Self::ContractChanged)
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

    /// The materialized-view store belongs to another network.
    #[error(
        "materialized-view store network mismatch: explorer expects {expected:?}, store serves {actual:?}"
    )]
    MaterializedViewStoreNetworkMismatch {
        /// Network configured for this explorer endpoint.
        expected: Network,
        /// Immutable network authenticated by the materialized-view store.
        actual: Network,
    },

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

    /// Active transparent-address ranking metadata could not be read.
    #[error("transparent-address ranking metadata admission failed: {0}")]
    TransparentAddressRankingMetadataRead(#[source] MaterializedViewStoreError),

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

    /// A capability identifier violated the lowercase dotted `_vN` grammar.
    #[error(
        "wallet query ServerInfo capability at index {index} is not a lowercase dotted identifier ending in _vN"
    )]
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
    if common.contract_revision < MINIMUM_WALLET_QUERY_CONTRACT_REVISION {
        return Err(
            ExplorerEndpointAdmissionError::WalletContractRevisionTooOld {
                minimum: MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                actual: common.contract_revision,
            },
        );
    }
    if let Some(index) = common
        .capabilities
        .iter()
        .position(|capability| !is_capability_identifier(capability))
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

fn validate_wallet_query_health_response(
    admitted_capability_identifiers: &[String],
    response: ServerInfoResponse,
    expected_network: Network,
) -> Result<(), ExplorerWalletQueryHealthError> {
    let observed_capability_identifiers = validate_server_info(response, expected_network)
        .map_err(ExplorerWalletQueryHealthError::ContractMismatch)?;
    if observed_capability_identifiers.as_ref() != admitted_capability_identifiers {
        return Err(ExplorerWalletQueryHealthError::ContractChanged);
    }
    Ok(())
}

fn is_capability_identifier(identifier: &str) -> bool {
    let Some((qualified_name, version)) = identifier.rsplit_once("_v") else {
        return false;
    };
    if version.is_empty()
        || version.starts_with('0')
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    let mut segments = qualified_name.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    let valid_segment = |segment: &str| {
        segment
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    valid_segment(first) && segments.clone().next().is_some() && segments.all(valid_segment)
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
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
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
    fn preserves_syntactically_valid_unknown_future_capabilities()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = validate_server_info(
            response_with(
                "zcash-regtest",
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                vec![WALLET_READ_SERVER_INFO_V2, "future.surface.new_contract_v7"],
            ),
            Network::ZcashRegtest,
        )?;

        assert!(capabilities.contains(&"future.surface.new_contract_v7".to_owned()));
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
                    MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
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
                    MINIMUM_WALLET_QUERY_CONTRACT_REVISION.saturating_sub(1),
                    vec![WALLET_READ_SERVER_INFO_V2],
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerEndpointAdmissionError::WalletContractRevisionTooOld {
                minimum: MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                actual,
            }) if actual == MINIMUM_WALLET_QUERY_CONTRACT_REVISION.saturating_sub(1)
        ));
    }

    #[test]
    fn rejects_missing_server_info_capability() {
        assert!(matches!(
            validate_server_info(
                response_with(
                    "zcash-regtest",
                    MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                    Vec::new(),
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerEndpointAdmissionError::WalletServerInfoCapabilityMissing)
        ));
    }

    #[test]
    fn rejects_malformed_capability_identifier() {
        for malformed in [
            "",
            " ",
            "wallet.read. visible_tip_block_v1",
            "Wallet.read.visible_tip_block_v1",
            "wallet-read-visible-tip-block_v1",
            "wallet.read.visible_tip_block",
            "wallet.read.visible_tip_block_v0",
            "wallet..visible_tip_block_v1",
        ] {
            assert!(matches!(
                validate_server_info(
                    response_with(
                        "zcash-regtest",
                        MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
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

    #[test]
    fn health_accepts_compatible_revision_and_rejects_capability_change()
    -> Result<(), Box<dyn std::error::Error>> {
        let admitted_response = || {
            response_with(
                "zcash-regtest",
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                vec![WALLET_READ_SERVER_INFO_V2, WALLET_READ_VISIBLE_TIP_BLOCK_V1],
            )
        };
        let admitted_capabilities =
            validate_server_info(admitted_response(), Network::ZcashRegtest)?;

        validate_wallet_query_health_response(
            &admitted_capabilities,
            response_with(
                "zcash-regtest",
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION.saturating_add(1),
                vec![WALLET_READ_SERVER_INFO_V2, WALLET_READ_VISIBLE_TIP_BLOCK_V1],
            ),
            Network::ZcashRegtest,
        )?;
        assert!(matches!(
            validate_wallet_query_health_response(
                &admitted_capabilities,
                response_with(
                    "zcash-regtest",
                    MINIMUM_WALLET_QUERY_CONTRACT_REVISION.saturating_sub(1),
                    vec![WALLET_READ_SERVER_INFO_V2, WALLET_READ_VISIBLE_TIP_BLOCK_V1,],
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerWalletQueryHealthError::ContractMismatch(
                ExplorerEndpointAdmissionError::WalletContractRevisionTooOld { .. }
            ))
        ));
        assert!(matches!(
            validate_wallet_query_health_response(
                &admitted_capabilities,
                response_with(
                    "zcash-regtest",
                    MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                    vec![WALLET_READ_SERVER_INFO_V2],
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerWalletQueryHealthError::ContractChanged)
        ));
        assert!(matches!(
            validate_wallet_query_health_response(
                &admitted_capabilities,
                response_with(
                    "zcash-testnet",
                    MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                    vec![WALLET_READ_SERVER_INFO_V2, WALLET_READ_VISIBLE_TIP_BLOCK_V1,],
                ),
                Network::ZcashRegtest,
            ),
            Err(ExplorerWalletQueryHealthError::ContractMismatch(
                ExplorerEndpointAdmissionError::WalletNetworkMismatch { .. }
            ))
        ));
        validate_wallet_query_health_response(
            &admitted_capabilities,
            admitted_response(),
            Network::ZcashRegtest,
        )?;
        Ok(())
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
