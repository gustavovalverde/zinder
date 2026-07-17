#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use zinder_core::wire::{encode_rpc_block_hash_hex, encode_zinder_native_chain_name};
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
    ChainTipMetadata, Network, UnixTimestampMillis,
};

#[test]
fn chain_epoch_carries_the_visible_consistency_boundary() {
    let visible_tip_hash = BlockHash::from_bytes([7; 32]);
    let settled_tip_hash = BlockHash::from_bytes([3; 32]);

    let chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(2),
        visible_tip_hash,
        settled_tip_height: BlockHeight::new(1),
        settled_tip_hash,
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
    assert_eq!(chain_epoch.visible_tip_height, BlockHeight::new(2));
    assert_eq!(chain_epoch.visible_tip_hash, visible_tip_hash);
    assert_eq!(chain_epoch.settled_tip_height, BlockHeight::new(1));
    assert_eq!(chain_epoch.settled_tip_hash, settled_tip_hash);
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

#[test]
fn every_supported_network_has_the_authoritative_genesis_hash() {
    let expected = [
        (
            Network::ZcashMainnet,
            "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08",
        ),
        (
            Network::ZcashTestnet,
            "05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38",
        ),
        (
            Network::ZcashRegtest,
            "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
        ),
    ];

    for (network, rpc_hash) in expected {
        assert_eq!(encode_rpc_block_hash_hex(network.genesis_hash()), rpc_hash);
    }
}
