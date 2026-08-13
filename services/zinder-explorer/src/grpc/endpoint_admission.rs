//! Admission and health verification for the Explorer Wallet dependency.
//!
//! The operator-built Explorer composition owns a single authenticated Wallet
//! channel. This module freezes the discovery evidence obtained on that
//! channel, so later request handling cannot rediscover a different endpoint
//! contract.

use std::{sync::Arc, time::Duration};

use thiserror::Error;
use zinder_core::{
    Network, NetworkUpgradeActivations, NetworkUpgradeActivationsFingerprintVersion,
    wire::encode_zinder_native_chain_name,
};
use zinder_proto::{
    capabilities::{WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1, WALLET_READ_SERVER_INFO_V2},
    v1::wallet::{
        NetworkUpgradeActivationsRequest, ServerInfoRequest, ServerInfoResponse,
        wallet_query_client::WalletQueryClient,
    },
    wire::{
        decode_canonical_construction_manifest_binding, network_upgrade_activations_from_message,
    },
};
use zinder_runtime::{
    AuthenticatedChannel, BearerToken, BearerTokenConnectError, BearerTokenError,
    connect_zinder_grpc,
};
use zinder_store::{CanonicalConstructionManifestBinding, CanonicalStoreConstructionIdentity};

/// Maximum wall time allowed for Wallet connection and initial discovery.
const WALLET_QUERY_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum wall time allowed for one Wallet health probe.
const WALLET_QUERY_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// Minimum native Wallet contract revision accepted by this Explorer binary.
const MINIMUM_WALLET_QUERY_CONTRACT_REVISION: u32 = 5;

/// Immutable discovery evidence used to admit one Wallet endpoint.
#[derive(Clone)]
pub(super) struct AdmittedWalletQueryEndpoint {
    channel: AuthenticatedChannel,
    expected_network: Network,
    minimum_contract_revision: u32,
    capability_identifiers: Arc<[String]>,
    construction_manifest_binding: CanonicalConstructionManifestBinding,
}

impl AdmittedWalletQueryEndpoint {
    /// Connects to and admits one Wallet endpoint before Explorer binds traffic.
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
            let descriptor = validate_server_info(
                response,
                expected_network,
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
            )?;
            Ok(Self {
                channel,
                expected_network,
                minimum_contract_revision: MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                capability_identifiers: descriptor.capability_identifiers,
                construction_manifest_binding: descriptor.construction_manifest_binding,
            })
        })
        .await
        .map_err(|_| ExplorerEndpointAdmissionError::WalletAdmissionTimedOut { timeout })?
    }

    /// Returns a clone-cheap Wallet client over the admitted channel.
    pub(crate) fn wallet_client(&self) -> WalletQueryClient<AuthenticatedChannel> {
        WalletQueryClient::new(self.channel.clone())
    }

    /// Returns whether the admitted Wallet declared one exact capability.
    pub(crate) fn has_capability(&self, capability: &str) -> bool {
        self.capability_identifiers
            .binary_search_by(|candidate| candidate.as_str().cmp(capability))
            .is_ok()
    }

    /// Validates the admitted Wallet binding against a local view-store identity.
    pub(crate) fn require_matching_materialized_view_identity(
        &self,
        materialized_view_identity: CanonicalStoreConstructionIdentity,
    ) -> Result<(), ExplorerEndpointAdmissionError> {
        if self.expected_network != materialized_view_identity.network()
            || self.construction_manifest_binding
                != materialized_view_identity.construction_manifest_binding()
        {
            return Err(
                ExplorerEndpointAdmissionError::MaterializedViewConstructionIdentityMismatch {
                    wallet_network: self.expected_network,
                    materialized_view_network: materialized_view_identity.network(),
                    wallet_binding: self.construction_manifest_binding,
                    materialized_view_binding: materialized_view_identity
                        .construction_manifest_binding(),
                },
            );
        }
        Ok(())
    }

    /// Fetches optional activation evidence from the already-admitted Wallet.
    ///
    /// Absence of the Wallet capability is an intentional structural omission;
    /// it does not trigger a Node fallback.
    pub(crate) async fn network_upgrade_activations(
        &self,
        materialized_view_identity: CanonicalStoreConstructionIdentity,
    ) -> Result<Option<NetworkUpgradeActivations>, ExplorerEndpointAdmissionError> {
        if !self.has_capability(WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1) {
            return Ok(None);
        }
        let response = tokio::time::timeout(
            WALLET_QUERY_ADMISSION_TIMEOUT,
            self.wallet_client()
                .network_upgrade_activations(NetworkUpgradeActivationsRequest {}),
        )
        .await
        .map_err(
            |_| ExplorerEndpointAdmissionError::WalletActivationAdmissionTimedOut {
                timeout: WALLET_QUERY_ADMISSION_TIMEOUT,
            },
        )?
        .map_err(ExplorerEndpointAdmissionError::WalletNetworkUpgradeActivations)?
        .into_inner();
        validate_network_upgrade_activations_response(
            self.expected_network,
            materialized_view_identity,
            response,
        )
        .map(Some)
    }

    /// Verifies that the admitted Wallet still serves its frozen contract.
    pub(super) async fn check_health(&self) -> Result<(), ExplorerWalletQueryHealthError> {
        let response = tokio::time::timeout(
            WALLET_QUERY_HEALTH_TIMEOUT,
            self.wallet_client().server_info(ServerInfoRequest {}),
        )
        .await
        .map_err(|_| ExplorerWalletQueryHealthError::TimedOut {
            timeout: WALLET_QUERY_HEALTH_TIMEOUT,
        })?
        .map_err(ExplorerWalletQueryHealthError::Request)?
        .into_inner();
        let descriptor = validate_server_info(
            response,
            self.expected_network,
            self.minimum_contract_revision,
        )
        .map_err(ExplorerWalletQueryHealthError::ContractMismatch)?;
        validate_frozen_wallet_descriptor(
            self.capability_identifiers.as_ref(),
            self.construction_manifest_binding,
            &descriptor,
        )
    }
}

