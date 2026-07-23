//! Zodl parity assertions.
//!
//! Zodl (mobile, via `zcash-android-wallet-sdk`) consumes the lightwalletd
//! compat surface and the federated `explorer.*` surface. These
//! compile-time assertions ensure the `ChainIndex` methods Zodl's typed
//! gRPC paths depend on are present on the trait surface.
//!
//! Cross-references: [Integration surfaces](../../../../docs/reference/integration-surfaces.md).

use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tonic::Request;
use zinder_client::{ChainIndex, EndpointBackedIndex, RemoteChainIndex};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_proto::compat::lightwalletd::{self, compact_tx_streamer_server::CompactTxStreamer};
use zinder_query::{
    CanonicalReader, WalletProjectionReader, WalletServingQuery, WalletServingReadPair,
};
use zinder_testkit::{
    MockTransactionBroadcaster, WalletServingStoreFixture, sample_regtest_upgrade_activations,
};

use super::{
    build_transparent_address_adapter, build_transparent_address_serving_fixture,
    parity_chain_fixture,
};

#[test]
fn parity_chain_index_surface_compiles_for_zodl_use_cases() {
    fn assert_base_compiles<T: ChainIndex>() {
        // typed BlockSelector resolver (compact-block hash-only paths)
        let _ = T::block_id_by_selector;
        // typed TransparentAddressBalance from the canonical unspent index
        let _ = T::transparent_address_balance;
        // typed subtree-root reads for SDK scan
        let _ = T::subtree_roots_in_range;
        // typed TxStatus envelope (raw decode + status disambiguation)
        let _ = T::transaction_by_id;
    }
    fn assert_endpoint_compiles<T: EndpointBackedIndex>() {
        // mempool point lookups for unmined UTXO overlays
        let _ = T::transparent_mempool_outputs_by_address;
        let _ = T::transparent_mempool_spends_by_outpoint;
    }
    assert_base_compiles::<RemoteChainIndex>();
    assert_endpoint_compiles::<RemoteChainIndex>();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Parity acceptance scenario covers the claimed lightwalletd scan shape in one auditable flow."
)]
async fn serves_lightwalletd_scan_shape_from_fixture() -> eyre::Result<()> {
    let chain_fixture = parity_chain_fixture(2);
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let mut store_fixture =
        WalletServingStoreFixture::from_chain(&chain_fixture, activations.as_ref())?;
    let (canonical_reader, wallet_reader) = store_fixture.take_readers()?;
    let serving_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(canonical_reader) as Arc<dyn CanonicalReader>,
        Arc::new(wallet_reader) as Arc<dyn WalletProjectionReader>,
    )?);
    let serving_pair_slot = Arc::new(ArcSwap::from(serving_pair));
    let query = WalletServingQuery::from_serving_pair_slot(
        Arc::clone(&serving_pair_slot),
        MockTransactionBroadcaster::broadcast_disabled(),
        Arc::clone(&activations),
    );
    let adapter =
        LightwalletdGrpcAdapter::new(query, activations).with_serving_pair_slot(serving_pair_slot);

    let visible_tip_block = adapter
        .get_latest_block(Request::new(lightwalletd::ChainSpec {}))
        .await?
        .into_inner();
    let compact_block = adapter
        .get_block(Request::new(lightwalletd::BlockId {
            height: visible_tip_block.height,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();
    let mut compact_blocks = adapter
        .get_block_range(Request::new(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: 2,
                hash: Vec::new(),
            }),
            pool_types: vec![
                lightwalletd::PoolType::Sapling as i32,
                lightwalletd::PoolType::Orchard as i32,
            ],
        }))
        .await?
        .into_inner();
    let tree_state = adapter
        .get_latest_tree_state(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let lightd_info = adapter
        .get_lightd_info(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();

    let first_ranged_block = compact_blocks
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing first compact block"))??;
    let second_ranged_block = compact_blocks
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing second compact block"))??;

    assert_eq!(visible_tip_block.height, 2);
    assert_eq!(compact_block.height, visible_tip_block.height);
    assert_eq!(first_ranged_block.height, 1);
    assert_eq!(second_ranged_block.height, 2);
    assert!(compact_blocks.next().await.is_none());
    assert_eq!(tree_state.height, 2);
    assert!(tree_state.sapling_tree.is_empty());
    assert!(tree_state.orchard_tree.is_empty());
    assert_eq!(lightd_info.vendor, "Zinder");
    assert_eq!(lightd_info.chain_name, "test");
    assert_eq!(lightd_info.block_height, visible_tip_block.height);
    assert_eq!(lightd_info.estimated_height, visible_tip_block.height);
    assert_eq!(
        lightd_info.lightwallet_protocol_version,
        lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT
    );
    assert!(
        !lightd_info.taddr_support,
        "shielded scan-shape fixtures must not advertise transparent-address support"
    );

    Ok(())
}

#[tokio::test]
async fn serves_zodl_transparent_discovery_shape_from_production_pair() -> eyre::Result<()> {
    let mut fixture = build_transparent_address_serving_fixture()?;
    let adapter = build_transparent_address_adapter(&mut fixture)?;
    let info = adapter
        .get_lightd_info(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let utxos = adapter
        .get_address_utxos(Request::new(lightwalletd::GetAddressUtxosArg {
            addresses: vec![fixture.address.clone()],
            start_height: u64::from(fixture.block_height.value()),
            max_entries: 10,
        }))
        .await?
        .into_inner();
    let history_filter = address_history_filter(fixture.address.clone());
    let mut txids = adapter
        .get_taddress_txids(Request::new(history_filter.clone()))
        .await?
        .into_inner();
    let mut transactions = adapter
        .get_taddress_transactions(Request::new(history_filter))
        .await?
        .into_inner();
    let transaction = adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: None,
            index: 0,
            hash: fixture.transaction_id.as_bytes().to_vec(),
        }))
        .await?
        .into_inner();

    assert!(info.taddr_support);
    let utxo = utxos
        .address_utxos
        .first()
        .ok_or_else(|| eyre::eyre!("Zodl fixture must expose one UTXO"))?;
    assert_eq!(utxo.address, fixture.address);
    assert_eq!(utxo.txid, fixture.transaction_id.as_bytes());
    assert_eq!(utxo.script, fixture.script_pub_key);
    assert_eq!(utxo.value_zat, fixture.value_zat);
    assert_eq!(utxo.height, u64::from(fixture.block_height.value()));

    let txid_response = txids
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing transparent txid response"))??;
    let transaction_response = transactions
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing transparent transaction response"))??;
    assert_eq!(txid_response.data, fixture.raw_transaction_bytes);
    assert_eq!(transaction_response.data, fixture.raw_transaction_bytes);
    assert_eq!(transaction.data, fixture.raw_transaction_bytes);
    assert!(txids.next().await.is_none());
    assert!(transactions.next().await.is_none());
    Ok(())
}

fn address_history_filter(address: String) -> lightwalletd::TransparentAddressBlockFilter {
    lightwalletd::TransparentAddressBlockFilter {
        address,
        range: Some(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }),
    }
}
