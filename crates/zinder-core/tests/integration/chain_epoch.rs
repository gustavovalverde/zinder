#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
    ChainTipMetadata, Network, UnixTimestampMillis,
};

#[test]
fn chain_epoch_carries_the_visible_consistency_boundary() {
    let tip_hash = BlockHash::from_bytes([7; 32]);
    let finalized_hash = BlockHash::from_bytes([3; 32]);

    let chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        tip_height: BlockHeight::new(2),
        tip_hash,
        finalized_height: BlockHeight::new(1),
        finalized_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(10),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };

    assert_eq!(chain_epoch.id, ChainEpochId::new(1));
    assert_eq!(chain_epoch.network, Network::ZcashRegtest);
    assert_eq!(
        encode_zinder_native_chain_name(chain_epoch.network),
        "zcash-regtest"
    );
    assert_eq!(chain_epoch.tip_height, BlockHeight::new(2));
    assert_eq!(chain_epoch.tip_hash, tip_hash);
    assert_eq!(chain_epoch.finalized_height, BlockHeight::new(1));
    assert_eq!(chain_epoch.finalized_hash, finalized_hash);
    assert_eq!(
        chain_epoch.artifact_schema_version,
        ArtifactSchemaVersion::new(10)
    );
    assert_eq!(
        chain_epoch.created_at,
        UnixTimestampMillis::new(1_774_668_000_000)
    );
}

#[test]
fn block_height_next_advances_by_one_and_saturates_at_ceiling() {
    let height = BlockHeight::new(100);
    assert_eq!(height.next(), Some(BlockHeight::new(101)));

    let ceiling = BlockHeight::new(u32::MAX);
    assert_eq!(
        ceiling.next(),
        None,
        "BlockHeight::next must surface None at the chain-position ceiling so \
         stream-recovery loops terminate instead of overflowing"
    );
}

#[test]
fn block_height_range_empty_at_iterates_no_heights() {
    let empty_range = BlockHeightRange::empty_at(BlockHeight::new(100));
    assert!(empty_range.start > empty_range.end);
    assert_eq!(empty_range.into_iter().next(), None);

    let ceiling_empty_range = BlockHeightRange::empty_at(BlockHeight::new(u32::MAX));
    assert!(ceiling_empty_range.start > ceiling_empty_range.end);
    assert_eq!(ceiling_empty_range.into_iter().next(), None);
}
