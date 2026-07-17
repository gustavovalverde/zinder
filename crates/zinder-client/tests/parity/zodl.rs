//! Zodl parity assertions.
//!
//! Zodl (mobile, via `zcash-android-wallet-sdk`) consumes the lightwalletd
//! compat surface and the federated `explorer.*` surface. These
//! compile-time assertions ensure the `ChainIndex` methods Zodl's typed
//! gRPC paths depend on are present on the trait surface.
//!
//! Cross-references: [Integration surfaces](../../../../docs/reference/integration-surfaces.md).

use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tonic::Request;
use zinder_client::{ChainIndex, EndpointBackedIndex, LocalChainIndex, RemoteChainIndex};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_proto::compat::lightwalletd::{self, compact_tx_streamer_server::CompactTxStreamer};
use zinder_query::{WalletQuery, derive_store_wallet_projection_reader};
use zinder_testkit::sample_regtest_upgrade_activations;

use super::{committed_store_fixture, parity_chain_fixture, transparent_address_history_fixture};

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
    assert_base_compiles::<LocalChainIndex>();
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
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(store_fixture.chain_store().clone(), (), activations.clone()),
        activations,
    );

    let latest_block = adapter
        .get_latest_block(Request::new(lightwalletd::ChainSpec {}))
        .await?
        .into_inner();
    let compact_block = adapter
        .get_block(Request::new(lightwalletd::BlockId {
            height: latest_block.height,
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
    let mut subtree_roots = adapter
        .get_subtree_roots(Request::new(lightwalletd::GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: lightwalletd::ShieldedProtocol::Sapling as i32,
            max_entries: 1,
        }))
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
    let subtree_root = subtree_roots
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing subtree root"))??;

    assert_eq!(latest_block.height, 2);
    assert_eq!(compact_block.height, latest_block.height);
    assert_eq!(first_ranged_block.height, 1);
    assert_eq!(second_ranged_block.height, 2);
    assert!(compact_blocks.next().await.is_none());
    assert_eq!(tree_state.height, 2);
    assert_eq!(tree_state.sapling_tree, "000000");
    assert_eq!(tree_state.orchard_tree, "111111");
    assert_eq!(subtree_root.completing_block_height, 2);
    assert_eq!(lightd_info.vendor, "Zinder");
    assert_eq!(lightd_info.chain_name, "test");
    assert_eq!(lightd_info.block_height, latest_block.height);
    assert_eq!(lightd_info.estimated_height, latest_block.height);
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
#[allow(
    clippy::too_many_lines,
    reason = "The Zodl-shaped transparent flow keeps the wallet-facing RPC expectations together."
)]
async fn serves_zodl_transparent_discovery_shape_from_fixture() -> eyre::Result<()> {
    let fixture = transparent_address_history_fixture()?;
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            fixture.store_fixture.chain_store().clone(),
            (),
            activations.clone(),
        )
        .with_derive_store(fixture.derive_store.clone()),
        activations,
    )
    .with_transparent_address_support()
    .with_wallet_projection_reader(derive_store_wallet_projection_reader(
        fixture.derive_store.clone(),
    ));

    let lightd_info = adapter
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
    let mut txids = adapter
        .get_taddress_txids(Request::new(address_history_filter(
            fixture.address.clone(),
        )))
        .await?
        .into_inner();
    let mut transactions = adapter
        .get_taddress_transactions(Request::new(address_history_filter(
            fixture.address.clone(),
        )))
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

    assert!(
        lightd_info.taddr_support,
        "Zodl-shaped transparent discovery requires the lightwalletd support signal"
    );
    assert_eq!(utxos.address_utxos.len(), 1);
    let utxo = utxos
        .address_utxos
        .first()
        .ok_or_else(|| eyre::eyre!("transparent fixture must expose one UTXO"))?;
    assert_eq!(utxo.address, fixture.address);
    assert_eq!(utxo.txid, fixture.transaction_id.as_bytes().to_vec());
    assert_eq!(utxo.index, 0);
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
    assert_eq!(
        txid_response.height,
        u64::from(fixture.block_height.value())
    );
    assert_eq!(transaction_response.data, fixture.raw_transaction_bytes);
    assert_eq!(
        transaction_response.height,
        u64::from(fixture.block_height.value())
    );
    assert_eq!(transaction.data, fixture.raw_transaction_bytes);
    assert_eq!(transaction.height, u64::from(fixture.block_height.value()));
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
