//! Startup contract admission for native Zinder query dependencies.

use std::fmt;

use thiserror::Error;
use zinder_core::{Network, wire::encode_zinder_native_chain_name};
use zinder_proto::CONTRACT_REVISION;
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_PRODUCTION_SERIES_V2, EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V4,
    WALLET_EVENTS_CHAIN_V1, WALLET_EVENTS_MEMPOOL_V2, WALLET_READ_SERVER_INFO_V2,
    WALLET_READ_TRANSACTION_BY_ID_V2, WALLET_READ_TRANSACTION_BYTES_V1,
};
use zinder_proto::v1::{
    explorer::{self, explorer_query_client::ExplorerQueryClient},
    wallet::{self, wallet_query_client::WalletQueryClient},
};
use zinder_runtime::{AuthenticatedChannel, RuntimeService};

const REQUIRED_EXPLORER_CAPABILITIES: [&str; 2] =
    [EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V4];
const REQUIRED_WALLET_CAPABILITIES: [&str; 3] = [
    WALLET_READ_SERVER_INFO_V2,
    WALLET_READ_TRANSACTION_BY_ID_V2,
    WALLET_READ_TRANSACTION_BYTES_V1,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UpstreamAdmission {
    pub(crate) realtime_websocket_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpstreamRole {
    ExplorerQuery,
    WalletQuery,
}

impl fmt::Display for UpstreamRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplorerQuery => formatter.write_str("ExplorerQuery"),
            Self::WalletQuery => formatter.write_str("WalletQuery"),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ZinderUpstreamContractError {
    #[error("{upstream} ServerInfo RPC failed: {source}")]
    Rpc {
        upstream: UpstreamRole,
        #[source]
        source: tonic::Status,
    },

    #[error("{upstream} ServerInfo response omitted required descriptor {field}")]
    MissingDescriptor {
        upstream: UpstreamRole,
        field: &'static str,
    },

    #[error("{upstream} service identity mismatch: expected {expected}, received {actual}")]
    ServiceIdentityMismatch {
        upstream: UpstreamRole,
        expected: RuntimeService,
        actual: String,
    },

    #[error("{upstream} network mismatch: expected {expected}, received {actual}")]
    NetworkMismatch {
        upstream: UpstreamRole,
        expected: &'static str,
        actual: String,
    },

    #[error("{upstream} contract revision is too old: minimum {expected}, received {actual}")]
    ContractRevisionMismatch {
        upstream: UpstreamRole,
        expected: u32,
        actual: u32,
    },

    #[error("{upstream} is missing required capability {capability}")]
    MissingCapability {
        upstream: UpstreamRole,
        capability: &'static str,
    },
}

pub(crate) async fn preflight_upstream_contract_pair(
    network: Network,
    explorer_channel: AuthenticatedChannel,
    wallet_channel: AuthenticatedChannel,
) -> Result<UpstreamAdmission, ZinderUpstreamContractError> {
    let mut explorer_client = ExplorerQueryClient::new(explorer_channel);
    let mut wallet_client = WalletQueryClient::new(wallet_channel);
    let (explorer_response, wallet_response) = tokio::join!(
        explorer_client.server_info(explorer::ServerInfoRequest {}),
        wallet_client.server_info(wallet::ServerInfoRequest {}),
    );
    let explorer_response = explorer_response
        .map_err(|source| ZinderUpstreamContractError::Rpc {
            upstream: UpstreamRole::ExplorerQuery,
            source,
        })?
        .into_inner();
    let wallet_response = wallet_response
        .map_err(|source| ZinderUpstreamContractError::Rpc {
            upstream: UpstreamRole::WalletQuery,
            source,
        })?
        .into_inner();
    validate_upstream_contract_pair(network, &explorer_response, &wallet_response)
}

pub(crate) fn validate_upstream_contract_pair(
    network: Network,
    explorer_response: &explorer::ServerInfoResponse,
    wallet_response: &wallet::ServerInfoResponse,
) -> Result<UpstreamAdmission, ZinderUpstreamContractError> {
    let explorer_common = explorer_response
        .info
        .as_ref()
        .ok_or(ZinderUpstreamContractError::MissingDescriptor {
            upstream: UpstreamRole::ExplorerQuery,
            field: "info",
        })?
        .common
        .as_ref()
        .ok_or(ZinderUpstreamContractError::MissingDescriptor {
            upstream: UpstreamRole::ExplorerQuery,
            field: "info.common",
        })?;
    let wallet_common = wallet_response
        .info
        .as_ref()
        .ok_or(ZinderUpstreamContractError::MissingDescriptor {
            upstream: UpstreamRole::WalletQuery,
            field: "info",
        })?
        .common
        .as_ref()
        .ok_or(ZinderUpstreamContractError::MissingDescriptor {
            upstream: UpstreamRole::WalletQuery,
            field: "info.common",
        })?;
    validate_service_identity(
        UpstreamRole::ExplorerQuery,
        RuntimeService::Explorer,
        explorer_common,
    )?;
    validate_service_identity(
        UpstreamRole::WalletQuery,
        RuntimeService::Query,
        wallet_common,
    )?;
    let expected_network = encode_zinder_native_chain_name(network);
    validate_network(
        UpstreamRole::ExplorerQuery,
        expected_network,
        explorer_common,
    )?;
    validate_network(UpstreamRole::WalletQuery, expected_network, wallet_common)?;
    validate_contract_revision(UpstreamRole::ExplorerQuery, explorer_common)?;
    validate_contract_revision(UpstreamRole::WalletQuery, wallet_common)?;
    validate_capabilities(
        UpstreamRole::ExplorerQuery,
        explorer_common,
        &REQUIRED_EXPLORER_CAPABILITIES,
    )?;
    validate_capabilities(
        UpstreamRole::WalletQuery,
        wallet_common,
        &REQUIRED_WALLET_CAPABILITIES,
    )?;
    Ok(UpstreamAdmission {
        realtime_websocket_enabled: descriptor_has_capability(
            explorer_common,
            EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
        ) && descriptor_has_capability(
            wallet_common,
            WALLET_EVENTS_CHAIN_V1,
        ) && descriptor_has_capability(
            wallet_common,
            WALLET_EVENTS_MEMPOOL_V2,
        ),
    })
}

fn descriptor_has_capability(
    descriptor: &zinder_proto::v1::ops::ServerInfo,
    capability: &str,
) -> bool {
    descriptor
        .capabilities
        .iter()
        .any(|advertised| advertised == capability)
}

fn validate_capabilities(
    upstream: UpstreamRole,
    descriptor: &zinder_proto::v1::ops::ServerInfo,
    required: &[&'static str],
) -> Result<(), ZinderUpstreamContractError> {
    if let Some(capability) = required
        .iter()
        .copied()
        .find(|capability| !descriptor_has_capability(descriptor, capability))
    {
        return Err(ZinderUpstreamContractError::MissingCapability {
            upstream,
            capability,
        });
    }
    Ok(())
}

fn validate_contract_revision(
    upstream: UpstreamRole,
    descriptor: &zinder_proto::v1::ops::ServerInfo,
) -> Result<(), ZinderUpstreamContractError> {
    if descriptor.contract_revision < CONTRACT_REVISION {
        return Err(ZinderUpstreamContractError::ContractRevisionMismatch {
            upstream,
            expected: CONTRACT_REVISION,
            actual: descriptor.contract_revision,
        });
    }
    Ok(())
}

fn validate_network(
    upstream: UpstreamRole,
    expected: &'static str,
    descriptor: &zinder_proto::v1::ops::ServerInfo,
) -> Result<(), ZinderUpstreamContractError> {
    if descriptor.network != expected {
        return Err(ZinderUpstreamContractError::NetworkMismatch {
            upstream,
            expected,
            actual: descriptor.network.clone(),
        });
    }
    Ok(())
}

fn validate_service_identity(
    upstream: UpstreamRole,
    expected: RuntimeService,
    descriptor: &zinder_proto::v1::ops::ServerInfo,
) -> Result<(), ZinderUpstreamContractError> {
    if descriptor.service_name != expected.binary_name() {
        return Err(ZinderUpstreamContractError::ServiceIdentityMismatch {
            upstream,
            expected,
            actual: descriptor.service_name.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zinder_proto::capabilities::EXPLORER_BLOCK_PRODUCTION_SERIES_V2;
    use zinder_proto::v1::ops;

    use super::*;

    #[test]
    fn minimal_transaction_pair_is_admitted_without_optional_realtime() {
        let explorer_response = minimal_explorer_response();
        let wallet_response = minimal_exact_pair_wallet_response();

        let admission = validate_upstream_contract_pair(
            Network::ZcashRegtest,
            &explorer_response,
            &wallet_response,
        );
        assert!(matches!(
            admission,
            Ok(UpstreamAdmission {
                realtime_websocket_enabled: false,
            })
        ));
    }

    #[test]
    fn realtime_admission_requires_block_production_and_wallet_events() {
        let explorer_response = realtime_explorer_response();
        let complete_wallet = minimal_exact_pair_wallet_response();

        for missing in [WALLET_EVENTS_CHAIN_V1, WALLET_EVENTS_MEMPOOL_V2] {
            let mut wallet_response = complete_wallet.clone();
            remove_capability(
                wallet_response
                    .info
                    .as_mut()
                    .and_then(|info| info.common.as_mut()),
                missing,
            );
            assert!(
                matches!(
                    validate_upstream_contract_pair(
                        Network::ZcashRegtest,
                        &explorer_response,
                        &wallet_response,
                    ),
                    Ok(UpstreamAdmission {
                        realtime_websocket_enabled: false,
                    })
                ),
                "missing {missing}",
            );
        }

        let minimal_explorer = minimal_explorer_response();
        assert!(matches!(
            validate_upstream_contract_pair(
                Network::ZcashRegtest,
                &minimal_explorer,
                &minimal_exact_pair_wallet_response(),
            ),
            Ok(UpstreamAdmission {
                realtime_websocket_enabled: false,
            })
        ));
    }

    #[test]
    fn missing_nested_descriptor_is_rejected() {
        let explorer_response = explorer_response(
            RuntimeService::Explorer,
            REQUIRED_EXPLORER_CAPABILITIES.as_slice(),
        );
        let mut wallet_response = wallet_response(
            RuntimeService::Query,
            REQUIRED_WALLET_CAPABILITIES.as_slice(),
        );
        if let Some(info) = wallet_response.info.as_mut() {
            info.common = None;
        }

        assert!(matches!(
            validate_upstream_contract_pair(
                Network::ZcashRegtest,
                &explorer_response,
                &wallet_response,
            ),
            Err(ZinderUpstreamContractError::MissingDescriptor {
                upstream: UpstreamRole::WalletQuery,
                field: "info.common",
            })
        ));
    }

    #[test]
    fn swapped_service_identities_are_rejected() {
        let explorer_response = explorer_response(
            RuntimeService::Query,
            REQUIRED_EXPLORER_CAPABILITIES.as_slice(),
        );
        let wallet_response = wallet_response(
            RuntimeService::Explorer,
            REQUIRED_WALLET_CAPABILITIES.as_slice(),
        );

        assert!(matches!(
            validate_upstream_contract_pair(
                Network::ZcashRegtest,
                &explorer_response,
                &wallet_response,
            ),
            Err(ZinderUpstreamContractError::ServiceIdentityMismatch {
                upstream: UpstreamRole::ExplorerQuery,
                expected: RuntimeService::Explorer,
                actual,
            }) if actual == RuntimeService::Query.binary_name()
        ));
    }

    #[test]
    fn wrong_network_is_rejected() {
        let explorer_response = explorer_response(
            RuntimeService::Explorer,
            REQUIRED_EXPLORER_CAPABILITIES.as_slice(),
        );
        let mut wallet_response = wallet_response(
            RuntimeService::Query,
            REQUIRED_WALLET_CAPABILITIES.as_slice(),
        );
        if let Some(common) = wallet_response
            .info
            .as_mut()
            .and_then(|info| info.common.as_mut())
        {
            common.network = "zcash-mainnet".to_owned();
        }

        assert!(matches!(
            validate_upstream_contract_pair(
                Network::ZcashRegtest,
                &explorer_response,
                &wallet_response,
            ),
            Err(ZinderUpstreamContractError::NetworkMismatch {
                upstream: UpstreamRole::WalletQuery,
                expected: "zcash-regtest",
                actual,
            }) if actual == "zcash-mainnet"
        ));
    }

    #[test]
    fn wrong_contract_revision_is_rejected() {
        let mut explorer_response = explorer_response(
            RuntimeService::Explorer,
            REQUIRED_EXPLORER_CAPABILITIES.as_slice(),
        );
        let wallet_response = wallet_response(
            RuntimeService::Query,
            REQUIRED_WALLET_CAPABILITIES.as_slice(),
        );
        if let Some(common) = explorer_response
            .info
            .as_mut()
            .and_then(|info| info.common.as_mut())
        {
            common.contract_revision = CONTRACT_REVISION.saturating_sub(1);
        }

        assert!(matches!(
            validate_upstream_contract_pair(
                Network::ZcashRegtest,
                &explorer_response,
                &wallet_response,
            ),
            Err(ZinderUpstreamContractError::ContractRevisionMismatch {
                upstream: UpstreamRole::ExplorerQuery,
                expected: CONTRACT_REVISION,
                actual,
            }) if actual == CONTRACT_REVISION.saturating_sub(1)
        ));
    }

    #[test]
    fn newer_contract_revision_is_admitted_with_required_capabilities() {
        let mut explorer_response = explorer_response(
            RuntimeService::Explorer,
            REQUIRED_EXPLORER_CAPABILITIES.as_slice(),
        );
        let mut wallet_response = wallet_response(
            RuntimeService::Query,
            REQUIRED_WALLET_CAPABILITIES.as_slice(),
        );
        for descriptor in [
            explorer_response
                .info
                .as_mut()
                .and_then(|info| info.common.as_mut()),
            wallet_response
                .info
                .as_mut()
                .and_then(|info| info.common.as_mut()),
        ]
        .into_iter()
        .flatten()
        {
            descriptor.contract_revision = CONTRACT_REVISION.saturating_add(1);
        }

        assert!(
            validate_upstream_contract_pair(
                Network::ZcashRegtest,
                &explorer_response,
                &wallet_response,
            )
            .is_ok()
        );
    }

    #[test]
    fn every_required_explorer_capability_is_enforced() {
        let complete_explorer = minimal_explorer_response();
        let wallet_response = minimal_exact_pair_wallet_response();

        for missing in REQUIRED_EXPLORER_CAPABILITIES {
            let mut explorer_response = complete_explorer.clone();
            remove_capability(
                explorer_response
                    .info
                    .as_mut()
                    .and_then(|info| info.common.as_mut()),
                missing,
            );
            assert!(matches!(
                validate_upstream_contract_pair(
                    Network::ZcashRegtest,
                    &explorer_response,
                    &wallet_response,
                ),
                Err(ZinderUpstreamContractError::MissingCapability {
                    upstream: UpstreamRole::ExplorerQuery,
                    capability,
                }) if capability == missing
            ));
        }
    }

    #[test]
    fn every_required_wallet_capability_is_enforced() {
        let explorer_response = minimal_explorer_response();
        let complete_wallet = minimal_exact_pair_wallet_response();

        for missing in REQUIRED_WALLET_CAPABILITIES {
            let mut wallet_response = complete_wallet.clone();
            remove_capability(
                wallet_response
                    .info
                    .as_mut()
                    .and_then(|info| info.common.as_mut()),
                missing,
            );
            assert!(matches!(
                validate_upstream_contract_pair(
                    Network::ZcashRegtest,
                    &explorer_response,
                    &wallet_response,
                ),
                Err(ZinderUpstreamContractError::MissingCapability {
                    upstream: UpstreamRole::WalletQuery,
                    capability,
                }) if capability == missing
            ));
        }
    }

    fn remove_capability(descriptor: Option<&mut ops::ServerInfo>, capability: &'static str) {
        if let Some(descriptor) = descriptor {
            descriptor
                .capabilities
                .retain(|advertised| advertised != capability);
        }
    }

    fn explorer_response(
        service: RuntimeService,
        capabilities: &[&str],
    ) -> explorer::ServerInfoResponse {
        explorer::ServerInfoResponse {
            info: Some(explorer::ExplorerServerInfo {
                common: Some(common_descriptor(service, capabilities)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn minimal_explorer_response() -> explorer::ServerInfoResponse {
        explorer_response(
            RuntimeService::Explorer,
            &[EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V4],
        )
    }

    fn realtime_explorer_response() -> explorer::ServerInfoResponse {
        explorer_response(
            RuntimeService::Explorer,
            &[
                EXPLORER_SERVER_INFO_V1,
                EXPLORER_TRANSACTION_DETAIL_V4,
                EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
            ],
        )
    }

    fn wallet_response(
        service: RuntimeService,
        capabilities: &[&str],
    ) -> wallet::ServerInfoResponse {
        wallet::ServerInfoResponse {
            info: Some(wallet::WalletServerInfo {
                common: Some(common_descriptor(service, capabilities)),
                ..Default::default()
            }),
        }
    }

    fn exact_pair_wallet_response() -> wallet::ServerInfoResponse {
        let info = zinder_query::build_wallet_server_info(&zinder_query::ServerInfoSettings {
            network: "zcash-regtest".to_owned(),
            transaction_blobs_retained: true,
            transaction_broadcast_enabled: true,
            chain_events_enabled: true,
            transparent_outpoint_spend_available: true,
            capability_profile: zinder_query::WalletCapabilityProfile::ExactPair,
            ..zinder_query::ServerInfoSettings::default()
        });
        wallet::ServerInfoResponse { info: Some(info) }
    }

    fn minimal_exact_pair_wallet_response() -> wallet::ServerInfoResponse {
        let mut response = exact_pair_wallet_response();
        if let Some(common) = response.info.as_mut().and_then(|info| info.common.as_mut()) {
            common.capabilities.retain(|capability| {
                [
                    WALLET_READ_SERVER_INFO_V2,
                    WALLET_READ_TRANSACTION_BY_ID_V2,
                    WALLET_READ_TRANSACTION_BYTES_V1,
                    WALLET_EVENTS_CHAIN_V1,
                    WALLET_EVENTS_MEMPOOL_V2,
                ]
                .contains(&capability.as_str())
            });
        }
        response
    }

    fn common_descriptor(service: RuntimeService, capabilities: &[&str]) -> ops::ServerInfo {
        ops::ServerInfo {
            network: "zcash-regtest".to_owned(),
            service_name: service.binary_name().to_owned(),
            service_version: "test".to_owned(),
            capabilities: capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            contract_revision: CONTRACT_REVISION,
            ..Default::default()
        }
    }
}
