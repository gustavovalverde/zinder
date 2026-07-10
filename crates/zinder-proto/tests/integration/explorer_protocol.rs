#![allow(
    missing_docs,
    reason = "Integration test names describe the native protocol contract under test."
)]

use eyre::eyre;
use prost::Message;
use zinder_proto::capabilities::{
    EXPLORER_MIGRATION_COHORTS_V1, EXPLORER_MIGRATION_DENOMINATIONS_V1,
    EXPLORER_MIGRATION_OVERVIEW_V1, EXPLORER_NETWORK_UPGRADE_STATUS_V1,
    EXPLORER_OVERVIEW_SNAPSHOT_V1, EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
    EXPLORER_UTXO_SET_SUMMARY_V1,
};
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
                    raw_transaction_bytes: None,
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

#[test]
fn ironwood_component_counts_round_trip_through_explorer_messages() -> eyre::Result<()> {
    let counts = explorer::TransactionComponentCounts {
        transparent_input_count: 1,
        transparent_output_count: 2,
        sapling_spend_count: 3,
        sapling_output_count: 4,
        orchard_action_count: 5,
        sprout_joinsplit_count: 6,
        ironwood_action_count: 7,
    };
    let decoded_counts = round_trip(&counts)?;
    assert_eq!(decoded_counts.ironwood_action_count, 7);

    let summary = explorer::BlockSummary {
        ironwood_action_count: 11,
        ..Default::default()
    };
    let decoded_summary = round_trip(&summary)?;
    assert_eq!(decoded_summary.ironwood_action_count, 11);
    Ok(())
}

#[test]
fn transparent_address_deltas_entry_round_trips_signed_values() -> eyre::Result<()> {
    let received = explorer::TransparentAddressDeltasEntry {
        transaction_id: "a".repeat(64),
        block_height: 2_530_000,
        block_time_unix_seconds: 1_700_000_000,
        index: 1,
        value_zat: 5_000_000,
        kind: explorer::TransparentDeltaKind::Received as i32,
    };
    let spent = explorer::TransparentAddressDeltasEntry {
        transaction_id: "b".repeat(64),
        block_height: 2_530_005,
        block_time_unix_seconds: 1_700_000_900,
        index: 0,
        value_zat: -3_000_000,
        kind: explorer::TransparentDeltaKind::Spent as i32,
    };

    let decoded_received = round_trip(&received)?;
    let decoded_spent = round_trip(&spent)?;

    assert_eq!(decoded_received.value_zat, 5_000_000);
    assert_eq!(
        decoded_received.kind,
        explorer::TransparentDeltaKind::Received as i32
    );
    assert_eq!(decoded_spent.value_zat, -3_000_000);
    assert_eq!(
        decoded_spent.kind,
        explorer::TransparentDeltaKind::Spent as i32
    );
    Ok(())
}

#[test]
fn transparent_address_deltas_response_carries_freshness_and_cursor() -> eyre::Result<()> {
    let response = explorer::TransparentAddressDeltasResponse {
        freshness: Some(explorer::ExplorerFreshness {
            capability_version: EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1.to_owned(),
            ..Default::default()
        }),
        entries: vec![explorer::TransparentAddressDeltasEntry::default()],
        next_cursor: vec![1, 2, 3, 4],
    };
    let decoded = round_trip(&response)?;

    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(decoded.next_cursor, vec![1, 2, 3, 4]);
    assert_eq!(
        decoded
            .freshness
            .ok_or_else(|| eyre!("freshness envelope missing"))?
            .capability_version,
        EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1
    );
    Ok(())
}

