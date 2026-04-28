#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    Network, UnixTimestampMillis,
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
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };

    assert_eq!(chain_epoch.id, ChainEpochId::new(1));
    assert_eq!(chain_epoch.network, Network::ZcashRegtest);
    assert_eq!(chain_epoch.network.name(), "zcash-regtest");
    assert_eq!(chain_epoch.tip_height, BlockHeight::new(2));
    assert_eq!(chain_epoch.tip_hash, tip_hash);
    assert_eq!(chain_epoch.finalized_height, BlockHeight::new(1));
    assert_eq!(chain_epoch.finalized_hash, finalized_hash);
    assert_eq!(
        chain_epoch.artifact_schema_version,
        ArtifactSchemaVersion::new(1)
    );
    assert_eq!(
        chain_epoch.created_at,
        UnixTimestampMillis::new(1_774_668_000_000)
    );
}
