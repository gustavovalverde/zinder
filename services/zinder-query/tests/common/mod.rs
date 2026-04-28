//! Test-support helpers shared across `zinder-query` integration tests.
//!
//! Each top-level test file pulls these in with `mod support;` and uses
//! `support::*` so synthetic chain-epoch shapes stay consistent.

#![allow(
    dead_code,
    reason = "Each test file uses a subset of these helpers; the full surface is shared."
)]
#![allow(
    unreachable_pub,
    reason = "Items are reachable via `mod support;` from each test binary."
)]

use prost::Message;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_proto::compat::lightwalletd::{ChainMetadata, CompactBlock as LightwalletdCompactBlock};

/// Builds a block hash whose 32 bytes repeat `seed` as four big-endian u32 chunks.
#[must_use]
pub fn block_hash_from_seed(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}

/// Creates a synthetic single-block chain epoch with deterministic identifiers.
#[must_use]
pub fn synthetic_chain_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
    let source_hash = block_hash_from_seed(height);
    let parent_hash = block_hash_from_seed(height.saturating_sub(1));
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
            created_at: UnixTimestampMillis::new(1_774_668_300_000 + u64::from(height)),
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

/// Creates a compact block artifact whose payload encodes the given commitment-tree sizes.
#[must_use]
pub fn compact_block_with_tree_sizes(
    height: BlockHeight,
    block_hash: BlockHash,
    sapling_commitment_tree_size: u32,
    orchard_commitment_tree_size: u32,
) -> CompactBlockArtifact {
    let payload_bytes = LightwalletdCompactBlock {
        proto_version: 1,
        height: u64::from(height.value()),
        hash: block_hash.as_bytes().into(),
        prev_hash: vec![0; 32],
        time: 1_774_668_300,
        header: Vec::new(),
        vtx: Vec::new(),
        chain_metadata: Some(ChainMetadata {
            sapling_commitment_tree_size,
            orchard_commitment_tree_size,
        }),
    }
    .encode_to_vec();

    CompactBlockArtifact::new(height, block_hash, payload_bytes)
}
