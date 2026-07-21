#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use eyre::eyre;
use tonic::{Code, Request};
use tonic_types::StatusExt;
use zinder_core::{
    ChainEpochId, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentOutputArtifact,
};
use zinder_proto::v1::ops::ErrorReason;
use zinder_proto::v1::wallet::{
    AddressLookup, TransparentAddressBalanceRequest, address_lookup,
    wallet_query_server::WalletQuery as WalletQueryService,
};
use zinder_query::{
    MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES, ServerInfoSettings, WalletQuery,
    WalletQueryGrpcAdapter,
};
use zinder_store::PrimaryChainStore;
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{chain_epoch_artifacts_with_transparent_facts, synthetic_chain_epoch};

const SCRIPT_PUB_KEY: &[u8] = &[
    0x76, 0xa9, 0x14, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88,
    0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac,
];

fn script_hash_lookup(script_hash: TransparentAddressScriptHash) -> AddressLookup {
    AddressLookup {
        selector: Some(address_lookup::Selector::ScriptHash(
            script_hash.as_bytes().to_vec(),
        )),
    }
}

#[tokio::test]
async fn transparent_address_balance_sums_confirmed_unspent_across_addresses() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let address_a = TransparentAddressScriptHash::from_bytes([0x11; 32]);
    let address_b = TransparentAddressScriptHash::from_bytes([0x22; 32]);
    let value_a = commit_unspent_outputs(&store, ChainEpochId::new(1), &[address_a, address_b])?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let response = WalletQueryService::transparent_address_balance(
        &grpc_adapter,
        Request::new(TransparentAddressBalanceRequest {
            addresses: vec![script_hash_lookup(address_a), script_hash_lookup(address_b)],
            at_epoch_id: None,
        }),
    )
    .await?
    .into_inner();

    assert_eq!(response.confirmed_zat, value_a);
    assert_eq!(
        response.unconfirmed_delta_zat, 0,
        "no ingest-control endpoint is wired; the mempool overlay contributes nothing"
    );
    assert_eq!(response.address_count, 2);
    assert!(
        response
            .chain_view
            .and_then(|chain_view| chain_view.chain_epoch)
            .is_some(),
        "the balance response binds to one chain epoch"
    );
    Ok(())
}

#[tokio::test]
async fn transparent_address_balance_rejects_an_empty_address_list() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let _ = commit_unspent_outputs(
        &store,
        ChainEpochId::new(1),
        &[TransparentAddressScriptHash::from_bytes([0x11; 32])],
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let status = match WalletQueryService::transparent_address_balance(
        &grpc_adapter,
        Request::new(TransparentAddressBalanceRequest {
            addresses: Vec::new(),
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(_response) => return Err(eyre!("expected an error for an empty address list")),
        Err(status) => status,
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn transparent_address_balance_rejects_more_addresses_than_the_cap() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let _ = commit_unspent_outputs(
        &store,
        ChainEpochId::new(1),
        &[TransparentAddressScriptHash::from_bytes([0x11; 32])],
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let over_cap = MAX_TRANSPARENT_ADDRESS_BALANCE_ADDRESSES + 1;
    let addresses = (0..over_cap)
        .map(|index| {
            let mut script_hash_bytes = [0u8; 32];
            script_hash_bytes[..4].copy_from_slice(&index.to_be_bytes());
            script_hash_lookup(TransparentAddressScriptHash::from_bytes(script_hash_bytes))
        })
        .collect();

    let status = match WalletQueryService::transparent_address_balance(
        &grpc_adapter,
        Request::new(TransparentAddressBalanceRequest {
            addresses,
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(_response) => return Err(eyre!("expected an error above the per-request cap")),
        Err(status) => status,
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    let reason = status
        .get_error_details()
        .error_info()
        .map(|error_info| error_info.reason.clone());
    assert_eq!(
        reason,
        Some(
            ErrorReason::TransparentBalanceAddressCountExceeded
                .as_str_name()
                .to_owned()
        ),
        "the cap rejection carries the typed reason"
    );
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