#[test]
fn utxo_set_summary_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = explorer::UtxoSetSummaryResponse {
        freshness: Some(explorer::ExplorerFreshness {
            chain_view: Some(wallet::ChainView {
                chain_epoch: Some(wallet::ChainEpoch::default()),
                indexed_tip: None,
                upstream_tip: None,
                derive: None,
            }),
            snapshot_age_millis: 0,
            capability_version: EXPLORER_UTXO_SET_SUMMARY_V1.to_owned(),
            unavailable: Vec::new(),
        }),
        utxo_count: 4096,
        total_value_zat: 2_100_000_000_000_000,
        summarized_height: 2_500_000,
        commitment: Some(wallet::TransparentUtxoSetCommitment {
            scheme: wallet::UtxoSetCommitmentScheme::Lthash16 as i32,
            commitment: vec![0xcd; 2048],
        }),
    };
    let decoded = round_trip(&response)?;

    assert_eq!(decoded.utxo_count, 4096);
    assert_eq!(decoded.total_value_zat, 2_100_000_000_000_000);
    assert_eq!(decoded.summarized_height, 2_500_000);
    assert_eq!(
        decoded
            .commitment
            .ok_or_else(|| eyre!("commitment present after round-trip"))?
            .commitment
            .len(),
        2048
    );
    assert_eq!(
        decoded
            .freshness
            .ok_or_else(|| eyre!("freshness envelope missing"))?
            .capability_version,
        EXPLORER_UTXO_SET_SUMMARY_V1
    );
    Ok(())
}

#[test]
fn utxo_set_summary_request_round_trips_the_epoch_pin() -> eyre::Result<()> {
    let request = explorer::UtxoSetSummaryRequest {
        at_epoch_id: Some(77),
    };
    let decoded = round_trip(&request)?;

    assert_eq!(decoded.at_epoch_id, Some(77));
    Ok(())
}

#[test]
fn migration_overview_request_round_trips_absent_bounds() -> eyre::Result<()> {
    let full_history = explorer::MigrationOverviewRequest::default();
    let decoded_full = round_trip(&full_history)?;
    assert!(decoded_full.start_height.is_none());
    assert!(decoded_full.end_height.is_none());

    let bounded = explorer::MigrationOverviewRequest {
        start_height: Some(2_500_000),
        end_height: Some(2_530_000),
    };
    let decoded_bounded = round_trip(&bounded)?;
    assert_eq!(decoded_bounded.start_height, Some(2_500_000));
    assert_eq!(decoded_bounded.end_height, Some(2_530_000));
    Ok(())
}

#[test]
fn migration_overview_response_round_trips_the_two_sided_audit() -> eyre::Result<()> {
    let response = explorer::MigrationOverviewResponse {
        freshness: Some(explorer::ExplorerFreshness {
            capability_version: EXPLORER_MIGRATION_OVERVIEW_V1.to_owned(),
            ..Default::default()
        }),
        total_migrated_ironwood_zat: 4_200_000_000,
        migration_count: 17,
        first_height: Some(2_500_010),
        last_height: Some(2_529_990),
        orchard_outflow_zat: 4_200_010_000,
        ironwood_inflow_zat: 4_200_000_000,
    };
    let decoded = round_trip(&response)?;

    assert_eq!(decoded.total_migrated_ironwood_zat, 4_200_000_000);
    assert_eq!(decoded.migration_count, 17);
    assert_eq!(decoded.first_height, Some(2_500_010));
    assert_eq!(decoded.last_height, Some(2_529_990));
    assert_eq!(decoded.orchard_outflow_zat, 4_200_010_000);
    assert_eq!(decoded.ironwood_inflow_zat, 4_200_000_000);
    assert_eq!(
        decoded
            .freshness
            .ok_or_else(|| eyre!("freshness envelope missing"))?
            .capability_version,
        EXPLORER_MIGRATION_OVERVIEW_V1
    );
    Ok(())
}

