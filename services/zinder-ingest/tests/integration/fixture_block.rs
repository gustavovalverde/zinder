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
    ArtifactSchemaVersion, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    Network, UnixTimestampMillis,
};
use zinder_ingest::{
    ArtifactDeriveError, BlockMismatchField, derive_block_artifact, derive_compact_block_artifact,
    derive_transaction_artifacts,
};
use zinder_proto::compat::lightwalletd::CompactBlock;
use zinder_source::{SourceBlock, decode_display_block_hash};
use zinder_store::ChainEpochArtifacts;
use zinder_testkit::StoreFixture;

#[test]
fn fixture_block_builds_durable_artifacts() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block()?;
    let block_artifact = derive_block_artifact(&source_block)?;
    let compact_block_artifact = derive_compact_block_artifact(&source_block)?;

    assert_eq!(block_artifact.height, BlockHeight::new(1));
    assert_eq!(block_artifact.block_hash, source_block.hash);
    assert_eq!(block_artifact.parent_hash, source_block.parent_hash);
    assert_eq!(block_artifact.payload_bytes, source_block.raw_block_bytes);

    let compact_block = CompactBlock::decode(compact_block_artifact.payload_bytes.as_slice())?;
    assert_eq!(compact_block.proto_version, 1);
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
        tip_height: source_block.height,
        tip_hash: source_block.hash,
        finalized_height: source_block.height,
        finalized_hash: source_block.hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_669_000_000),
    };

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block_artifact.clone()],
        vec![compact_block_artifact.clone()],
    ))?;

    let reader = store.current_chain_epoch_reader()?;
    assert_eq!(reader.block_at(source_block.height)?, Some(block_artifact));
    assert_eq!(
        reader.compact_block_at(source_block.height)?,
        Some(compact_block_artifact)
    );

    Ok(())
}

#[test]
fn fixture_block_transaction_artifacts_round_trip_through_store() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block()?;
    let transactions = derive_transaction_artifacts(&source_block)?;
    assert_eq!(
        transactions.len(),
        1,
        "regtest fixture block 1 has a single coinbase transaction"
    );
    let coinbase = transactions
        .first()
        .ok_or("transaction artifacts vector is empty")?;
    assert_eq!(coinbase.block_height, source_block.height);
    assert_eq!(coinbase.block_hash, source_block.hash);
    assert!(
        !coinbase.payload_bytes.is_empty(),
        "coinbase serialized payload bytes must be present"
    );

    let block_artifact = derive_block_artifact(&source_block)?;
    let compact_block_artifact = derive_compact_block_artifact(&source_block)?;
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        tip_height: source_block.height,
        tip_hash: source_block.hash,
        finalized_height: source_block.height,
        finalized_hash: source_block.hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_669_000_000),
    };

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            chain_epoch,
            vec![block_artifact],
            vec![compact_block_artifact],
        )
        .with_transactions(transactions.clone()),
    )?;

    let reader = store.current_chain_epoch_reader()?;
    let stored = reader
        .transaction_by_id(coinbase.transaction_id)?
        .ok_or("coinbase transaction should be readable after commit")?;
    assert_eq!(&stored, coinbase);

    Ok(())
}

