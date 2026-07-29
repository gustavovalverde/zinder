//! Frozen P1 Zallet bounded-scan contract assertions.
//!
//! This fixture proves the public client shape used by the unshippable P1
//! tracer. It deliberately excludes the selector, transaction, mempool,
//! event, broadcast, transparent, and whole-sync behavior assigned to later
//! slices. Real current-Zallet execution remains separate certification.

use std::{num::NonZeroU32, sync::Arc};

use tokio_stream::StreamExt as _;
use zinder_client::{
    BlockHeight, BlockHeightRange, Capability, CapabilityDescriptor, ChainIndex,
    EndpointBackedIndex, OwnedChainSnapshot, RemoteChainIndex, ShieldedProtocol, SubtreeRootIndex,
    SubtreeRootRange,
};
use zinder_store::RawBlobRetention;

use super::{open_remote_chain_index, parity_chain_fixture};

#[test]
fn bounded_scan_surface_compiles_for_the_p1_zallet_tracer() {
    fn assert_base_compiles<T: ChainIndex>() {
        let _ = T::current_epoch;
        let _ = T::visible_tip_block;
        let _ = T::network_upgrade_activations;
        let _ = T::full_block_at;
        let _ = T::full_blocks_in_range;
        let _ = T::tree_state_at;
        let _ = T::subtree_roots_in_range;
    }
    fn assert_endpoint_compiles<T: EndpointBackedIndex>() {
        let _ = T::server_info;
    }
    fn assert_storable_chain_view<View: Clone + Send + Sync + 'static>() {}

    assert_base_compiles::<RemoteChainIndex>();
    assert_endpoint_compiles::<RemoteChainIndex>();

    assert_storable_chain_view::<OwnedChainSnapshot<RemoteChainIndex>>();
    assert_storable_chain_view::<OwnedChainSnapshot<dyn ChainIndex>>();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "The frozen P1 tracer scenario keeps exact capability preflight and every bounded epoch-bound read in one consumer flow."
)]
async fn serves_the_frozen_p1_bounded_scan_from_release_composition() -> eyre::Result<()> {
    let chain_fixture = parity_chain_fixture(2).with_raw_blob_retention(RawBlobRetention::All);
    let chain_index = open_remote_chain_index(&chain_fixture).await?;
    let server_info = chain_index.server_info().await?;
    for required in [
        Capability::ServerInfo,
        Capability::NetworkUpgradeActivations,
        Capability::VisibleTipBlock,
        Capability::TreeState,
        Capability::SubtreeRoots,
        Capability::SubtreeRootsIronwood,
        Capability::FullBlock,
        Capability::FullBlockRange,
    ] {
        assert!(
            server_info.supports(required.clone()),
            "release fixture omitted frozen P1 tracer capability {}",
            required.as_str()
        );
    }
    let activations = chain_index.network_upgrade_activations().await?;
    assert_eq!(
        activations,
        zinder_testkit::sample_regtest_upgrade_activations()
    );

    let chain_index = Arc::new(chain_index);
    let chain_view = OwnedChainSnapshot::capture(chain_index).await?;

    let visible_tip_block = chain_view.visible_tip_block().await?;
    assert_eq!(visible_tip_block.height, BlockHeight::new(2));
    let tree_state = chain_view.tree_state_at(BlockHeight::new(1)).await?;
    assert_eq!(tree_state.height, BlockHeight::new(1));

    let full_block = chain_view.full_block_at(BlockHeight::new(1)).await?;
    assert_eq!(full_block.height, BlockHeight::new(1));
    let mut full_blocks = chain_view
        .full_blocks_in_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))
        .await?;
    let mut full_block_heights = Vec::new();
    while let Some(block) = full_blocks.next().await {
        full_block_heights.push(block?.height);
    }
    assert_eq!(
        full_block_heights,
        vec![BlockHeight::new(1), BlockHeight::new(2)]
    );

    for protocol in [
        ShieldedProtocol::Sapling,
        ShieldedProtocol::Orchard,
        ShieldedProtocol::Ironwood,
    ] {
        let roots = chain_view
            .subtree_roots_in_range(SubtreeRootRange::new(
                protocol,
                SubtreeRootIndex::new(0),
                NonZeroU32::MIN,
            ))
            .await?;
        assert!(roots.len() <= 1);
    }

    Ok(())
}
