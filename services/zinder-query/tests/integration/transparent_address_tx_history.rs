#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tonic::Request;
use zinder_core::{
    BlockArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, CompactBlockArtifact,
    Network, TransactionId, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    UnixTimestampMillis,
};
use zinder_proto::v1::wallet::{
    self, AddressLookup, address_lookup, wallet_query_server::WalletQuery as WalletQueryService,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts, PrimaryChainStore, ReorgWindowChange,
};
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{block_hash_from_seed, synthetic_chain_epoch};

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xEF; 32];

#[tokio::test]
async fn transparent_address_tx_ids_in_range_round_trips_through_native_grpc() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    commit_tx_index(&store, ChainEpochId::new(1), 1, address_script_hash, 5)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let mut stream = WalletQueryService::transparent_address_tx_ids_in_range(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressTxIdsInRangeRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            start_height: 0,
            end_height: 1000,
            max_entries: 0,
            from_cursor: Vec::new(),
            at_epoch: None,
            descending: false,
        }),
    )
    .await?
    .into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }

    assert_eq!(chunks.len(), 5);
    for (chunk, tx_index) in chunks.iter().zip(0_u32..) {
        assert_eq!(chunk.tx_index_in_block, tx_index);
        assert_eq!(chunk.block_height, 1);
        assert!(!chunk.transaction_id.is_empty());
        assert!(!chunk.block_hash.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn transparent_address_tx_ids_in_range_supports_descending() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    commit_tx_index(&store, ChainEpochId::new(1), 1, address_script_hash, 4)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let mut stream = WalletQueryService::transparent_address_tx_ids_in_range(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressTxIdsInRangeRequest {
            address: Some(AddressLookup {
                selector: Some(address_lookup::Selector::ScriptHash(
                    ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
                )),
            }),
            start_height: 0,
            end_height: 1000,
            max_entries: 0,
            from_cursor: Vec::new(),
            at_epoch: None,
            descending: true,
        }),
    )
    .await?
    .into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }

    assert_eq!(chunks.len(), 4);
    let tx_indexes: Vec<u32> = chunks.iter().map(|chunk| chunk.tx_index_in_block).collect();
    assert_eq!(tx_indexes, vec![3, 2, 1, 0]);
    Ok(())
}

#[tokio::test]
async fn transparent_address_tx_ids_cursor_preserves_descending_direction() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    commit_tx_index(&store, ChainEpochId::new(1), 1, address_script_hash, 4)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let first_page =
        collect_tx_history_chunks(&grpc_adapter, tx_history_request(2, Vec::new(), true)).await?;
    assert_eq!(
        first_page
            .iter()
            .map(|chunk| chunk.tx_index_in_block)
            .collect::<Vec<_>>(),
        vec![3, 2]
    );
    let cursor = first_page
        .last()
        .map(|chunk| chunk.cursor.clone())
        .filter(|cursor| !cursor.is_empty())
        .ok_or_else(|| eyre::eyre!("first page should include a resume cursor"))?;

    let resumed =
        collect_tx_history_chunks(&grpc_adapter, tx_history_request(10, cursor, false)).await?;

    assert_eq!(
        resumed
            .iter()
            .map(|chunk| chunk.tx_index_in_block)
            .collect::<Vec<_>>(),
        vec![1, 0]
    );
    Ok(())
}

#[tokio::test]
async fn transparent_address_tx_ids_clamps_oversized_page_request() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    commit_tx_index(&store, ChainEpochId::new(1), 1, address_script_hash, 1001)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let chunks = collect_tx_history_chunks(
        &grpc_adapter,
        tx_history_request(u32::MAX, Vec::new(), false),
    )
    .await?;

    assert_eq!(chunks.len(), 1000);
    assert!(
        chunks.last().is_some_and(|chunk| !chunk.cursor.is_empty()),
        "clamped page should expose a cursor when more rows remain"
    );
    Ok(())
}

#[tokio::test]
async fn transparent_address_tx_ids_returns_visible_replacement_after_reorg() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    commit_reorged_tx_index_rows(&store, address_script_hash)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let chunks =
        collect_tx_history_chunks(&grpc_adapter, tx_history_request(10, Vec::new(), false)).await?;

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].tx_index_in_block, 0);
    assert_eq!(chunks[0].transaction_id, vec![0x22; 32]);
    assert_eq!(
        chunks[0].block_hash,
        block_hash_from_seed(20).as_bytes().to_vec()
    );
    Ok(())
}

