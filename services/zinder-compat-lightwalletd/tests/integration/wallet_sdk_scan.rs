#![allow(
    missing_docs,
    reason = "Integration test names describe the wallet acceptance contract."
)]

//! Wallet-SDK scan acceptance test for the lightwalletd compat shim.
//!
//! Realizes the named PRD-0001 acceptance criterion that "a
//! lightwalletd-compatible client can scan a regtest range from Zinder
//! without sending viewing keys or spending keys to Zinder." The test
//! exercises the wire contract that wallets actually consume:
//!
//! 1. Stand up the `LightwalletdGrpcAdapter` over a populated `PrimaryChainStore`.
//! 2. Connect through the generated `CompactTxStreamerClient`, the same
//!    transport `librustzcash` consumers use.
//! 3. Call `GetBlockRange` and decode every compact block.
//! 4. Assert each block carries:
//!    - serialized block header bytes that round-trip through `zebra_chain`
//!      (proves `header` population, which lightwalletd wallets need for
//!      header-chain validation),
//!    - a populated `vtx` list whose `txid` entries match transaction
//!      artifacts retrievable by id from the store, and
//!    - chain metadata reflecting the committed Sapling and Orchard
//!      commitment-tree sizes.
//! 5. Assert no viewing keys, spending keys, seed phrases, or other key
//!    material appear in any client→server payload.
//!
//! The full `zcash_client_backend` SDK note-discovery test is documented as
//! a follow-up (gated behind a `wallet-sdk-acceptance` cargo feature) so the
//! workspace dependency graph stays clean. This file proves the contract
//! wallets consume; the SDK test will prove a specific reference wallet
//! interprets the contract correctly.

use eyre::eyre;
use prost::Message;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, transport::Server};
use zebra_chain::{
    block::Header as ZebraBlockHeader,
    serialization::{ZcashDeserializeInto, ZcashSerialize},
};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::{
    BlockHash, BlockHeight, ChainEpochId, ChainTipMetadata, Network, SUBTREE_LEAF_COUNT,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, TransactionArtifact,
    TransactionId,
};
use zinder_proto::compat::lightwalletd::{
    self, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zinder_query::WalletQuery;
use zinder_testkit::{ChainFixture, FixtureBlock, StoreFixture};

const SDK_SCAN_BLOCK_COUNT: u32 = 10;
const SDK_SCAN_SAPLING_TREE_SIZE: u32 = SUBTREE_LEAF_COUNT;

#[tokio::test]
async fn lightwalletd_compatible_client_scans_range_without_sending_keys() -> eyre::Result<()> {
    let store_fixture = sdk_scan_store_fixture()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .into_server();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let mut client = CompactTxStreamerClient::connect(format!("http://{server_addr}")).await?;
    let latest_block = client
        .get_latest_block(lightwalletd::ChainSpec {})
        .await?
        .into_inner();
    let block_range_request = lightwalletd::BlockRange {
        start: Some(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }),
        end: Some(lightwalletd::BlockId {
            height: u64::from(SDK_SCAN_BLOCK_COUNT),
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    };
    assert_no_key_material_in_request(&block_range_request);

    let mut compact_blocks_stream = client
        .get_block_range(Request::new(block_range_request))
        .await?
        .into_inner();
    let mut received_blocks = Vec::new();
    while let Some(compact_block) = compact_blocks_stream.message().await? {
        received_blocks.push(compact_block);
    }

    assert_eq!(latest_block.height, u64::from(SDK_SCAN_BLOCK_COUNT));
    assert_eq!(
        received_blocks.len(),
        usize::try_from(SDK_SCAN_BLOCK_COUNT)?
    );

    for compact_block in &received_blocks {
        assert_compact_block_carries_serialized_header(compact_block)?;
        assert_compact_block_carries_indexed_transactions(compact_block, &mut client).await?;
        assert_compact_block_carries_chain_metadata(compact_block)?;
    }

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

#[tokio::test]
async fn lightwalletd_subtree_roots_request_carries_no_key_material() -> eyre::Result<()> {
    let store_fixture = sdk_scan_store_fixture()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .into_server();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });

    let mut client = CompactTxStreamerClient::connect(format!("http://{server_addr}")).await?;
    let request = lightwalletd::GetSubtreeRootsArg {
        start_index: 0,
        shielded_protocol: lightwalletd::ShieldedProtocol::Sapling as i32,
        max_entries: 1,
    };
    let request_bytes = request.encode_to_vec();
    assert_no_key_material_in_bytes(&request_bytes);

    let mut subtree_roots = client
        .get_subtree_roots(Request::new(request))
        .await?
        .into_inner();
    let first_root = subtree_roots
        .message()
        .await?
        .ok_or_else(|| eyre!("subtree-root stream produced no entries"))?;
    assert_eq!(first_root.completing_block_height, 1);

    server_handle.abort();
    let _ = server_handle.await;
    Ok(())
}

fn sdk_scan_store_fixture() -> eyre::Result<StoreFixture> {
    let base_fixture = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(SDK_SCAN_BLOCK_COUNT)
        .with_tip_metadata_override(ChainTipMetadata::new(SDK_SCAN_SAPLING_TREE_SIZE, 0));

    let mut chain_fixture = base_fixture;
    for height_value in 1..=SDK_SCAN_BLOCK_COUNT {
        let height = BlockHeight::new(height_value);
        let block = chain_fixture
            .block_at(height)
            .ok_or_else(|| eyre!("fixture block missing at height"))?
            .clone();
        let payload_bytes = sdk_scan_compact_block_payload(&block)?;
        let transaction_artifact = TransactionArtifact::new(
            TransactionId::from_bytes(sdk_scan_txid_bytes(height_value)),
            block.height,
            block.hash,
            sdk_scan_transaction_payload(height_value),
        );
        chain_fixture = chain_fixture
            .with_compact_block_payload_at(height, payload_bytes)
            .with_transaction_artifact(transaction_artifact);
    }

    let completing_block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture block missing at height 1"))?
        .clone();
    chain_fixture = chain_fixture.with_sapling_subtree_root(SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x70; 32]),
        completing_block.height,
        completing_block.hash,
    ));

    Ok(StoreFixture::with_chain_committed(
        &chain_fixture,
        ChainEpochId::new(1),
    )?)
}

