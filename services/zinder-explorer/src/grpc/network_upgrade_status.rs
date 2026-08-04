//! `ExplorerQuery.NetworkUpgradeStatus` handler.
//!
//! Projects the admitted Wallet [`NetworkUpgradeActivations`] table onto the
//! wire, anchoring the `active` flags to the canonical tip height read from
//! `WalletQuery.VisibleTipBlock`.

use tonic::{Request, Response, Status};
use zinder_core::wire::encode_branch_id_hex;
use zinder_core::{BlockHeight, NetworkUpgradeActivations};
use zinder_proto::capabilities::EXPLORER_NETWORK_UPGRADE_STATUS_V1;
use zinder_proto::v1::explorer::{
    ExplorerFreshness, NetworkUpgradeEntry, NetworkUpgradeStatusRequest,
    NetworkUpgradeStatusResponse,
};
use zinder_proto::v1::wallet::{
    self, VisibleTipBlockRequest, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{UpstreamObservationCache, attach_upstream_observation};

/// Executes one `ExplorerQuery.NetworkUpgradeStatus` request.
pub(crate) async fn query_network_upgrade_status(
    activations: &NetworkUpgradeActivations,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    _request: Request<NetworkUpgradeStatusRequest>,
) -> Result<Response<NetworkUpgradeStatusResponse>, Status> {
    let response = wallet_client
        .visible_tip_block(Request::new(VisibleTipBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner();
    let (mut status, freshness) =
        build_network_upgrade_status_from_visible_tip(activations, &response)?;
    status.freshness =
        Some(attach_upstream_observation(upstream_observation_cache, freshness).await);
    Ok(Response::new(status))
}

/// Builds the status fields and local freshness from one coherent Wallet tip response.
///
/// The handler attaches the independently observed upstream axis only after this
/// immutable Wallet response has been fully interpreted.
fn build_network_upgrade_status_from_visible_tip(
    activations: &NetworkUpgradeActivations,
    response: &wallet::VisibleTipBlockResponse,
) -> Result<(NetworkUpgradeStatusResponse, ExplorerFreshness), Status> {
    let chain_epoch = require_network_upgrade_visible_tip_coherence(response)?;
    let tip_height = chain_epoch
        .visible_tip
        .as_ref()
        .ok_or_else(|| {
            ExplorerError::internal("VisibleTipBlockResponse chain epoch visible_tip missing")
        })?
        .height;

    let upgrades = activations
        .activations()
        .iter()
        .map(|activation| {
            let activation_height = activation.activation_height.value();
            NetworkUpgradeEntry {
                name: activation.name.clone(),
                branch_id_hex: encode_branch_id_hex(activation.branch_id),
                activation_height,
                active: activation_height <= tip_height,
            }
        })
        .collect();

    let (active_upgrade_name, active_upgrade_branch_id_hex) = activations
        .active_at(BlockHeight::new(tip_height))
        .map_or_else(
            || (String::new(), String::new()),
            |activation| {
                (
                    activation.name.clone(),
                    encode_branch_id_hex(activation.branch_id),
                )
            },
        );

    let freshness = network_upgrade_freshness(chain_epoch);
    Ok((
        NetworkUpgradeStatusResponse {
            freshness: None,
            tip_height,
            upgrades,
            active_upgrade_name,
            active_upgrade_branch_id_hex,
        },
        freshness,
    ))
}

/// Builds freshness from the validated Wallet epoch without inventing local
/// materialized-view axes for this Wallet-sourced contract.
fn network_upgrade_freshness(chain_epoch: wallet::ChainEpoch) -> ExplorerFreshness {
    ExplorerFreshness {
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(chain_epoch),
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: None,
        }),
        snapshot_age_millis: 0,
        capability_version: EXPLORER_NETWORK_UPGRADE_STATUS_V1.to_owned(),
        unavailable: Vec::new(),
    }
}

fn require_network_upgrade_visible_tip_coherence(
    response: &wallet::VisibleTipBlockResponse,
) -> Result<wallet::ChainEpoch, Status> {
    let chain_epoch = response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| {
            ExplorerError::internal("VisibleTipBlockResponse.chain_view.chain_epoch missing")
        })?;
    let epoch_tip = chain_epoch.visible_tip.as_ref().ok_or_else(|| {
        ExplorerError::internal("VisibleTipBlockResponse chain epoch visible_tip missing")
    })?;
    let response_tip = response.visible_tip_block.as_ref().ok_or_else(|| {
        ExplorerError::internal("VisibleTipBlockResponse.visible_tip_block missing")
    })?;
    let height_mismatch = response_tip.height != epoch_tip.height;
    let hash_mismatch = response_tip.block_hash != epoch_tip.hash;
    if height_mismatch || hash_mismatch {
        return Err(ExplorerError::unsatisfied_precondition(
            "Wallet VisibleTipBlock response tip does not match its chain epoch tip",
        )
        .into());
    }
    Ok(chain_epoch.clone())
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;

    fn response_at(height: u32) -> wallet::VisibleTipBlockResponse {
        let hash = format!("tip-{height}");
        wallet::VisibleTipBlockResponse {
            chain_view: Some(wallet::ChainView {
                chain_epoch: Some(wallet::ChainEpoch {
                    chain_epoch_id: 7,
                    visible_tip: Some(wallet::BlockTip {
                        height,
                        hash: hash.clone(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            visible_tip_block: Some(wallet::BlockId {
                height,
                block_hash: hash,
            }),
        }
    }

    fn response() -> wallet::VisibleTipBlockResponse {
        response_at(100)
    }

    #[test]
    fn network_upgrade_status_rejects_a_response_tip_with_a_different_epoch_hash()
    -> Result<(), &'static str> {
        let mut response = response();
        response.visible_tip_block = Some(wallet::BlockId {
            height: 100,
            block_hash: "ccdd".to_owned(),
        });

        let error = require_network_upgrade_visible_tip_coherence(&response)
            .err()
            .ok_or("mismatched response tip must fail")?;
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        Ok(())
    }

    #[test]
    fn network_upgrade_status_freshness_carries_the_validated_wallet_epoch_only()
    -> Result<(), &'static str> {
        let epoch = require_network_upgrade_visible_tip_coherence(&response())
            .map_err(|_| "coherent Wallet response must produce an epoch")?;
        let freshness = network_upgrade_freshness(epoch);
        let chain_view = freshness
            .chain_view
            .as_ref()
            .ok_or("Wallet epoch must be carried in freshness")?;
        assert_eq!(
            chain_view
                .chain_epoch
                .as_ref()
                .map(|epoch| epoch.chain_epoch_id),
            Some(7)
        );
        assert!(chain_view.indexed_tip.is_none());
        assert!(chain_view.materialized_views.is_none());
        Ok(())
    }

    #[test]
    fn network_upgrade_status_uses_only_the_captured_e1_wallet_response() -> Result<(), &'static str>
    {
        let activations = zinder_testkit::sample_regtest_upgrade_activations();
        let e1_response = response_at(602);
        let (e1_status, e1_freshness) =
            build_network_upgrade_status_from_visible_tip(&activations, &e1_response)
                .map_err(|_| "captured E1 Wallet response must be coherent")?;

        // A later Wallet response crosses the NU6.3 activation boundary. It
        // must not alter the already assembled E1 status or freshness.
        let e2_response = response_at(603);
        let (e2_status, _) =
            build_network_upgrade_status_from_visible_tip(&activations, &e2_response)
                .map_err(|_| "later E2 Wallet response must be coherent")?;

        assert_eq!(e1_status.tip_height, 602);
        assert_eq!(e1_status.active_upgrade_name, "NU6.2");
        assert_eq!(e2_status.active_upgrade_name, "NU6.3");
        assert!(
            e1_status
                .upgrades
                .iter()
                .any(|upgrade| upgrade.name == "NU6.3" && !upgrade.active)
        );
        assert_eq!(
            e1_freshness
                .chain_view
                .and_then(|chain_view| chain_view.chain_epoch)
                .map(|chain_epoch| chain_epoch.visible_tip.map(|tip| tip.height)),
            Some(Some(602))
        );
        Ok(())
    }
}