#[test]
fn migration_cohorts_response_round_trips_anchor_and_summary() -> eyre::Result<()> {
    let anchor = vec![0xab; 32];
    let response = explorer::MigrationCohortsResponse {
        freshness: Some(explorer::ExplorerFreshness {
            capability_version: EXPLORER_MIGRATION_COHORTS_V1.to_owned(),
            ..Default::default()
        }),
        cohorts: vec![explorer::MigrationCohort {
            orchard_anchor: anchor.clone(),
            member_count: 9,
            total_migrated_zat: 1_800_000_000,
            conformant_member_count: 7,
        }],
        cohort_count: 1,
        avg_member_count: 9,
        min_member_count: 9,
        max_member_count: 9,
    };
    let decoded = round_trip(&response)?;

    let cohort = decoded
        .cohorts
        .first()
        .ok_or_else(|| eyre!("cohort missing after round-trip"))?;
    assert_eq!(cohort.orchard_anchor, anchor);
    assert_eq!(cohort.member_count, 9);
    assert_eq!(cohort.conformant_member_count, 7);
    assert_eq!(cohort.total_migrated_zat, 1_800_000_000);
    assert_eq!(decoded.cohort_count, 1);
    assert_eq!(decoded.max_member_count, 9);
    assert_eq!(
        decoded
            .freshness
            .ok_or_else(|| eyre!("freshness envelope missing"))?
            .capability_version,
        EXPLORER_MIGRATION_COHORTS_V1
    );
    Ok(())
}

#[test]
fn migration_denominations_response_round_trips_bins() -> eyre::Result<()> {
    let response = explorer::MigrationDenominationsResponse {
        freshness: Some(explorer::ExplorerFreshness {
            capability_version: EXPLORER_MIGRATION_DENOMINATIONS_V1.to_owned(),
            ..Default::default()
        }),
        bins: vec![
            explorer::MigrationDenominationBin {
                denomination_zat: 100_000_000,
                count: 5,
            },
            explorer::MigrationDenominationBin {
                denomination_zat: 1_000_000_000,
                count: 2,
            },
        ],
        total_tx: 7,
    };
    let decoded = round_trip(&response)?;

    assert_eq!(decoded.bins.len(), 2);
    assert_eq!(decoded.total_tx, 7);
    assert_eq!(
        decoded
            .bins
            .first()
            .ok_or_else(|| eyre!("first bin missing"))?
            .denomination_zat,
        100_000_000
    );
    assert_eq!(
        decoded
            .freshness
            .ok_or_else(|| eyre!("freshness envelope missing"))?
            .capability_version,
        EXPLORER_MIGRATION_DENOMINATIONS_V1
    );
    Ok(())
}

#[test]
fn network_upgrade_status_response_round_trips_entries_and_active_pointer() -> eyre::Result<()> {
    let response = explorer::NetworkUpgradeStatusResponse {
        freshness: Some(explorer::ExplorerFreshness {
            capability_version: EXPLORER_NETWORK_UPGRADE_STATUS_V1.to_owned(),
            ..Default::default()
        }),
        tip_height: 2_530_000,
        upgrades: vec![
            explorer::NetworkUpgradeEntry {
                name: "NU5".to_owned(),
                branch_id_hex: "c2d6d0b4".to_owned(),
                activation_height: 1_842_420,
                active: true,
            },
            explorer::NetworkUpgradeEntry {
                name: "NU6.3".to_owned(),
                branch_id_hex: "37a5165b".to_owned(),
                activation_height: 3_000_000,
                active: false,
            },
        ],
        active_upgrade_name: "NU5".to_owned(),
        active_upgrade_branch_id_hex: "c2d6d0b4".to_owned(),
    };
    let decoded = round_trip(&response)?;

    assert_eq!(decoded.tip_height, 2_530_000);
    assert_eq!(decoded.upgrades.len(), 2);
    let pending = decoded
        .upgrades
        .get(1)
        .ok_or_else(|| eyre!("pending upgrade entry missing"))?;
    assert_eq!(pending.name, "NU6.3");
    assert_eq!(pending.branch_id_hex, "37a5165b");
    assert_eq!(pending.activation_height, 3_000_000);
    assert!(!pending.active);
    assert_eq!(decoded.active_upgrade_name, "NU5");
    assert_eq!(decoded.active_upgrade_branch_id_hex, "c2d6d0b4");
    assert_eq!(
        decoded
            .freshness
            .ok_or_else(|| eyre!("freshness envelope missing"))?
            .capability_version,
        EXPLORER_NETWORK_UPGRADE_STATUS_V1
    );
    Ok(())
}

fn round_trip<MessageType>(message: &MessageType) -> Result<MessageType, prost::DecodeError>
where
    MessageType: Message + Default,
{
    let encoded = message.encode_to_vec();
    MessageType::decode(encoded.as_slice())
}
