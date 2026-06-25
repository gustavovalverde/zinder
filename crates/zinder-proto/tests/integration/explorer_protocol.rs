#![allow(
    missing_docs,
    reason = "Integration test names describe the native protocol contract under test."
)]

use eyre::eyre;
use prost::Message;
use zinder_proto::capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1;
use zinder_proto::v1::{explorer, wallet};

#[test]
fn upstream_tip_round_trips_through_prost() -> eyre::Result<()> {
    let upstream_tip = wallet::UpstreamTip {
        committed_height: Some(2_530_000),
        estimated_height: Some(2_544_375),
    };
    let decoded = round_trip(&upstream_tip)?;

    assert_eq!(decoded.committed_height, Some(2_530_000));
    assert_eq!(decoded.estimated_height, Some(2_544_375));
    Ok(())
}

#[test]
fn upstream_tip_optional_fields_default_to_none() -> eyre::Result<()> {
    let upstream_tip = wallet::UpstreamTip::default();
    let decoded = round_trip(&upstream_tip)?;

    assert!(decoded.committed_height.is_none());
    assert!(decoded.estimated_height.is_none());
    Ok(())
}

#[test]
fn explorer_freshness_carries_chain_view_with_every_axis() -> eyre::Result<()> {
    let freshness = explorer::ExplorerFreshness {
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(wallet::ChainEpoch::default()),
            indexed_tip: Some(wallet::IndexedTip {
                tip: Some(wallet::BlockTip {
                    height: 2_529_999,
                    hash: "11".repeat(32),
                }),
                block_time_unix_seconds: 1_774_670_000,
            }),
            upstream_tip: Some(wallet::UpstreamTip {
                committed_height: Some(2_530_000),
                estimated_height: Some(2_544_375),
            }),
            derive: Some(wallet::DeriveStatus {
                health: wallet::DeriveHealth::CatchingUp as i32,
                indexed_height: 2_529_999,
                lag_blocks: 1,
                observed_at_millis: 1_774_670_400_000,
            }),
        }),
        snapshot_age_millis: 0,
        capability_version: EXPLORER_OVERVIEW_SNAPSHOT_V1.to_owned(),
        unavailable: Vec::new(),
    };
    let decoded = round_trip(&freshness)?;

    let chain_view = decoded
        .chain_view
        .ok_or_else(|| eyre!("chain_view not set"))?;
    let upstream = chain_view
        .upstream_tip
        .ok_or_else(|| eyre!("upstream_tip not set"))?;
    assert_eq!(upstream.committed_height, Some(2_530_000));
    assert_eq!(upstream.estimated_height, Some(2_544_375));
    let indexed_tip = chain_view
        .indexed_tip
        .ok_or_else(|| eyre!("indexed_tip not set"))?;
    assert_eq!(
        indexed_tip
            .tip
            .ok_or_else(|| eyre!("indexed tip missing"))?
            .height,
        2_529_999
    );
    Ok(())
}

/// An absent `indexed_tip` means "derive head unknown", never "at tip"; the
/// proto3 optional message survives the round trip as `None`.
#[test]
fn explorer_freshness_absent_indexed_tip_means_unknown() -> eyre::Result<()> {
    let freshness = explorer::ExplorerFreshness {
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(wallet::ChainEpoch::default()),
            indexed_tip: None,
            upstream_tip: None,
            derive: None,
        }),
        snapshot_age_millis: 0,
        capability_version: EXPLORER_OVERVIEW_SNAPSHOT_V1.to_owned(),
        unavailable: Vec::new(),
    };
    let decoded = round_trip(&freshness)?;

    let chain_view = decoded
        .chain_view
        .ok_or_else(|| eyre!("chain_view not set"))?;
    assert!(chain_view.indexed_tip.is_none());
    assert!(chain_view.upstream_tip.is_none());
    Ok(())
}

#[test]
fn transaction_detail_response_embeds_shared_wallet_location() -> eyre::Result<()> {
    let response = explorer::TransactionDetailResponse {
        freshness: None,
        facts: None,
        location: Some(wallet::TransactionLocation {
            location: Some(wallet::transaction_location::Location::Mined(
                wallet::MinedTransaction {
                    location: Some(wallet::MinedBlockLocation {
                        transaction_id: "ab".repeat(32),
                        block_height: 99,
                        block_hash: "cd".repeat(32),
                        tx_index_in_block: 1,
                    }),
                    details: Some(wallet::MinedDetails {
                        consensus_branch_id: 0xc2d6_d0b4,
                        block_time: 1_774_670_000,
                        confirmations: 12,
                    }),
                    raw_transaction_bytes: Vec::new(),
                },
            )),
        }),
        paid_fee_zat: None,
        prevout_resolution_status: 0,
        transparent_inputs: Vec::new(),
    };
    let decoded = round_trip(&response)?;

    let location = decoded
        .location
        .and_then(|location| location.location)
        .ok_or_else(|| eyre!("location oneof missing"))?;
    let wallet::transaction_location::Location::Mined(mined) = location else {
        return Err(eyre!("expected mined arm"));
    };
    assert_eq!(
        mined
            .location
            .ok_or_else(|| eyre!("mined block location missing"))?
            .block_height,
        99
    );

    Ok(())
}

#[test]
fn transaction_detail_response_carries_conflicting_location() -> eyre::Result<()> {
    let response = explorer::TransactionDetailResponse {
        freshness: None,
        facts: None,
        location: Some(wallet::TransactionLocation {
            location: Some(wallet::transaction_location::Location::Conflicting(
                wallet::ConflictingChainTransaction {},
            )),
        }),
        paid_fee_zat: None,
        prevout_resolution_status: 0,
        transparent_inputs: Vec::new(),
    };
    let decoded = round_trip(&response)?;

    let location = decoded
        .location
        .and_then(|location| location.location)
        .ok_or_else(|| eyre!("location oneof missing"))?;
    assert!(matches!(
        location,
        wallet::transaction_location::Location::Conflicting(_)
    ));

    Ok(())
}

fn round_trip<MessageType>(message: &MessageType) -> Result<MessageType, prost::DecodeError>
where
    MessageType: Message + Default,
{
    let encoded = message.encode_to_vec();
    MessageType::decode(encoded.as_slice())
}
