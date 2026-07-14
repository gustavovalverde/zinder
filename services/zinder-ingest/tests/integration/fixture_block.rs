#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::error::Error;

use prost::Message;
use serde_json::Value;
use zebra_chain::{
    block::Header as ZebraBlockHeader,
    serialization::{ZcashDeserializeInto, ZcashSerialize},
};
use zinder_core::{
    ArtifactSchemaVersion, BlockBlobArtifact, BlockHash, BlockHeight,
    CanonicalBlockFactsDigestVersion, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalTransactionFacts, ChainEpoch, ChainEpochId,
    ChainTipMetadata, Network, PositionedCanonicalBlock, TransparentInputFact, UnixTimestampMillis,
};
use zinder_ingest::{
    BlockMismatchField, CanonicalBlockConstructionError, CommitmentTreeSizes, RawBlobPolicy,
    finalize_canonical_block, prepare_canonical_block_with_raw_blob_policy,
};
use zinder_proto::compat::lightwalletd::CompactBlock;
use zinder_source::{SourceBlock, decode_rpc_block_hash};
use zinder_store::ChainEpochArtifacts;
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

/// Runs the production two-stage pipeline against an empty offset.
///
/// Calls `prepare_canonical_block` then `finalize_canonical_block` on `source_block`
/// with a zeroed running commitment-tree position, returning the
/// `PositionedCanonicalBlock`. Use this when a test wants the block-local facts
/// together with position-dependent compact metadata.
fn build_positioned_block_for_test(
    source_block: &SourceBlock,
) -> Result<PositionedCanonicalBlock, CanonicalBlockConstructionError> {
    let activations = sample_regtest_upgrade_activations();
    let prepared = prepare_canonical_block_with_raw_blob_policy(
        source_block,
        &activations,
        RawBlobPolicy::All,
    )?;
    let mut tree_sizes = CommitmentTreeSizes::default();
    finalize_canonical_block(prepared, &mut tree_sizes)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "this end-to-end fixture test asserts the full artifact surface in one place"
)]
fn fixture_block_builds_canonical_facts() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block()?;
    let block = build_positioned_block_for_test(&source_block)?;
    let block_header_artifact = block.facts.block_header.clone();
    let raw_block_bytes = block
        .facts
        .raw_block_bytes
        .clone()
        .ok_or("fixture construction must include raw block bytes")?;
    let block_blob_artifact = BlockBlobArtifact::new(
        block_header_artifact.height,
        block_header_artifact.block_hash,
        block_header_artifact.parent_hash,
        raw_block_bytes,
    );
    let compact_block_artifact = block.compact_block;

    assert_eq!(block_header_artifact.height, BlockHeight::new(1));
    assert_eq!(block_header_artifact.block_hash, source_block.hash);
    assert_eq!(block_header_artifact.parent_hash, source_block.parent_hash);
    assert_eq!(block_header_artifact.block_time, 1_296_694_002);
    assert_eq!(block_blob_artifact.height, BlockHeight::new(1));
    assert_eq!(block_blob_artifact.block_hash, source_block.hash);
    assert_eq!(block_blob_artifact.parent_hash, source_block.parent_hash);
    assert_eq!(
        block_blob_artifact.raw_block_bytes,
        source_block.raw_block_bytes
    );

    let compact_block = CompactBlock::decode(compact_block_artifact.payload_bytes.as_slice())?;
    assert_eq!(compact_block.height, 1);
    assert_eq!(compact_block.hash, source_block.hash.as_bytes().to_vec());
    assert_eq!(
        compact_block.prev_hash,
        source_block.parent_hash.as_bytes().to_vec()
    );
    assert_eq!(compact_block.time, 1_296_694_002);
    assert!(
        !compact_block.header.is_empty(),
        "compact block must carry serialized header bytes for lightwalletd-compatible scanning"
    );
    let parsed_header: ZebraBlockHeader =
        compact_block.header.as_slice().zcash_deserialize_into()?;
    assert_eq!(
        parsed_header.previous_block_hash.0,
        source_block.parent_hash.as_bytes()
    );
    let round_tripped = parsed_header.zcash_serialize_to_vec()?;
    assert_eq!(round_tripped, compact_block.header);
    let chain_metadata = compact_block
        .chain_metadata
        .as_ref()
        .ok_or("compact block missing chain metadata")?;
    assert_eq!(chain_metadata.sapling_commitment_tree_size, 0);
    assert_eq!(chain_metadata.orchard_commitment_tree_size, 0);

    let compact_transaction = compact_block
        .vtx
        .first()
        .ok_or("compact block missing coinbase compact transaction")?;
    assert_eq!(compact_block.vtx.len(), 1);
    assert_eq!(compact_transaction.index, 0);
    assert_eq!(compact_transaction.txid.len(), 32);
    assert!(compact_transaction.vin.is_empty());
    assert!(compact_transaction.spends.is_empty());
    assert!(compact_transaction.outputs.is_empty());
    assert!(compact_transaction.actions.is_empty());

    let transparent_output = compact_transaction
        .vout
        .first()
        .ok_or("compact transaction missing coinbase output")?;
    assert_eq!(compact_transaction.vout.len(), 1);
    assert_eq!(transparent_output.value, 625_000_000);
    assert_eq!(
        hex::encode(&transparent_output.script_pub_key),
        "76a914b75028cd1ea0ca554fd5e7c8cc7ad70a89b8dd4f88ac"
    );

    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        visible_tip_height: source_block.height,
        visible_tip_hash: source_block.hash,
        settled_tip_height: source_block.height,
        settled_tip_hash: source_block.hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_669_000_000),
    };

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            chain_epoch,
            vec![block_header_artifact.clone()],
            vec![compact_block_artifact.clone()],
        )
        .with_block_blobs(vec![block_blob_artifact.clone()]),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.block_header_at(source_block.height)?,
        Some(block_header_artifact)
    );
    assert_eq!(
        reader.block_blob_at(source_block.height)?,
        Some(block_blob_artifact)
    );
    assert_eq!(
        reader.compact_block_at(source_block.height)?,
        Some(compact_block_artifact)
    );

    Ok(())
}

