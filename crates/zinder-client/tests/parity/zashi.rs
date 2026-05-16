//! Zashi / Zodl parity assertions.
//!
//! Zashi (mobile, via `zcash-android-wallet-sdk`) consumes the lightwalletd
//! compat surface and the federated `explorer.*` surface. These
//! compile-time assertions ensure the `ChainIndex` methods Zashi's typed
//! gRPC paths depend on are present on the trait surface.
//!
//! Cross-references: [Integration surfaces](../../../../docs/reference/integration-surfaces.md).

use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tonic::Request;
use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_proto::compat::lightwalletd::{self, compact_tx_streamer_server::CompactTxStreamer};
use zinder_query::WalletQuery;
use zinder_testkit::sample_regtest_upgrade_activations;

use super::{committed_store_fixture, parity_chain_fixture};

#[test]
fn parity_chain_index_surface_compiles_for_zashi_use_cases() {
    fn assert_compiles<T: ChainIndex>() {
        // typed BlockSelector resolver (compact-block hash-only paths)
        let _ = T::block_id_by_selector;
        // mempool point lookups for unmined UTXO overlays
        let _ = T::transparent_mempool_outputs_by_address;
        let _ = T::transparent_mempool_spend_by_outpoint;
        // typed TransparentAddressBalance via federated derive
        let _ = T::transparent_address_balance;
        // typed subtree-root reads for SDK scan
        let _ = T::subtree_roots_in_range;
        // typed TxStatus envelope (raw decode + status disambiguation)
        let _ = T::transaction_by_id;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Parity acceptance scenario covers the full lightwalletd scan surface in one auditable flow."
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
    assert!(lightd_info.taddr_support);

    Ok(())
}