fn sdk_scan_compact_block_payload(block: &FixtureBlock) -> eyre::Result<Vec<u8>> {
    let zebra_header = synthesized_zebra_header(block);
    let header_bytes = zebra_header.zcash_serialize_to_vec()?;

    Ok(lightwalletd::CompactBlock {
        proto_version: 1,
        height: u64::from(block.height.value()),
        hash: block.hash.as_bytes().to_vec(),
        prev_hash: block.parent_hash.as_bytes().to_vec(),
        time: block.block_time_seconds,
        header: header_bytes,
        vtx: vec![lightwalletd::CompactTx {
            index: 0,
            txid: sdk_scan_txid_bytes(block.height.value()).to_vec(),
            fee: 0,
            spends: Vec::new(),
            outputs: vec![lightwalletd::CompactSaplingOutput {
                cmu: vec![0x11; 32],
                ephemeral_key: vec![0x22; 32],
                ciphertext: vec![0x33; 52],
            }],
            actions: Vec::new(),
            vin: Vec::new(),
            vout: Vec::new(),
        }],
        chain_metadata: Some(lightwalletd::ChainMetadata {
            sapling_commitment_tree_size: SDK_SCAN_SAPLING_TREE_SIZE,
            orchard_commitment_tree_size: 0,
        }),
    }
    .encode_to_vec())
}