/// Failure observed while checking an admitted Wallet dependency.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExplorerWalletQueryHealthError {
    /// The traffic-gated Wallet discovery RPC did not succeed.
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

    /// A successful discovery response no longer satisfies admission.
    #[error("wallet query health check returned an incompatible contract: {0}")]
    ContractMismatch(#[source] ExplorerEndpointAdmissionError),

    /// A valid replacement endpoint changed its advertised capabilities.
    #[error("wallet query health check returned capabilities different from startup admission")]
    ContractChanged,

    /// A valid replacement endpoint changed its construction binding.
    #[error(
        "wallet query health check returned a construction binding different from startup admission"
    )]
    ConstructionBindingChanged,
}

impl ExplorerWalletQueryHealthError {
    /// Returns whether the peer answered with incompatible frozen evidence.
    #[must_use]
    pub const fn is_contract_mismatch(&self) -> bool {
        matches!(
            self,
            Self::ContractMismatch(_) | Self::ContractChanged | Self::ConstructionBindingChanged
        )
    }
}

/// Failure returned before Explorer may bind a listener.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExplorerEndpointAdmissionError {
    /// The configured endpoint is not a valid tonic endpoint URL.
    #[error("wallet query endpoint URL is invalid: {0}")]
    WalletEndpointInvalid(#[source] tonic::transport::Error),

    /// The configured endpoint could not be reached.
    #[error("wallet query endpoint is unreachable: {0}")]
    WalletEndpointUnreachable(#[source] tonic::transport::Error),

    /// The configured outbound bearer token cannot be encoded as metadata.
    #[error("wallet query bearer token is invalid: {0}")]
    WalletAuthorizationInvalid(#[source] BearerTokenError),

    /// Wallet discovery rejected the admission request.
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

    /// The Wallet activation-table request did not complete in time.
    #[error("wallet activation admission timed out after {timeout:?}")]
    WalletActivationAdmissionTimedOut {
        /// Maximum wall time allowed for the admitted activation request.
        timeout: Duration,
    },

    /// Wallet rejected its advertised activation-table method.
    #[error(
        "wallet query NetworkUpgradeActivations failed with {code:?}: {message}",
        code = .0.code(),
        message = .0.message()
    )]
    WalletNetworkUpgradeActivations(#[source] tonic::Status),

    /// Wallet returned a malformed activation table after advertising it.
    #[error("wallet query returned malformed activation evidence: {0}")]
    WalletNetworkUpgradeActivationsMalformed(#[source] zinder_proto::wire::WalletWireDecodeError),

    /// Wallet descriptor omitted the Wallet-specific metadata.
    #[error("wallet query ServerInfo response omitted info")]
    WalletInfoMissing,

    /// Wallet descriptor omitted the common endpoint identity.
    #[error("wallet query ServerInfo response omitted common identity")]
    WalletCommonIdentityMissing,

    /// Wallet descriptor omitted its required construction binding.
    #[error("wallet query ServerInfo response omitted canonical construction binding")]
    WalletConstructionBindingMissing,

    /// Wallet descriptor carried a malformed construction binding.
    #[error("wallet query ServerInfo construction binding is malformed: {0}")]
    WalletConstructionBindingMalformed(
        #[source] zinder_proto::wire::CanonicalConstructionManifestBindingDecodeError,
    ),

    /// Wallet endpoint serves a different network.
    #[error("wallet query network mismatch: expected {expected}, received {actual}")]
    WalletNetworkMismatch {
        /// Network configured for Explorer.
        expected: &'static str,
        /// Network advertised by Wallet.
        actual: String,
    },

    /// Wallet contract predates Explorer's compiled minimum.
    #[error("wallet query contract revision {actual} is older than required revision {minimum}")]
    WalletContractRevisionTooOld {
        /// Minimum revision understood by Explorer.
        minimum: u32,
        /// Revision advertised by Wallet.
        actual: u32,
    },

    /// Wallet answered discovery without claiming that discovery method.
    #[error("wallet query ServerInfo omitted required capability {WALLET_READ_SERVER_INFO_V2}")]
    WalletServerInfoCapabilityMissing,

    /// A capability identifier violated the protocol grammar.
    #[error(
        "wallet query ServerInfo capability at index {index} is not a lowercase dotted identifier ending in _vN"
    )]
    WalletCapabilityIdentifierMalformed {
        /// Zero-based position in the received capability list.
        index: usize,
    },

    /// Wallet and materialized-view store do not describe one construction.
    #[error("wallet and materialized-view store construction identities differ")]
    MaterializedViewConstructionIdentityMismatch {
        /// Network frozen at Wallet admission.
        wallet_network: Network,
        /// Network persisted by the materialized-view store.
        materialized_view_network: Network,
        /// Construction binding frozen at Wallet admission.
        wallet_binding: CanonicalConstructionManifestBinding,
        /// Construction binding persisted by the materialized-view store.
        materialized_view_binding: CanonicalConstructionManifestBinding,
    },

    /// Wallet activation evidence does not match the materialized-view identity.
    #[error("wallet activation-table fingerprint differs from materialized-view identity")]
    WalletActivationFingerprintMismatch {
        /// Activation fingerprint persisted by the materialized-view store.
        expected: zinder_core::NetworkUpgradeActivationsFingerprint,
        /// Activation fingerprint returned by Wallet.
        actual: zinder_core::NetworkUpgradeActivationsFingerprint,
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

struct WalletEndpointDescriptor {
    capability_identifiers: Arc<[String]>,
    construction_manifest_binding: CanonicalConstructionManifestBinding,
}

fn validate_frozen_wallet_descriptor(
    admitted_capability_identifiers: &[String],
    admitted_construction_manifest_binding: CanonicalConstructionManifestBinding,
    observed: &WalletEndpointDescriptor,
) -> Result<(), ExplorerWalletQueryHealthError> {
    if observed.capability_identifiers.as_ref() != admitted_capability_identifiers {
        return Err(ExplorerWalletQueryHealthError::ContractChanged);
    }
    if observed.construction_manifest_binding != admitted_construction_manifest_binding {
        return Err(ExplorerWalletQueryHealthError::ConstructionBindingChanged);
    }
    Ok(())
}

fn validate_server_info(
    response: ServerInfoResponse,
    expected_network: Network,
    minimum_contract_revision: u32,
) -> Result<WalletEndpointDescriptor, ExplorerEndpointAdmissionError> {
    let wallet_info = response
        .info
        .ok_or(ExplorerEndpointAdmissionError::WalletInfoMissing)?;
    let common = wallet_info
        .common
        .ok_or(ExplorerEndpointAdmissionError::WalletCommonIdentityMissing)?;
    let expected_network_name = encode_zinder_native_chain_name(expected_network);
    if common.network != expected_network_name {
        return Err(ExplorerEndpointAdmissionError::WalletNetworkMismatch {
            expected: expected_network_name,
            actual: common.network,
        });
    }
    if common.contract_revision < minimum_contract_revision {
        return Err(
            ExplorerEndpointAdmissionError::WalletContractRevisionTooOld {
                minimum: minimum_contract_revision,
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
    let binding = wallet_info
        .canonical_construction_manifest_binding
        .as_ref()
        .ok_or(ExplorerEndpointAdmissionError::WalletConstructionBindingMissing)?;
    let binding = decode_canonical_construction_manifest_binding(binding)
        .map_err(ExplorerEndpointAdmissionError::WalletConstructionBindingMalformed)?;
    let mut capability_identifiers = common.capabilities;
    capability_identifiers.sort_unstable();
    capability_identifiers.dedup();
    Ok(WalletEndpointDescriptor {
        capability_identifiers: capability_identifiers.into(),
        construction_manifest_binding: CanonicalConstructionManifestBinding {
            version: binding.format_version(),
            sha256: binding.sha256(),
        },
    })
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

fn validate_network_upgrade_activations_response(
    expected_network: Network,
    materialized_view_identity: CanonicalStoreConstructionIdentity,
    response: zinder_proto::v1::wallet::NetworkUpgradeActivationsResponse,
) -> Result<NetworkUpgradeActivations, ExplorerEndpointAdmissionError> {
    let activations = network_upgrade_activations_from_message(expected_network, response)
        .map_err(ExplorerEndpointAdmissionError::WalletNetworkUpgradeActivationsMalformed)?;
    let observed_fingerprint =
        activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::CURRENT);
    let expected_fingerprint = materialized_view_identity.network_upgrade_activations_fingerprint();
    if observed_fingerprint != expected_fingerprint {
        return Err(
            ExplorerEndpointAdmissionError::WalletActivationFingerprintMismatch {
                expected: expected_fingerprint,
                actual: observed_fingerprint,
            },
        );
    }
    Ok(activations)
}

#[cfg(test)]
mod tests {
    use zinder_proto::v1::{ops, wallet};
    use zinder_testkit::{
        published_regtest_canonical_construction_identity, sample_regtest_upgrade_activations,
    };

    use super::*;

    fn activation_response(
        activations: &NetworkUpgradeActivations,
    ) -> wallet::NetworkUpgradeActivationsResponse {
        wallet::NetworkUpgradeActivationsResponse {
            activations: activations
                .activations()
                .iter()
                .map(|activation| wallet::NetworkUpgradeActivation {
                    consensus_branch_id: activation.branch_id.value(),
                    name: activation.name.clone(),
                    activation_height: activation.activation_height.value(),
                })
                .collect(),
        }
    }

    fn server_info_response(identity: CanonicalStoreConstructionIdentity) -> ServerInfoResponse {
        let binding = identity.construction_manifest_binding();
        ServerInfoResponse {
            info: Some(wallet::WalletServerInfo {
                common: Some(ops::ServerInfo {
                    network: encode_zinder_native_chain_name(Network::ZcashRegtest).to_owned(),
                    capabilities: vec![WALLET_READ_SERVER_INFO_V2.to_owned()],
                    contract_revision: MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
                    ..Default::default()
                }),
                canonical_construction_manifest_binding: Some(
                    ops::CanonicalConstructionManifestBinding {
                        format_version: u32::from(binding.version),
                        sha256: binding.sha256.to_vec(),
                    },
                ),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn descriptor_validation_rejects_missing_and_malformed_structural_claims()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = published_regtest_canonical_construction_identity()?;
        let valid = server_info_response(identity);
        validate_server_info(
            valid.clone(),
            Network::ZcashRegtest,
            MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
        )?;

        let mut missing_binding = valid.clone();
        missing_binding
            .info
            .as_mut()
            .ok_or("wallet info missing")?
            .canonical_construction_manifest_binding = None;
        assert!(matches!(
            validate_server_info(
                missing_binding,
                Network::ZcashRegtest,
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
            ),
            Err(ExplorerEndpointAdmissionError::WalletConstructionBindingMissing)
        ));

        let mut malformed_capability = valid;
        malformed_capability
            .info
            .as_mut()
            .and_then(|info| info.common.as_mut())
            .ok_or("common info missing")?
            .capabilities
            .push("Wallet.Read.Bad_v01".to_owned());
        assert!(matches!(
            validate_server_info(
                malformed_capability,
                Network::ZcashRegtest,
                MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
            ),
            Err(ExplorerEndpointAdmissionError::WalletCapabilityIdentifierMalformed { .. })
        ));
        Ok(())
    }

    #[test]
    fn activation_admission_accepts_exact_evidence_and_rejects_malformed_or_different_tables()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = published_regtest_canonical_construction_identity()?;
        let exact = sample_regtest_upgrade_activations();
        assert_eq!(
            validate_network_upgrade_activations_response(
                Network::ZcashRegtest,
                identity,
                activation_response(&exact),
            )?,
            exact
        );

        let malformed = wallet::NetworkUpgradeActivationsResponse {
            activations: vec![wallet::NetworkUpgradeActivation {
                consensus_branch_id: 1,
                name: " ".to_owned(),
                activation_height: 1,
            }],
        };
        assert!(matches!(
            validate_network_upgrade_activations_response(
                Network::ZcashRegtest,
                identity,
                malformed,
            ),
            Err(ExplorerEndpointAdmissionError::WalletNetworkUpgradeActivationsMalformed(_))
        ));

        let different = wallet::NetworkUpgradeActivationsResponse {
            activations: vec![wallet::NetworkUpgradeActivation {
                consensus_branch_id: 1,
                name: "Different".to_owned(),
                activation_height: 1,
            }],
        };
        assert!(matches!(
            validate_network_upgrade_activations_response(
                Network::ZcashRegtest,
                identity,
                different,
            ),
            Err(ExplorerEndpointAdmissionError::WalletActivationFingerprintMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn frozen_health_descriptor_rejects_capability_or_construction_replacement()
    -> Result<(), Box<dyn std::error::Error>> {
        let first_identity = published_regtest_canonical_construction_identity()?;
        let second_identity = {
            let activations = sample_regtest_upgrade_activations();
            let fixture = zinder_testkit::WalletServingStoreFixture::from_chain(
                &zinder_testkit::ChainFixture::new(Network::ZcashRegtest).extend_blocks(2),
                &activations,
            )?;
            fixture.canonical_construction_identity()?
        };
        let admitted = validate_server_info(
            server_info_response(first_identity),
            Network::ZcashRegtest,
            MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
        )?;
        let same = validate_server_info(
            server_info_response(first_identity),
            Network::ZcashRegtest,
            MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
        )?;
        validate_frozen_wallet_descriptor(
            admitted.capability_identifiers.as_ref(),
            admitted.construction_manifest_binding,
            &same,
        )?;

        let replacement = validate_server_info(
            server_info_response(second_identity),
            Network::ZcashRegtest,
            MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
        )?;
        assert!(matches!(
            validate_frozen_wallet_descriptor(
                admitted.capability_identifiers.as_ref(),
                admitted.construction_manifest_binding,
                &replacement,
            ),
            Err(ExplorerWalletQueryHealthError::ConstructionBindingChanged)
        ));

        let mut changed_capabilities = server_info_response(first_identity);
        changed_capabilities
            .info
            .as_mut()
            .and_then(|info| info.common.as_mut())
            .ok_or("common info missing")?
            .capabilities
            .push("wallet.read.visible_tip_v1".to_owned());
        let changed_capabilities = validate_server_info(
            changed_capabilities,
            Network::ZcashRegtest,
            MINIMUM_WALLET_QUERY_CONTRACT_REVISION,
        )?;
        assert!(matches!(
            validate_frozen_wallet_descriptor(
                admitted.capability_identifiers.as_ref(),
                admitted.construction_manifest_binding,
                &changed_capabilities,
            ),
            Err(ExplorerWalletQueryHealthError::ContractChanged)
        ));
        Ok(())
    }
}
