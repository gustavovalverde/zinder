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
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, TransparentOutputArtifact, TransparentSpendFact,
    UnixTimestampMillis,
};
use zinder_proto::compat::lightwalletd::{ChainMetadata, CompactBlock as LightwalletdCompactBlock};
use zinder_proto::v1::wallet;
use zinder_store::{CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEpochArtifacts};
use zinder_testkit::{
    FixtureTransactionRows, build_fixture_transaction_rows, encode_fixture_block_replay,
};

/// Builds commit-ready canonical artifacts whose replay envelopes and
/// transparent projection rows come from the same fixture transaction rows.
#[must_use]
pub fn chain_epoch_artifacts_with_transparent_facts(
    chain_epoch: ChainEpoch,
    block_headers: Vec<BlockHeaderArtifact>,
    compact_blocks: Vec<CompactBlockArtifact>,
    transparent_outputs: &[TransparentOutputArtifact],
    transparent_spends: Vec<TransparentSpendFact>,
) -> ChainEpochArtifacts {
    let transaction_rows =
        build_fixture_transaction_rows(&[], transparent_outputs, &transparent_spends);
    let block_replay_envelopes = block_headers
        .iter()
        .map(|block_header| {
            let block_transaction_rows = transaction_rows
                .iter()
                .filter(|rows| {
                    rows.location.block_height == block_header.height
                        && rows.location.block_hash == block_header.block_hash
                })
                .cloned()
                .collect::<Vec<_>>();
            encode_fixture_block_replay(block_header, &block_transaction_rows)
        })
        .collect();
    let mut artifacts = ChainEpochArtifacts::new(
        chain_epoch,
        block_headers,
        block_replay_envelopes,
        compact_blocks,
    );
    if !transaction_rows.is_empty() {
        artifacts = attach_fixture_transaction_rows(artifacts, &transaction_rows);
    }
    if !transparent_spends.is_empty() {
        artifacts = artifacts.with_transparent_spend_facts(transparent_spends);
    }
    artifacts
}

fn attach_fixture_transaction_rows(
    artifacts: ChainEpochArtifacts,
    transaction_rows: &[FixtureTransactionRows],
) -> ChainEpochArtifacts {
    artifacts
        .with_block_transaction_index(
            transaction_rows
                .iter()
                .map(|rows| rows.block_transaction_index)
                .collect(),
        )
        .with_transaction_locations(transaction_rows.iter().map(|rows| rows.location).collect())
        .with_transaction_facts(
            transaction_rows
                .iter()
                .map(|rows| rows.facts.clone())
                .collect(),
        )
        .with_transaction_intrinsic_value_balances(
            transaction_rows
                .iter()
                .filter_map(FixtureTransactionRows::intrinsic_value_balances_artifact)
                .collect(),
        )
        .with_transparent_outputs_by_outpoint(
            transaction_rows
                .iter()
                .flat_map(FixtureTransactionRows::transparent_output_artifacts)
                .collect(),
        )
}

/// Splits a `TransparentAddressUnspentOutputs` stream into the single leading
/// `ChainView` header and the payload items that follow it.
///
/// Asserts the structural stream-header contract: the first message is the
/// header, every later message is an item, and no second header appears.
pub fn split_unspent_outputs_stream(
    chunks: Vec<wallet::TransparentUnspentOutputsChunk>,
) -> eyre::Result<(wallet::ChainView, Vec<wallet::TransparentUnspentOutput>)> {
    let mut header: Option<wallet::ChainView> = None;
    let mut items = Vec::new();
    for chunk in chunks {
        match chunk
            .body
            .ok_or_else(|| eyre::eyre!("chunk carries no body"))?
        {
            wallet::transparent_unspent_outputs_chunk::Body::Header(chain_view) => {
                assert!(header.is_none(), "stream sent more than one header");
                assert!(items.is_empty(), "header must precede every item");
                header = Some(chain_view);
            }
            wallet::transparent_unspent_outputs_chunk::Body::Item(output) => {
                assert!(header.is_some(), "item arrived before the header");
                items.push(output);
            }
        }
    }
    let header = header.ok_or_else(|| eyre::eyre!("stream emits exactly one header"))?;
    Ok((header, items))
}