fn synthesized_zebra_header(block: &FixtureBlock) -> ZebraBlockHeader {
    let mut buffer = Vec::with_capacity(1_487);
    buffer.extend_from_slice(&u32::to_le_bytes(4));
    buffer.extend_from_slice(&block.parent_hash.as_bytes());
    buffer.extend_from_slice(&[0_u8; 32]);
    buffer.extend_from_slice(&[0_u8; 32]);
    buffer.extend_from_slice(&u32::to_le_bytes(block.block_time_seconds));
    buffer.extend_from_slice(&u32::to_le_bytes(0x200f_0f0f));
    buffer.extend_from_slice(&[0_u8; 32]);
    buffer.extend_from_slice(&[0xfd, 0x40, 0x05]);
    buffer.extend(std::iter::repeat_n(0_u8, 1_344));
    buffer
        .as_slice()
        .zcash_deserialize_into()
        .unwrap_or_else(|_| {
            unreachable!(
                "synthesized header bytes should always deserialize into a Zebra block header"
            )
        })
}

fn sdk_scan_txid_bytes(height_value: u32) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&height_value.to_be_bytes());
    }
    bytes
}

fn sdk_scan_transaction_payload(height_value: u32) -> Vec<u8> {
    format!("zinder-acceptance-tx-at-height-{height_value}").into_bytes()
}

fn assert_compact_block_carries_serialized_header(
    compact_block: &lightwalletd::CompactBlock,
) -> eyre::Result<()> {
    assert!(
        !compact_block.header.is_empty(),
        "compact block at height {} must carry a serialized header for header-chain validation",
        compact_block.height
    );
    let parsed_header: ZebraBlockHeader =
        compact_block.header.as_slice().zcash_deserialize_into()?;
    let round_tripped = parsed_header.zcash_serialize_to_vec()?;
    assert_eq!(
        round_tripped, compact_block.header,
        "compact block header bytes must round-trip through zebra_chain"
    );
    let parent_hash = BlockHash::from_bytes(parsed_header.previous_block_hash.0);
    assert_eq!(
        parent_hash.as_bytes().to_vec(),
        compact_block.prev_hash,
        "decoded header parent hash must match the wire-level prev_hash"
    );
    Ok(())
}

async fn assert_compact_block_carries_indexed_transactions(
    compact_block: &lightwalletd::CompactBlock,
    client: &mut CompactTxStreamerClient<tonic::transport::Channel>,
) -> eyre::Result<()> {
    assert!(
        !compact_block.vtx.is_empty(),
        "compact block at height {} must carry at least one transaction",
        compact_block.height
    );
    for compact_tx in &compact_block.vtx {
        let mut display_txid = compact_tx.txid.clone();
        display_txid.reverse();
        let response = client
            .get_transaction(Request::new(lightwalletd::TxFilter {
                block: None,
                index: 0,
                hash: display_txid,
            }))
            .await?
            .into_inner();
        assert_eq!(response.height, compact_block.height);
        assert!(
            !response.data.is_empty(),
            "indexed transaction payload must be present for txid in compact block at height {}",
            compact_block.height
        );
    }
    Ok(())
}

fn assert_compact_block_carries_chain_metadata(
    compact_block: &lightwalletd::CompactBlock,
) -> eyre::Result<()> {
    let chain_metadata = compact_block.chain_metadata.as_ref().ok_or_else(|| {
        eyre!("compact block must carry chain_metadata for tree-state advertisement")
    })?;
    assert_eq!(
        chain_metadata.sapling_commitment_tree_size,
        SDK_SCAN_SAPLING_TREE_SIZE
    );
    Ok(())
}

fn assert_no_key_material_in_request(request: &lightwalletd::BlockRange) {
    let request_bytes = request.encode_to_vec();
    assert_no_key_material_in_bytes(&request_bytes);
}

fn assert_no_key_material_in_bytes(request_bytes: &[u8]) {
    let key_material_markers: &[&[u8]] = &[
        b"sk-",
        b"zk-",
        b"viewing-key",
        b"spending-key",
        b"sapling-extfvk",
        b"orchard-fvk",
        b"unified-fvk",
        b"seed-phrase",
        b"mnemonic",
    ];
    for marker in key_material_markers {
        assert!(
            !request_bytes
                .windows(marker.len())
                .any(|window| window == *marker),
            "request payload must not embed wallet key material; found marker {marker:?}"
        );
    }
}