#[test]
fn canonical_block_facts_keep_transparent_state_block_local() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block_from(
        Network::ZcashRegtest,
        include_str!("../fixtures/z3-regtest-ironwood-block-603.json"),
    )?;
    let PositionedCanonicalBlock { facts, .. } = build_positioned_block_for_test(&source_block)?;
    let transparent_input = facts
        .transactions
        .iter()
        .flat_map(|transaction| &transaction.transparent_inputs)
        .find(|input| !input.spent_outpoint.is_coinbase_sentinel())
        .ok_or("Ironwood fixture must contain a non-coinbase transparent input")?;
    let &TransparentInputFact {
        input_index,
        spent_outpoint,
    } = transparent_input;

    assert_eq!(input_index, 0);
    assert!(!spent_outpoint.is_coinbase_sentinel());
    assert!(
        facts
            .transactions
            .iter()
            .any(|transaction| !transaction.transparent_outputs.is_empty())
    );

    Ok(())
}

#[test]
fn canonical_block_facts_digest_is_stable_and_content_sensitive() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block()?;
    let facts = build_positioned_block_for_test(&source_block)?.facts;
    let repeated_facts = build_positioned_block_for_test(&source_block)?.facts;
    let digest = facts.digest(CanonicalBlockFactsDigestVersion::CURRENT);

    assert_eq!(
        digest,
        repeated_facts.digest(CanonicalBlockFactsDigestVersion::CURRENT)
    );
    assert_eq!(
        hex::encode(digest.as_bytes()),
        "17e75ee4e0f124a8d1a638c1674a410c1cce54f44f5874076a1bf737f4914bab"
    );

    let mut changed_facts = facts;
    changed_facts.block_header.block_time = changed_facts
        .block_header
        .block_time
        .checked_add(1)
        .ok_or("fixture block time cannot be incremented")?;
    let changed_digest = changed_facts.digest(CanonicalBlockFactsDigestVersion::CURRENT);
    assert_ne!(digest, changed_digest);

    let mut reference_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
        CanonicalBlockFactsSequenceDigestVersion::CURRENT,
    );
    reference_builder.try_append(digest)?;
    reference_builder.try_append(changed_digest)?;
    let reference_sequence_digest = reference_builder.finish();
    assert_eq!(
        reference_sequence_digest.version(),
        CanonicalBlockFactsSequenceDigestVersion::CURRENT
    );
    assert_eq!(
        hex::encode(reference_sequence_digest.as_bytes()),
        "899f55e335ed4263f017c0c54dd65661e4788f45cb3bc7dbad1be25692440233"
    );

    let mut repeated_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
        CanonicalBlockFactsSequenceDigestVersion::CURRENT,
    );
    repeated_builder.try_append(digest)?;
    repeated_builder.try_append(changed_digest)?;
    assert_eq!(reference_sequence_digest, repeated_builder.finish());
    assert_eq!(reference_sequence_digest.block_count(), 2);

    let mut reordered_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
        CanonicalBlockFactsSequenceDigestVersion::CURRENT,
    );
    reordered_builder.try_append(changed_digest)?;
    reordered_builder.try_append(digest)?;
    assert_ne!(reference_sequence_digest, reordered_builder.finish());

    Ok(())
}