/// Splits a `TransparentAddressTxIdsInRange` stream into the single leading
/// `ChainView` header and the payload items that follow it.
///
/// Asserts the structural stream-header contract: the first message is the
/// header, every later message is an item, and no second header appears.
pub fn split_tx_ids_stream(
    chunks: Vec<wallet::TransparentAddressTxIdsChunk>,
) -> eyre::Result<(wallet::ChainView, Vec<wallet::TransparentAddressTxId>)> {
    let mut header: Option<wallet::ChainView> = None;
    let mut items = Vec::new();
    for chunk in chunks {
        match chunk
            .body
            .ok_or_else(|| eyre::eyre!("chunk carries no body"))?
        {
            wallet::transparent_address_tx_ids_chunk::Body::Header(chain_view) => {
                assert!(header.is_none(), "stream sent more than one header");
                assert!(items.is_empty(), "header must precede every item");
                header = Some(chain_view);
            }
            wallet::transparent_address_tx_ids_chunk::Body::Item(entry) => {
                assert!(header.is_some(), "item arrived before the header");
                items.push(entry);
            }
        }
    }
    let header = header.ok_or_else(|| eyre::eyre!("stream emits exactly one header"))?;
    Ok((header, items))
}

/// Builds a block hash whose 32 bytes repeat `seed` as four big-endian u32 chunks.
#[must_use]
pub fn block_hash_from_seed(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}

/// Returns the deterministic raw block bytes used by synthetic query fixtures.
#[must_use]
pub fn synthetic_raw_block_bytes(height: u32) -> Vec<u8> {
    format!("raw-block-{height}").into_bytes()
}

/// Creates a synthetic single-block chain epoch with deterministic identifiers.
#[must_use]
pub fn synthetic_chain_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let source_hash = block_hash_from_seed(height);
    let parent_hash = block_hash_from_seed(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            visible_tip_height: block_height,
            visible_tip_hash: source_hash,
            settled_tip_height: block_height,
            settled_tip_hash: source_hash,
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_300_000 + u64::from(height)),
        },
        BlockHeaderArtifact::new(
            block_height,
            source_hash,
            parent_hash,
            [0; 32],
            [0; 32],
            0,
            0,
            [0; 32],
            0,
            u64::try_from(synthetic_raw_block_bytes(height).len()).unwrap_or(u64::MAX),
        ),
        CompactBlockArtifact::new(
            block_height,
            source_hash,
            format!("compact-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
    )
}

/// Builds a chain epoch spanning blocks `1..=visible_tip` with an explicit
/// settled tip.
///
/// Unlike [`synthetic_chain_epoch`], the settled tip is decoupled from the
/// visible tip, so a caller can commit spends below the settled tip and then
/// advance it to exercise the retention sweep.
#[must_use]
pub fn synthetic_multi_block_epoch(
    chain_epoch_id: u64,
    visible_tip: u32,
    settled_tip: u32,
) -> (
    ChainEpoch,
    Vec<BlockHeaderArtifact>,
    Vec<CompactBlockArtifact>,
) {
    let chain_epoch = ChainEpoch {
        id: ChainEpochId::new(chain_epoch_id),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(visible_tip),
        visible_tip_hash: block_hash_from_seed(visible_tip),
        settled_tip_height: BlockHeight::new(settled_tip),
        settled_tip_hash: block_hash_from_seed(settled_tip),
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_300_000 + u64::from(visible_tip)),
    };
    let blocks = (1..=visible_tip).map(synthetic_block_header).collect();
    let compact_blocks = (1..=visible_tip).map(synthetic_block_compact).collect();
    (chain_epoch, blocks, compact_blocks)
}

fn synthetic_block_header(height: u32) -> BlockHeaderArtifact {
    BlockHeaderArtifact::new(
        BlockHeight::new(height),
        block_hash_from_seed(height),
        block_hash_from_seed(height.saturating_sub(1)),
        [0; 32],
        [0; 32],
        0,
        0,
        [0; 32],
        0,
        32,
    )
}

fn synthetic_block_compact(height: u32) -> CompactBlockArtifact {
    CompactBlockArtifact::new(
        BlockHeight::new(height),
        block_hash_from_seed(height),
        format!("compact-block-{height}").into_bytes(),
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
        height: u64::from(height.value()),
        hash: block_hash.as_bytes().into(),
        prev_hash: vec![0; 32],
        time: 1_774_668_300,
        header: Vec::new(),
        vtx: Vec::new(),
        chain_metadata: Some(ChainMetadata {
            sapling_commitment_tree_size,
            orchard_commitment_tree_size,
            ironwood_commitment_tree_size: 0,
        }),
    }
    .encode_to_vec();

    CompactBlockArtifact::new(height, block_hash, payload_bytes)
}