async fn collect_tx_history_chunks(
    grpc_adapter: &WalletQueryGrpcAdapter<WalletQuery<PrimaryChainStore, ()>>,
    request: wallet::TransparentAddressTxIdsInRangeRequest,
) -> eyre::Result<Vec<wallet::TransparentAddressTxIdsChunk>> {
    let mut stream = WalletQueryService::transparent_address_tx_ids_in_range(
        grpc_adapter,
        Request::new(request),
    )
    .await?
    .into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }

    Ok(chunks)
}

fn tx_history_request(
    max_entries: u32,
    from_cursor: Vec<u8>,
    descending: bool,
) -> wallet::TransparentAddressTxIdsInRangeRequest {
    wallet::TransparentAddressTxIdsInRangeRequest {
        address: Some(AddressLookup {
            selector: Some(address_lookup::Selector::ScriptHash(
                ADDRESS_SCRIPT_HASH_BYTES.to_vec(),
            )),
        }),
        start_height: 0,
        end_height: 1000,
        max_entries,
        from_cursor,
        at_epoch: None,
        descending,
    }
}

fn commit_reorged_tx_index_rows(
    store: &PrimaryChainStore,
    address_script_hash: TransparentAddressScriptHash,
) -> eyre::Result<()> {
    let (finalized_epoch, finalized_block, finalized_compact_block) = synthetic_chain_epoch(1, 1);
    let (mut initial_epoch, initial_block, initial_compact_block) = synthetic_chain_epoch(1, 2);
    initial_epoch.finalized_height = finalized_epoch.tip_height;
    initial_epoch.finalized_hash = finalized_epoch.tip_hash;
    let stale_artifact = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        initial_block.height,
        0,
        TransactionId::from_bytes([0x11; 32]),
        initial_block.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            initial_epoch,
            vec![finalized_block.clone(), initial_block],
            vec![finalized_compact_block, initial_compact_block],
        )
        .with_transparent_address_tx_index(vec![stale_artifact]),
    )?;

    let replacement_height = BlockHeight::new(2);
    let replacement_hash = block_hash_from_seed(20);
    let replacement_epoch = ChainEpoch {
        id: ChainEpochId::new(2),
        network: Network::ZcashRegtest,
        tip_height: replacement_height,
        tip_hash: replacement_hash,
        finalized_height: finalized_epoch.tip_height,
        finalized_hash: finalized_epoch.tip_hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_300_020),
    };
    let replacement_block = BlockArtifact::new(
        replacement_height,
        replacement_hash,
        finalized_block.block_hash,
        b"replacement-block-2".to_vec(),
    );
    let replacement_compact_block = CompactBlockArtifact::new(
        replacement_height,
        replacement_hash,
        b"replacement-compact-block-2".to_vec(),
    );
    let visible_artifact = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        replacement_height,
        0,
        TransactionId::from_bytes([0x22; 32]),
        replacement_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_transparent_address_tx_index(vec![visible_artifact])
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: replacement_height,
        }),
    )?;

    Ok(())
}

fn commit_tx_index(
    store: &PrimaryChainStore,
    chain_epoch_id: ChainEpochId,
    height: u32,
    address_script_hash: TransparentAddressScriptHash,
    entries: u32,
) -> eyre::Result<()> {
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(chain_epoch_id.value(), height);
    let mut artifacts = Vec::new();
    for tx_index in 0..entries {
        let mut transaction_id_bytes = [0; 32];
        transaction_id_bytes[..4].copy_from_slice(&tx_index.to_be_bytes());
        artifacts.push(TransparentAddressTxIndexArtifact::new(
            address_script_hash,
            block.height,
            tx_index,
            TransactionId::from_bytes(transaction_id_bytes),
            block.block_hash,
        ));
    }

    let mut commit = ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block]);
    commit = commit.with_transparent_address_tx_index(artifacts);
    store.commit_chain_epoch(commit)?;
    Ok(())
}
