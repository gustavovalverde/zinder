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
    ChainEpochId, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentOutputArtifact, TransparentSpendFact, TransparentUnspentOutput,
};
use zinder_proto::v1::wallet::{
    self, AddressLookup, address_lookup, wallet_query_server::WalletQuery as WalletQueryService,
};
use zinder_query::{WalletEndpointMetadata, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::PrimaryChainStore;
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{
    chain_epoch_artifacts_with_transparent_facts, split_unspent_outputs_stream,
    synthetic_chain_epoch,
};

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xAB; 32];
const SCRIPT_PUB_KEY: &[u8] = &[
    0x76, 0xa9, 0x14, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88,
    0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac,
];

fn unspent_outputs_request(
    start_height: u32,
    at_epoch_id: Option<ChainEpochId>,
) -> wallet::TransparentAddressUnspentOutputsRequest {
    wallet::TransparentAddressUnspentOutputsRequest {
        address: Some(AddressLookup {
            selector: Some(address_lookup::Selector::ScriptHash(
                ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
            )),
        }),
        start_height,
        at_epoch_id: at_epoch_id.map(ChainEpochId::value),
    }
}

async fn drain_unspent_outputs(
    grpc_adapter: &WalletQueryGrpcAdapter<WalletQuery<PrimaryChainStore>>,
    start_height: u32,
    at_epoch_id: Option<ChainEpochId>,
) -> eyre::Result<(wallet::ChainView, Vec<wallet::TransparentUnspentOutput>)> {
    let mut stream = WalletQueryService::transparent_address_unspent_outputs(
        grpc_adapter,
        Request::new(unspent_outputs_request(start_height, at_epoch_id)),
    )
    .await?
    .into_inner();
    let mut chunks = Vec::new();
    while let Some(message) = stream.next().await {
        chunks.push(message?);
    }
    split_unspent_outputs_stream(chunks)
}

#[tokio::test]
async fn transparent_address_unspent_outputs_streams_complete_set() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_utxos = commit_unspent_outputs(&store, ChainEpochId::new(1), 1, 3)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let (header, outputs) = drain_unspent_outputs(&grpc_adapter, 0, None).await?;

    assert_eq!(outputs.len(), stored_utxos.len());
    assert!(
        header.chain_epoch.is_some(),
        "the leading header carries the pinned chain epoch for the whole stream"
    );
    for (output, stored) in outputs.iter().zip(&stored_utxos) {
        assert_eq!(output.value_zat, stored.value_zat);
        assert_eq!(output.script_pub_key, stored.script_pub_key);
        assert_eq!(output.block_height, stored.block_height.value());
    }

    Ok(())
}

#[tokio::test]
async fn transparent_address_unspent_outputs_streams_across_multiple_internal_pages()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_utxos = commit_unspent_outputs(&store, ChainEpochId::new(1), 1, 1001)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let (_header, outputs) = drain_unspent_outputs(&grpc_adapter, 0, None).await?;

    assert_eq!(
        outputs.len(),
        stored_utxos.len(),
        "the stream is complete and cannot truncate at a page boundary"
    );

    Ok(())
}

#[tokio::test]
async fn transparent_address_unspent_outputs_honors_start_height_floor() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_utxos = commit_unspent_outputs(&store, ChainEpochId::new(1), 1, 3)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let (_at_header, at_mined_height) = drain_unspent_outputs(&grpc_adapter, 1, None).await?;
    assert_eq!(at_mined_height.len(), stored_utxos.len());

    let (_above_header, above_mined_height) = drain_unspent_outputs(&grpc_adapter, 2, None).await?;
    assert!(
        above_mined_height.is_empty(),
        "outputs mined below the wallet-birthday floor are excluded"
    );

    Ok(())
}

#[tokio::test]
async fn transparent_address_unspent_outputs_rejects_invalid_address_selector() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let _ = commit_unspent_outputs(&store, ChainEpochId::new(1), 1, 1)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let status = match WalletQueryService::transparent_address_unspent_outputs(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressUnspentOutputsRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::Address(String::from(
                    "not-an-address",
                ))),
            }),
            start_height: 0,
            at_epoch_id: None,
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