#[test]
fn ironwood_canonical_block_facts_digest_matches_known_answer() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block_from(
        Network::ZcashRegtest,
        include_str!("../fixtures/z3-regtest-ironwood-block-603.json"),
    )?;
    let facts = build_positioned_block_for_test(&source_block)?.facts;

    assert!(facts.transactions.len() > 1);
    assert!(facts.transactions.iter().any(|transaction| {
        transaction.public_facts.version == zinder_core::TransactionVersion::V6
            && transaction.public_facts.auth_digest.is_some()
            && !transaction.transparent_inputs.is_empty()
    }));
    assert_eq!(
        hex::encode(
            facts
                .digest(CanonicalBlockFactsDigestVersion::CURRENT)
                .as_bytes()
        ),
        "5bd48d01beffa241d8c2fc416c39c2a2bcd0b511f74f8e6c0b27739bdc008281"
    );

    Ok(())
}

#[test]
fn fixture_block_transaction_envelope_preserves_order_and_raw_bytes() -> Result<(), Box<dyn Error>>
{
    let source_block = fixture_source_block()?;
    let block = build_positioned_block_for_test(&source_block)?;
    let transaction_facts = block.facts.transactions;
    assert_eq!(
        transaction_facts.len(),
        1,
        "regtest fixture block 1 has a single coinbase transaction"
    );
    let coinbase_facts = transaction_facts
        .first()
        .ok_or("transaction facts vector is empty")?;
    let coinbase_blob = coinbase_facts
        .raw_transaction_bytes
        .as_ref()
        .ok_or("coinbase raw bytes are missing")?;
    assert!(
        !coinbase_blob.is_empty(),
        "coinbase serialized payload bytes must be present"
    );
    assert_eq!(coinbase_facts.transparent_outputs.len(), 1);
    assert_eq!(coinbase_facts.transparent_outputs[0].output_index, 0);
    assert_coinbase_branch_id_uses_activation_table(&transaction_facts, &source_block)?;

    Ok(())
}

fn assert_coinbase_branch_id_uses_activation_table(
    transaction_facts: &[CanonicalTransactionFacts],
    source_block: &SourceBlock,
) -> Result<(), Box<dyn Error>> {
    let coinbase_facts = transaction_facts
        .first()
        .ok_or("transaction facts vector is empty")?;
    assert_eq!(
        coinbase_facts.public_facts.consensus_branch_id,
        Some(sample_regtest_upgrade_activations().consensus_branch_id_at(source_block.height)),
        "mined transaction facts must use the network-upgrade activation table"
    );
    Ok(())
}

#[test]
fn testnet_sapling_block_compact_artifact_carries_sapling_outputs() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block_from(
        Network::ZcashTestnet,
        include_str!("../fixtures/zcash-testnet-sapling-block-1842432.json"),
    )?;
    let compact_block_artifact = build_positioned_block_for_test(&source_block)?.compact_block;
    let compact_block = CompactBlock::decode(compact_block_artifact.payload_bytes.as_slice())?;
    let chain_metadata = compact_block
        .chain_metadata
        .as_ref()
        .ok_or("compact block missing chain metadata")?;

    assert_eq!(compact_block.height, 1_842_432);
    assert_eq!(chain_metadata.sapling_commitment_tree_size, 1);
    assert_eq!(chain_metadata.orchard_commitment_tree_size, 0);

    let sapling_transaction = compact_block
        .vtx
        .iter()
        .find(|transaction| !transaction.outputs.is_empty())
        .ok_or("compact block missing Sapling-bearing transaction")?;
    assert_eq!(sapling_transaction.outputs.len(), 1);
    assert!(sapling_transaction.actions.is_empty());

    let sapling_output = sapling_transaction
        .outputs
        .first()
        .ok_or("compact transaction missing Sapling output")?;
    assert_eq!(sapling_output.cmu.len(), 32);
    assert_eq!(sapling_output.ephemeral_key.len(), 32);
    assert_eq!(sapling_output.ciphertext.len(), 52);

    Ok(())
}