#[test]
fn testnet_sapling_block_compact_artifact_carries_sapling_outputs() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block_from(
        Network::ZcashTestnet,
        include_str!("../fixtures/zcash-testnet-sapling-block-1842432.json"),
    )?;
    let compact_block_artifact = derive_compact_block_artifact(&source_block)?;
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
    let compact_block_artifact = derive_compact_block_artifact(&source_block)?;
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
fn compact_block_builder_rejects_observed_tree_size_mismatch() -> Result<(), Box<dyn Error>> {
    let source_block = fixture_source_block()?.with_tree_state_payload_bytes(
        br#"{"sapling":{"commitments":{"size":1}},"orchard":{"commitments":{"size":0}}}"#.to_vec(),
    );

    let error = match derive_compact_block_artifact(&source_block) {
        Ok(compact_block_artifact) => {
            return Err(
                format!("expected tree-size mismatch, got {compact_block_artifact:?}").into(),
            );
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ArtifactDeriveError::ObservedTreeSizeMismatch { .. }
    ));

    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "Inline NodeSource stub plus end-to-end backfill assertion read more clearly together."
)]
#[tokio::test]
async fn backfill_rejects_wrong_checkpoint_tree_metadata() -> Result<(), Box<dyn Error>> {
    use std::{num::NonZeroU32, sync::Arc};

    use async_trait::async_trait;
    use tempfile::tempdir;
    use zinder_core::{BlockId, ChainTipMetadata, ShieldedProtocol, SubtreeRootIndex};
    use zinder_ingest::{BackfillConfig, IngestError, NodeSourceKind, backfill};
    use zinder_source::{
        DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeCapabilities, NodeSource, NodeTarget,
        SourceChainCheckpoint, SourceError, SourceSubtreeRoots,
    };

    // The tree-size defense fires when the node's z_gettreestate payload
    // exposes an explicit `commitments.size`. Use the regtest coinbase-only
    // fixture (where the real Sapling tree size after height 1 is 0) and lie
    // to the bootstrap by claiming the checkpoint already had 99 Sapling
    // commitments. The first backfilled block (height 1) builds against the
    // wrong initial metadata, observes `size = 0`, and surfaces
    // `ArtifactDeriveError::ObservedTreeSizeMismatch`.
    struct StubFixtureSource {
        block: SourceBlock,
        tip_height: BlockHeight,
    }

    #[async_trait]
    impl NodeSource for StubFixtureSource {
        fn capabilities(&self) -> NodeCapabilities {
            NodeCapabilities::default()
        }

        async fn fetch_block_by_height(
            &self,
            _height: BlockHeight,
        ) -> Result<SourceBlock, SourceError> {
            Ok(self.block.clone())
        }

        async fn tip_id(&self) -> Result<BlockId, SourceError> {
            Ok(BlockId::new(self.tip_height, self.block.hash))
        }

        async fn fetch_subtree_roots(
            &self,
            protocol: ShieldedProtocol,
            start_index: SubtreeRootIndex,
            _max_entries: NonZeroU32,
        ) -> Result<SourceSubtreeRoots, SourceError> {
            Ok(SourceSubtreeRoots::new(protocol, start_index, Vec::new()))
        }
    }

    let fixture_block = fixture_source_block()?.with_tree_state_payload_bytes(
        br#"{"sapling":{"commitments":{"size":0}},"orchard":{"commitments":{"size":0}}}"#.to_vec(),
    );
    let stub_source = Arc::new(StubFixtureSource {
        block: fixture_block,
        tip_height: BlockHeight::new(200),
    });

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("wrong-checkpoint-store");
    let bogus_checkpoint_height = BlockHeight::new(0);
    let backfill_config = BackfillConfig {
        node: NodeTarget::new(
            Network::ZcashRegtest,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            std::time::Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_path,
        from_height: BlockHeight::new(1),
        to_height: BlockHeight::new(1),
        commit_batch_blocks: NonZeroU32::new(1).ok_or("invalid batch size")?,
        allow_near_tip_finalize: true,
        checkpoint: Some(SourceChainCheckpoint::new(
            bogus_checkpoint_height,
            BlockHash::from_bytes([0; 32]),
            ChainTipMetadata::new(99, 0),
        )),
    };

    let error = match backfill(&backfill_config, stub_source.as_ref()).await {
        Ok(outcome) => return Err(format!("expected tree-size mismatch, got {outcome:?}").into()),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            IngestError::ArtifactDerive(ArtifactDeriveError::ObservedTreeSizeMismatch {
                expected_sapling: 99,
                observed_sapling: 0,
                ..
            })
        ),
        "expected ObservedTreeSizeMismatch with seeded sapling=99 vs observed=0; got {error:?}"
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
        decode_display_block_hash(string_field(&fixture, "hash")?)?
    );
    assert_eq!(
        source_block.parent_hash,
        decode_display_block_hash(string_field(&fixture, "previousblockhash")?)?
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
    let error = match derive_compact_block_artifact(source_block) {
        Ok(compact_block_artifact) => {
            return Err(format!(
                "expected compact block build failure, got {compact_block_artifact:?}"
            )
            .into());
        }
        Err(error) => error,
    };

    let ArtifactDeriveError::SourceBlockMismatch { field, .. } = error else {
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
