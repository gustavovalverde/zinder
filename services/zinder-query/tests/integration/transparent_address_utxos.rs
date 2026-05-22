#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tonic::{Code, Request};
use tonic_types::StatusExt;
use zinder_core::{
    ChainEpochId, TransactionId, TransparentAddressScriptHash, TransparentAddressUtxoArtifact,
    TransparentOutPoint, TransparentPrevoutArtifact,
};
use zinder_proto::v1::wallet::{
    self, AddressLookup, address_lookup, wallet_query_server::WalletQuery as WalletQueryService,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::{ChainEpochArtifacts, PrimaryChainStore};
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::synthetic_chain_epoch;

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xAB; 32];
const SCRIPT_PUB_KEY: &[u8] = &[
    0x76, 0xa9, 0x14, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88,
    0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac,
];

#[tokio::test]
async fn transparent_address_utxos_round_trip_through_native_grpc() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_utxos = commit_transparent_address_utxos(&store, ChainEpochId::new(1), 1, 3)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let response = WalletQueryService::transparent_address_utxos(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUtxosRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            max_entries: None,
            from_cursor: Vec::new(),
            at_epoch: None,
            start_height: 0,
        }),
    )
    .await?
    .into_inner();

    assert_eq!(response.utxos.len(), stored_utxos.len());
    assert!(response.next_cursor.is_empty());
    let mut stream = WalletQueryService::transparent_address_utxos_stream(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUtxosRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            max_entries: None,
            from_cursor: Vec::new(),
            at_epoch: None,
            start_height: 0,
        }),
    )
    .await?
    .into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }

    assert_eq!(chunks.len(), stored_utxos.len());

    Ok(())
}

#[tokio::test]
async fn transparent_address_utxos_paginates_with_cursor() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_utxos = commit_transparent_address_utxos(&store, ChainEpochId::new(1), 1, 4)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let first_page = WalletQueryService::transparent_address_utxos(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUtxosRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            max_entries: Some(2),
            from_cursor: Vec::new(),
            at_epoch: None,
            start_height: 0,
        }),
    )
    .await?
    .into_inner();
    assert_eq!(first_page.utxos.len(), 2);
    assert!(!first_page.next_cursor.is_empty());

    let second_page = WalletQueryService::transparent_address_utxos(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUtxosRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            max_entries: Some(10),
            from_cursor: first_page.next_cursor,
            at_epoch: None,
            start_height: 0,
        }),
    )
    .await?
    .into_inner();

    assert_eq!(
        first_page.utxos.len() + second_page.utxos.len(),
        stored_utxos.len()
    );
    assert!(second_page.next_cursor.is_empty());

    Ok(())
}

#[tokio::test]
async fn transparent_address_utxos_clamps_oversized_page_request() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let _stored_utxos = commit_transparent_address_utxos(&store, ChainEpochId::new(1), 1, 1001)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let response = WalletQueryService::transparent_address_utxos(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUtxosRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            max_entries: Some(u32::MAX),
            from_cursor: Vec::new(),
            at_epoch: None,
            start_height: 0,
        }),
    )
    .await?
    .into_inner();

    assert_eq!(response.utxos.len(), 1000);
    assert!(
        !response.next_cursor.is_empty(),
        "clamped page should expose a cursor when more rows remain"
    );

    Ok(())
}

#[tokio::test]
async fn transparent_address_utxos_rejects_invalid_address_selector() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let _ = commit_transparent_address_utxos(&store, ChainEpochId::new(1), 1, 1)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let status = match WalletQueryService::transparent_address_utxos(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUtxosRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::Address(String::from(
                    "not-an-address",
                ))),
            }),
            max_entries: None,
            from_cursor: Vec::new(),
            at_epoch: None,
            start_height: 0,
        }),
    )
    .await
    {
        Ok(_response) => return Err(eyre!("expected an error response, got success")),
        Err(status) => status,
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    let details = status.get_error_details();
    assert!(
        details.bad_request().is_some_and(|bad_request| bad_request
            .field_violations
            .iter()
            .any(|violation| violation.field == "address")),
        "expected a `address` field violation in BadRequest details"
    );

    Ok(())
}

fn commit_transparent_address_utxos(
    store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    height: u32,
    utxo_count: u32,
) -> eyre::Result<Vec<TransparentAddressUtxoArtifact>> {
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(chain_epoch_id.value(), height);
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let mut utxos = Vec::new();
    for output_index in 0..utxo_count {
        let mut transaction_id_bytes = [0; 32];
        transaction_id_bytes[..4].copy_from_slice(&output_index.to_be_bytes());
        utxos.push(TransparentAddressUtxoArtifact::new(
            address_script_hash,
            SCRIPT_PUB_KEY.to_vec(),
            TransparentOutPoint::new(
                TransactionId::from_bytes(transaction_id_bytes),
                output_index,
            ),
            1_000_000_u64 + u64::from(output_index),
            block.height,
            block.block_hash,
        ));
    }

    let prevouts = utxos
        .iter()
        .map(transparent_prevout_from_utxo)
        .collect::<Vec<_>>();
    let mut artifacts = ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block]);
    artifacts = artifacts.with_transparent_address_utxos(utxos.clone());
    artifacts = artifacts.with_transparent_prevouts(prevouts);
    store.commit_chain_epoch(artifacts)?;

    Ok(utxos)
}

fn transparent_prevout_from_utxo(
    utxo: &TransparentAddressUtxoArtifact,
) -> TransparentPrevoutArtifact {
    TransparentPrevoutArtifact::new(
        utxo.outpoint,
        utxo.value_zat,
        utxo.script_pub_key.clone(),
        utxo.address_script_hash,
        utxo.block_height,
        utxo.block_hash,
    )
}