#[test]
fn testnet_orchard_block_compact_artifact_carries_orchard_actions() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block_from(
        Network::ZcashTestnet,
        include_str!("../fixtures/zcash-testnet-orchard-block-1842462.json"),
    )?;
    let compact_block_artifact = build_positioned_block_for_test(&source_block)?.compact_block;
    let compact_block = CompactBlock::decode(compact_block_artifact.payload_bytes.as_slice())?;
    let chain_metadata = compact_block
        .chain_metadata
        .as_ref()
        .ok_or("compact block missing chain metadata")?;

    assert_eq!(compact_block.height, 1_842_462);
    assert_eq!(chain_metadata.sapling_commitment_tree_size, 0);
    assert_eq!(chain_metadata.orchard_commitment_tree_size, 2);

    let orchard_transaction = compact_block
        .vtx
        .iter()
        .find(|transaction| !transaction.actions.is_empty())
        .ok_or("compact block missing Orchard-bearing transaction")?;
    assert_eq!(orchard_transaction.actions.len(), 2);
    assert!(orchard_transaction.outputs.is_empty());

    for action in &orchard_transaction.actions {
        assert_eq!(action.nullifier.len(), 32);
        assert_eq!(action.cmx.len(), 32);
        assert_eq!(action.ephemeral_key.len(), 32);
        assert_eq!(action.ciphertext.len(), 52);
    }

    Ok(())
}

#[test]
fn regtest_ironwood_block_compact_artifact_carries_ironwood_actions() -> Result<(), Box<dyn Error>>
{
    let source_block = fixture_source_block_from(
        Network::ZcashRegtest,
        include_str!("../fixtures/z3-regtest-ironwood-block-603.json"),
    )?;
    let compact_block_artifact = build_positioned_block_for_test(&source_block)?.compact_block;
    let compact_block = CompactBlock::decode(compact_block_artifact.payload_bytes.as_slice())?;
    let chain_metadata = compact_block
        .chain_metadata
        .as_ref()
        .ok_or("compact block missing chain metadata")?;

    assert_eq!(compact_block.height, 603);
    assert_eq!(chain_metadata.orchard_commitment_tree_size, 0);
    assert_eq!(chain_metadata.ironwood_commitment_tree_size, 2);

    let ironwood_transaction = compact_block
        .vtx
        .iter()
        .find(|transaction| !transaction.ironwood_actions.is_empty())
        .ok_or("compact block missing Ironwood-bearing transaction")?;
    assert_eq!(ironwood_transaction.ironwood_actions.len(), 2);
    assert!(ironwood_transaction.actions.is_empty());
    assert!(ironwood_transaction.vout.is_empty());
    assert_eq!(ironwood_transaction.vin.len(), 1);

    for action in &ironwood_transaction.ironwood_actions {
        assert_eq!(action.nullifier.len(), 32);
        assert_eq!(action.cmx.len(), 32);
        assert_eq!(action.ephemeral_key.len(), 32);
        assert_eq!(action.ciphertext.len(), 52);
    }

    Ok(())
}

#[test]
fn regtest_block_without_orchard_actions_carries_forward_tree_size() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block()?;
    let activations = sample_regtest_upgrade_activations();
    let prepared = prepare_canonical_block_with_raw_blob_policy(
        &source_block,
        &activations,
        RawBlobPolicy::All,
    )?;

    assert_eq!(prepared.tree_size_additions.orchard, 0);
    assert_eq!(prepared.tree_size_additions.sapling, 0);
    assert_eq!(prepared.tree_size_additions.ironwood, 0);

    let mut running_tree_sizes = CommitmentTreeSizes {
        sapling: 11,
        orchard: 42,
        ironwood: 7,
    };
    let positioned_block = finalize_canonical_block(prepared, &mut running_tree_sizes)?;

    assert_eq!(running_tree_sizes.orchard, 42);
    assert_eq!(running_tree_sizes.sapling, 11);
    assert_eq!(running_tree_sizes.ironwood, 7);

    let compact_block =
        CompactBlock::decode(positioned_block.compact_block.payload_bytes.as_slice())?;
    let chain_metadata = compact_block
        .chain_metadata
        .as_ref()
        .ok_or("compact block missing chain metadata")?;
    assert_eq!(chain_metadata.orchard_commitment_tree_size, 42);
    assert_eq!(chain_metadata.sapling_commitment_tree_size, 11);
    assert_eq!(chain_metadata.ironwood_commitment_tree_size, 7);
    assert!(
        compact_block
            .vtx
            .iter()
            .all(|transaction| transaction.actions.is_empty()
                && transaction.ironwood_actions.is_empty()),
        "regtest block 1 carries no Orchard or Ironwood actions"
    );

    Ok(())
}