#[tokio::test]
async fn transparent_address_unspent_outputs_pinned_to_a_past_epoch_diverges_from_the_live_tip()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let funding_epoch = ChainEpochId::new(1);
    commit_address_output_then_spend(&store, funding_epoch)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let (live_header, live_outputs) = drain_unspent_outputs(&grpc_adapter, 0, None).await?;
    assert!(
        live_outputs.is_empty(),
        "at the live tip the funding output is already spent"
    );

    let (pinned_header, pinned_outputs) =
        drain_unspent_outputs(&grpc_adapter, 0, Some(funding_epoch)).await?;
    assert_eq!(
        pinned_outputs.len(),
        1,
        "pinned to the funding epoch the output is still unspent"
    );

    let pinned_epoch = pinned_header
        .chain_epoch
        .ok_or_else(|| eyre!("the pinned header must carry its chain epoch"))?;
    assert_eq!(pinned_epoch.chain_epoch_id, funding_epoch.value());
    let live_epoch = live_header
        .chain_epoch
        .ok_or_else(|| eyre!("the live header must carry its chain epoch"))?;
    assert_ne!(
        pinned_epoch.chain_epoch_id, live_epoch.chain_epoch_id,
        "the pin reads a different epoch than the live tip"
    );

    Ok(())
}

fn commit_address_output_then_spend(
    store: &PrimaryChainStore,
    funding_epoch: ChainEpochId,
) -> eyre::Result<()> {
    let (epoch_one, block_one, compact_one) = synthetic_chain_epoch(funding_epoch.value(), 1);
    let (epoch_two, block_two, compact_two) = synthetic_chain_epoch(funding_epoch.value() + 1, 2);

    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x21; 32]), 0);
    let output = TransparentOutputArtifact::new(
        outpoint,
        7_000_000,
        SCRIPT_PUB_KEY.to_vec(),
        address_script_hash,
        block_one.height,
        block_one.block_hash,
    );

    let spend = TransparentSpendFact::new(
        outpoint,
        0,
        TransactionId::from_bytes([0x33; 32]),
        0,
        block_two.height,
        block_two.block_hash,
        output.value_zat,
        address_script_hash,
        output.block_height,
        output.block_hash,
    );
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_one,
        vec![block_one],
        vec![compact_one],
        &[output],
        Vec::new(),
    )?)?;
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_two,
        vec![block_two],
        vec![compact_two],
        &[],
        vec![spend],
    )?)?;

    Ok(())
}

fn commit_unspent_outputs(
    store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    height: u32,
    utxo_count: u32,
) -> eyre::Result<Vec<TransparentUnspentOutput>> {
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(chain_epoch_id.value(), height);
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let mut utxos = Vec::new();
    for output_index in 0..utxo_count {
        let mut transaction_id_bytes = [0; 32];
        transaction_id_bytes[..4].copy_from_slice(&output_index.to_be_bytes());
        utxos.push(TransparentUnspentOutput::new(
            address_script_hash,
            SCRIPT_PUB_KEY.to_vec(),
            TransparentOutPoint::new(TransactionId::from_bytes(transaction_id_bytes), 0),
            1_000_000_u64 + u64::from(output_index),
            block.height,
            block.block_hash,
        ));
    }

    let prevouts = utxos
        .iter()
        .map(transparent_output_from_utxo)
        .collect::<Vec<_>>();
    let artifacts = chain_epoch_artifacts_with_transparent_facts(
        chain_epoch,
        vec![block],
        vec![compact_block],
        &prevouts,
        Vec::new(),
    )?;
    store.commit_chain_epoch(artifacts)?;

    Ok(utxos)
}

fn transparent_output_from_utxo(utxo: &TransparentUnspentOutput) -> TransparentOutputArtifact {
    TransparentOutputArtifact::new(
        utxo.outpoint,
        utxo.value_zat,
        utxo.script_pub_key.clone(),
        utxo.address_script_hash,
        utxo.block_height,
        utxo.block_hash,
    )
}
