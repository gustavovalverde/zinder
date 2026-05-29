#![allow(
    missing_docs,
    reason = "Integration test names describe the native protocol contract under test."
)]

use eyre::eyre;
use prost::Message;
use zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1;
use zinder_proto::v1::explorer;

#[test]
fn upstream_observation_round_trips_through_prost() -> eyre::Result<()> {
    let observation = explorer::UpstreamObservation {
        upstream_committed_tip_height: Some(2_530_000),
        upstream_estimated_tip_height: Some(2_544_375),
        upstream_verification_progress: Some(0.9943),
    };
    let decoded = round_trip(&observation)?;

    assert_eq!(decoded.upstream_committed_tip_height, Some(2_530_000));
    assert_eq!(decoded.upstream_estimated_tip_height, Some(2_544_375));
    assert_eq!(decoded.upstream_verification_progress, Some(0.9943));
    Ok(())
}

#[test]
fn upstream_observation_optional_fields_default_to_none() -> eyre::Result<()> {
    let observation = explorer::UpstreamObservation::default();
    let decoded = round_trip(&observation)?;

    assert!(decoded.upstream_committed_tip_height.is_none());
    assert!(decoded.upstream_estimated_tip_height.is_none());
    assert!(decoded.upstream_verification_progress.is_none());
    Ok(())
}

#[test]
fn explorer_freshness_carries_optional_upstream_observation() -> eyre::Result<()> {
    let freshness = explorer::ExplorerFreshness {
        chain_epoch: None,
        snapshot_age_millis: 0,
        capability_version: EXPLORER_OVERVIEW_SNAPSHOT_V1.to_owned(),
        unavailable: Vec::new(),
        indexed_head: None,
        upstream: Some(explorer::UpstreamObservation {
            upstream_committed_tip_height: Some(2_530_000),
            upstream_estimated_tip_height: Some(2_544_375),
            upstream_verification_progress: Some(0.9943),
        }),
    };
    let decoded = round_trip(&freshness)?;

    let upstream = decoded.upstream.ok_or_else(|| eyre!("upstream not set"))?;
    assert_eq!(upstream.upstream_committed_tip_height, Some(2_530_000));
    assert_eq!(upstream.upstream_estimated_tip_height, Some(2_544_375));
    assert_eq!(upstream.upstream_verification_progress, Some(0.9943));
    Ok(())
}

#[test]
fn explorer_freshness_omits_upstream_when_probe_has_not_fired() -> eyre::Result<()> {
    let freshness = explorer::ExplorerFreshness {
        chain_epoch: None,
        snapshot_age_millis: 0,
        capability_version: EXPLORER_OVERVIEW_SNAPSHOT_V1.to_owned(),
        unavailable: Vec::new(),
        indexed_head: None,
        upstream: None,
    };
    let decoded = round_trip(&freshness)?;

    assert!(decoded.upstream.is_none());
    Ok(())
}

fn round_trip<MessageType>(message: &MessageType) -> Result<MessageType, prost::DecodeError>
where
    MessageType: Message + Default,
{
    let encoded = message.encode_to_vec();
    MessageType::decode(encoded.as_slice())
}