#[test]
fn compact_block_builder_rejects_source_identity_mismatch() -> Result<(), Box<dyn Error>> {
    let mut mismatched_hash_block = fixture_source_block()?;
    mismatched_hash_block.hash = changed_block_hash(mismatched_hash_block.hash);
    assert_compact_block_mismatch(&mismatched_hash_block, BlockMismatchField::Hash)?;

    let mut mismatched_parent_block = fixture_source_block()?;
    mismatched_parent_block.parent_hash = changed_block_hash(mismatched_parent_block.parent_hash);
    assert_compact_block_mismatch(&mismatched_parent_block, BlockMismatchField::ParentHash)?;

    let mut mismatched_time_block = fixture_source_block()?;
    mismatched_time_block.block_time_seconds = mismatched_time_block
        .block_time_seconds
        .checked_add(1)
        .ok_or("fixture block time cannot be incremented")?;
    assert_compact_block_mismatch(&mismatched_time_block, BlockMismatchField::Time)?;

    Ok(())
}

fn fixture_source_block() -> Result<SourceBlock, Box<dyn Error>> {
    fixture_source_block_from(
        Network::ZcashRegtest,
        include_str!("../fixtures/z3-regtest-block-1.json"),
    )
}

fn fixture_source_block_from(
    network: Network,
    fixture_json: &str,
) -> Result<SourceBlock, Box<dyn Error>> {
    let fixture: Value = serde_json::from_str(fixture_json)?;
    let raw_block_hex = string_field(&fixture, "raw_block_hex")?;
    let raw_block_bytes = hex::decode(raw_block_hex)?;
    let height = u32_field(&fixture, "height")?;
    let source_block =
        SourceBlock::from_raw_block_bytes(network, BlockHeight::new(height), raw_block_bytes)?;

    assert_eq!(
        source_block.hash,
        decode_rpc_block_hash(string_field(&fixture, "hash")?)?
    );
    assert_eq!(
        source_block.parent_hash,
        decode_rpc_block_hash(string_field(&fixture, "previousblockhash")?)?
    );
    assert_eq!(
        source_block.block_time_seconds,
        u32_field(&fixture, "time")?
    );

    Ok(source_block)
}

fn changed_block_hash(hash: BlockHash) -> BlockHash {
    let mut hash_bytes = hash.as_bytes();
    hash_bytes[0] ^= 0xff;
    BlockHash::from_bytes(hash_bytes)
}

fn assert_compact_block_mismatch(
    source_block: &SourceBlock,
    expected_field: BlockMismatchField,
) -> Result<(), Box<dyn Error>> {
    let error = match build_positioned_block_for_test(source_block) {
        Ok(block) => {
            return Err(format!("expected compact block build failure, got {block:?}").into());
        }
        Err(error) => error,
    };

    let CanonicalBlockConstructionError::SourceBlockMismatch { field, .. } = error else {
        return Err(format!("expected source block mismatch, got {error:?}").into());
    };
    if field != expected_field {
        return Err(format!("expected mismatch on {expected_field:?}, got {field:?}").into());
    }
    Ok(())
}

fn string_field<'fixture>(
    fixture: &'fixture Value,
    field_name: &'static str,
) -> Result<&'fixture str, Box<dyn Error>> {
    fixture
        .get(field_name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("fixture field {field_name} must be a string").into())
}

fn u32_field(fixture: &Value, field_name: &'static str) -> Result<u32, Box<dyn Error>> {
    let number = fixture
        .get(field_name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("fixture field {field_name} must be an integer"))?;

    Ok(u32::try_from(number)?)
}
