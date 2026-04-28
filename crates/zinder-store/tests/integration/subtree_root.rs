#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange, UnixTimestampMillis,
};
use zinder_store::{ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore};

#[test]
fn subtree_roots_read_from_one_visible_chain_epoch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x51; 32]),
        block.height,
        block.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_subtree_roots(vec![subtree_root.clone()]),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    let subtree_roots = reader.subtree_roots(SubtreeRootRange::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        NonZeroU32::new(2).ok_or_else(|| eyre!("invalid max entries"))?,
    ))?;

    assert_eq!(subtree_roots, vec![Some(subtree_root), None]);

    Ok(())
}

fn synthetic_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
    let source_hash = block_hash(height);
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            tip_height: block_height,
            tip_hash: source_hash,
            finalized_height: block_height,
            finalized_hash: source_hash,
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_600_000 + u64::from(height)),
        },
        BlockArtifact::new(
            block_height,
            source_hash,
            parent_hash,
            format!("raw-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
        CompactBlockArtifact::new(
            block_height,
            source_hash,
            format!("compact-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
    )
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
