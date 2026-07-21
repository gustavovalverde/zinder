#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use tonic::Request;
use zinder_core::{
    ChainEpochId, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentOutputArtifact,
};
use zinder_proto::v1::wallet::{
    TransparentUtxoSetSummaryRequest, wallet_query_server::WalletQuery as WalletQueryService,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::PrimaryChainStore;
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{chain_epoch_artifacts_with_transparent_facts, synthetic_chain_epoch};

const SCRIPT_PUB_KEY: &[u8] = &[
    0x76, 0xa9, 0x14, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88,
    0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac,
];

#[tokio::test]
async fn transparent_utxo_set_summary_counts_and_sums_the_unspent_set() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let total_value_zat = commit_unspent_outputs(
        &store,
        ChainEpochId::new(1),
        &[
            TransparentAddressScriptHash::from_bytes([0x11; 32]),
            TransparentAddressScriptHash::from_bytes([0x22; 32]),
            TransparentAddressScriptHash::from_bytes([0x33; 32]),
        ],
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let response = WalletQueryService::transparent_utxo_set_summary(
        &grpc_adapter,
        Request::new(TransparentUtxoSetSummaryRequest { at_epoch_id: None }),
    )
    .await?
    .into_inner();

    assert_eq!(response.utxo_count, 3);
    assert_eq!(response.total_value_zat, total_value_zat);
    assert_eq!(response.summarized_height, 1);
    assert!(
        response.commitment.is_none(),
        "the commitment field is absent when the operator did not opt in"
    );
    assert!(
        response
            .chain_view
            .and_then(|chain_view| chain_view.chain_epoch)
            .is_some(),
        "the summary response binds to one chain epoch"
    );
    Ok(())
}

#[tokio::test]
async fn transparent_utxo_set_summary_carries_the_commitment_when_enabled() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_unspent_outputs(
        &store,
        ChainEpochId::new(1),
        &[
            TransparentAddressScriptHash::from_bytes([0x11; 32]),
            TransparentAddressScriptHash::from_bytes([0x22; 32]),
        ],
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let settings = ServerInfoSettings {
        utxo_set_commitment_enabled: true,
        ..ServerInfoSettings::default()
    };
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, settings);

    let response = WalletQueryService::transparent_utxo_set_summary(
        &grpc_adapter,
        Request::new(TransparentUtxoSetSummaryRequest { at_epoch_id: None }),
    )
    .await?
    .into_inner();

    let commitment = response
        .commitment
        .ok_or_else(|| eyre::eyre!("commitment present when enabled"))?;
    assert_eq!(
        commitment.scheme,
        zinder_proto::v1::wallet::UtxoSetCommitmentScheme::Lthash16 as i32
    );
    assert_eq!(commitment.commitment.len(), 2048);
    Ok(())
}

#[tokio::test]
async fn transparent_utxo_set_summary_is_zero_for_an_empty_set() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let _ = commit_unspent_outputs(&store, ChainEpochId::new(1), &[])?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let response = WalletQueryService::transparent_utxo_set_summary(
        &grpc_adapter,
        Request::new(TransparentUtxoSetSummaryRequest { at_epoch_id: None }),
    )
    .await?
    .into_inner();

    assert_eq!(response.utxo_count, 0);
    assert_eq!(response.total_value_zat, 0);
    Ok(())
}

fn commit_unspent_outputs(
    store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    address_script_hashes: &[TransparentAddressScriptHash],
) -> eyre::Result<u64> {
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(chain_epoch_id.value(), 1);
    let mut prevouts = Vec::new();
    let mut total_value_zat: u64 = 0;
    for (address_index, address_script_hash) in address_script_hashes.iter().enumerate() {
        let mut transaction_id_bytes = [0; 32];
        let address_index = u64::try_from(address_index).unwrap_or(u64::MAX);
        transaction_id_bytes[..8].copy_from_slice(&address_index.to_be_bytes());
        let value_zat = 1_000_000_u64 + total_value_zat;
        total_value_zat = total_value_zat.saturating_add(value_zat);
        prevouts.push(TransparentOutputArtifact::new(
            TransparentOutPoint::new(TransactionId::from_bytes(transaction_id_bytes), 0),
            value_zat,
            SCRIPT_PUB_KEY.to_vec(),
            *address_script_hash,
            block.height,
            block.block_hash,
        ));
    }

    let artifacts = chain_epoch_artifacts_with_transparent_facts(
        chain_epoch,
        vec![block],
        vec![compact_block],
        &prevouts,
        Vec::new(),
    )?;
    store.commit_chain_epoch(artifacts)?;
    Ok(total_value_zat)
}
