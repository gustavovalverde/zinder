#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use prost::Message;
use rust_rocksdb::{DB, Options};
use tempfile::TempDir;
use zinder_core::{
    BlockHeight, BlockId, ChainTipMetadata, CommitmentTreeCheckpoint, CommitmentTreeFrontier,
    CommitmentTreeFrontiers, Network, NetworkUpgradeActivations,
    NetworkUpgradeActivationsFingerprintVersion, ShieldedProtocol,
    wire::encode_height_key_ascending,
};
use zinder_ingest::{
    CanonicalBlockLoadOutcome, CanonicalConstructionConfig, CanonicalConstructionError,
    load_fresh_canonical_blocks,
};
use zinder_proto::compat::lightwalletd::CompactBlock;
use zinder_source::{
    NodeCapabilities, NodeSource, SourceBlock, SourceChainSegment, SourceChainSegmentLimits,
    SourceError,
};
use zinder_store::{
    CanonicalStoreBuildPlan, CanonicalStoreError, CanonicalStoreWorkload, RocksDbCanonicalBuilder,
    RocksDbCanonicalStore, RocksDbResourceBudget,
};
use zinder_testkit::sample_regtest_upgrade_activations;

use super::fixture_block::{fixture_ironwood_source_block, fixture_source_block};

#[derive(Clone)]
struct SingleBlockSource {
    block: SourceBlock,
    expected_predecessor: BlockId,
    request_count: Arc<AtomicUsize>,
}

impl SingleBlockSource {
    fn new(block: SourceBlock, expected_predecessor: BlockId) -> Self {
        Self {
            block,
            expected_predecessor,
            request_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl NodeSource for SingleBlockSource {
    fn capabilities(&self) -> NodeCapabilities {
        NodeCapabilities::default()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        if height == self.block.height {
            Ok(self.block.clone())
        } else {
            Err(SourceError::BlockUnavailable {
                height,
                reason: "single-block source has no block at the requested height".to_owned(),
            })
        }
    }

    async fn fetch_chain_segment(
        &self,
        limits: SourceChainSegmentLimits,
    ) -> Result<SourceChainSegment, SourceError> {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        if limits.cursor.block_id() != Some(self.expected_predecessor) {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "canonical construction did not anchor its first request",
            });
        }
        Ok(SourceChainSegment::connected_blocks([self.block.clone()]))
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        Ok(BlockId::new(self.block.height, self.block.hash))
    }
}

#[tokio::test]
async fn wallet_canonical_blocks_retain_transactions_and_position_compact_metadata()
-> Result<(), Box<dyn std::error::Error>> {
    let (temporary, store_path, outcome, expected_tip_metadata) =
        load_ironwood_fixture(CanonicalStoreWorkload::Wallet).await?;
    let CanonicalBlockLoadOutcome { builder, evidence } = outcome;

    assert_eq!(evidence.block_count, 1);
    assert_eq!(evidence.block_header_count, 1);
    assert_eq!(evidence.block_hash_index_count, 1);
    assert_eq!(evidence.block_replay_count, 1);
    assert_eq!(evidence.compact_block_count, 1);
    assert!(evidence.transaction_location_count > 0);
    assert_eq!(
        evidence.transaction_blob_count,
        evidence.transaction_location_count
    );
    assert_eq!(evidence.block_blob_count, 0);
    assert_eq!(evidence.tree_state_checkpoint_count, 2);
    assert_eq!(evidence.block_final_note_commitment_roots_count, 0);
    assert_eq!(evidence.tip_metadata, expected_tip_metadata);
    drop(builder);

    let compact_block = read_compact_block(&store_path, evidence.tip_height)?;
    let compact_metadata = compact_block
        .chain_metadata
        .ok_or("persisted compact block must carry chain metadata")?;
    assert_eq!(compact_metadata.sapling_commitment_tree_size, 0);
    assert_eq!(compact_metadata.orchard_commitment_tree_size, 0);
    assert_eq!(compact_metadata.ironwood_commitment_tree_size, 2);

    let error = RocksDbCanonicalStore::open_ready(
        &store_path,
        &regtest_activations(),
        CanonicalStoreWorkload::Wallet,
        RocksDbResourceBudget::for_local_tests(),
    )
    .err()
    .ok_or("block-local canonical construction must remain BUILDING")?;
    assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));
    assert!(!temporary.path().join("derive").exists());
    Ok(())
}

#[tokio::test]
async fn explorer_canonical_blocks_add_block_blob_retention()
-> Result<(), Box<dyn std::error::Error>> {
    let (_temporary, _store_path, outcome, expected_tip_metadata) =
        load_ironwood_fixture(CanonicalStoreWorkload::Explorer).await?;

    assert_eq!(outcome.evidence.block_count, 1);
    assert_eq!(outcome.evidence.block_blob_count, 1);
    assert_eq!(outcome.evidence.tree_state_checkpoint_count, 2);
    assert_eq!(outcome.evidence.block_final_note_commitment_roots_count, 1);
    assert!(outcome.evidence.transaction_location_count > 0);
    assert_eq!(
        outcome.evidence.transaction_blob_count,
        outcome.evidence.transaction_location_count
    );
    assert_eq!(outcome.evidence.tip_metadata, expected_tip_metadata);
    Ok(())
}

async fn load_ironwood_fixture(
    workload: CanonicalStoreWorkload,
) -> Result<
    (
        TempDir,
        std::path::PathBuf,
        CanonicalBlockLoadOutcome,
        ChainTipMetadata,
    ),
    Box<dyn std::error::Error>,
> {
    let source_block = fixture_ironwood_source_block()?;
    let predecessor_height = source_block
        .height
        .value()
        .checked_sub(1)
        .map(BlockHeight::new)
        .ok_or("fixture must have a predecessor")?;
    let checkpoint = BlockId::new(predecessor_height, source_block.parent_hash);
    let predecessor_frontiers = pre_ironwood_empty_frontiers();
    let expected_tip_metadata = ChainTipMetadata::new(0, 0, 2);
    let fixed_tip = BlockId::new(source_block.height, source_block.hash);
    let activations = regtest_activations();
    let build_plan = CanonicalStoreBuildPlan::checkpointed(
        &activations,
        CommitmentTreeCheckpoint::new(
            checkpoint,
            source_block.block_time_seconds.saturating_sub(1),
            predecessor_frontiers,
        ),
        fixed_tip,
    )?;
    let temporary = TempDir::new()?;
    let store_path = temporary.path().join("canonical");
    let builder = RocksDbCanonicalBuilder::create_fresh(
        &store_path,
        workload,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let source = SingleBlockSource::new(source_block, checkpoint);
    let config = CanonicalConstructionConfig::for_local_tests(Duration::from_secs(5), activations);
    let outcome = load_fresh_canonical_blocks(builder, &source, config).await?;
    Ok((temporary, store_path, outcome, expected_tip_metadata))
}

fn pre_ironwood_empty_frontiers() -> CommitmentTreeFrontiers {
    CommitmentTreeFrontiers::from_validated_parts(
        Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Sapling)),
        Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Orchard)),
        None,
    )
}

fn read_compact_block(
    store_path: &std::path::Path,
    height: BlockHeight,
) -> Result<CompactBlock, Box<dyn std::error::Error>> {
    let column_families = DB::list_cf(&Options::default(), store_path)?;
    let database =
        DB::open_cf_for_read_only(&Options::default(), store_path, &column_families, false)?;
    let compact_blocks = database
        .cf_handle("compact_block")
        .ok_or("fresh store must contain the compact-block column family")?;
    let payload_bytes = database
        .get_cf(&compact_blocks, encode_height_key_ascending(height))?
        .ok_or("fresh store must contain the tip compact block")?;
    Ok(CompactBlock::decode(payload_bytes.as_slice())?)
}

#[tokio::test]
async fn canonical_blocks_reach_fixed_source_tip_without_wallet_state_writes()
-> Result<(), Box<dyn std::error::Error>> {
    let source_block = fixture_source_block()?;
    assert_eq!(source_block.height, BlockHeight::new(1));
    assert_eq!(
        source_block.parent_hash,
        Network::ZcashRegtest.genesis_hash()
    );
    let fixed_tip = BlockId::new(source_block.height, source_block.hash);
    let activations = regtest_activations();
    let build_plan = CanonicalStoreBuildPlan::complete(
        &activations,
        source_block.block_time_seconds.saturating_sub(1),
        fixed_tip,
    )?;
    let history_predecessor = build_plan.history_predecessor().block_id;
    let temporary = TempDir::new()?;
    let store_path = temporary.path().join("canonical");
    let builder = RocksDbCanonicalBuilder::create_fresh(
        &store_path,
        CanonicalStoreWorkload::Wallet,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let source = SingleBlockSource::new(source_block, history_predecessor);
    let config = CanonicalConstructionConfig::for_local_tests(Duration::from_secs(5), activations);

    let outcome = load_fresh_canonical_blocks(builder, &source, config).await?;
    assert_eq!(outcome.builder.build_plan().build_tip(), fixed_tip);
    assert_eq!(outcome.evidence.block_count, 1);
    assert_eq!(outcome.evidence.tip_height, fixed_tip.height);
    assert_eq!(outcome.evidence.tip_hash, fixed_tip.hash);
    assert_eq!(outcome.evidence.block_header_count, 1);
    assert_eq!(outcome.evidence.block_replay_count, 1);
    assert_eq!(outcome.evidence.compact_block_count, 1);
    assert_eq!(outcome.evidence.tree_state_checkpoint_count, 2);
    assert_eq!(outcome.evidence.block_final_note_commitment_roots_count, 0);
    assert_eq!(
        outcome.evidence.transaction_location_count,
        outcome.evidence.transaction_blob_count
    );
    assert!(outcome.evidence.logical_bytes > 0);
    assert!(outcome.evidence.sst_file_bytes > 0);
    drop(outcome.builder);

    let error = RocksDbCanonicalStore::open_ready(
        &store_path,
        &regtest_activations(),
        CanonicalStoreWorkload::Wallet,
        RocksDbResourceBudget::for_local_tests(),
    )
    .err()
    .ok_or("block-family construction must remain BUILDING")?;
    assert!(matches!(error, CanonicalStoreError::StoreNotReady { .. }));

    let column_families =
        rust_rocksdb::DB::list_cf(&rust_rocksdb::Options::default(), &store_path)?;
    for wallet_state_family in [
        "address_output_index",
        "transparent_output",
        "transparent_spend_fact",
        "transaction_facts",
    ] {
        assert!(
            !column_families
                .iter()
                .any(|family| family == wallet_state_family)
        );
    }
    assert!(!temporary.path().join("derive").exists());
    Ok(())
}

#[tokio::test]
async fn canonical_construction_rejects_source_blocks_from_another_network()
-> Result<(), Box<dyn std::error::Error>> {
    let source_block = fixture_source_block()?;
    let fixed_tip = BlockId::new(source_block.height, source_block.hash);
    let activations = regtest_activations();
    let build_plan = CanonicalStoreBuildPlan::complete(
        &activations,
        source_block.block_time_seconds.saturating_sub(1),
        fixed_tip,
    )?;
    let history_predecessor = build_plan.history_predecessor().block_id;
    let temporary = TempDir::new()?;
    let builder = RocksDbCanonicalBuilder::create_fresh(
        temporary.path().join("canonical"),
        CanonicalStoreWorkload::Wallet,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let source = SingleBlockSource::new(
        SourceBlock {
            network: Network::ZcashMainnet,
            ..source_block
        },
        history_predecessor,
    );
    let config = CanonicalConstructionConfig::for_local_tests(Duration::from_secs(5), activations);

    let error = load_fresh_canonical_blocks(builder, &source, config)
        .await
        .err()
        .ok_or("wrong-network source block must be rejected")?;

    assert!(matches!(
        error,
        CanonicalConstructionError::SourceBlockNetworkMismatch {
            height,
            store_network: Network::ZcashRegtest,
            source_network: Network::ZcashMainnet,
        } if height == BlockHeight::new(1)
    ));
    Ok(())
}

#[tokio::test]
async fn canonical_construction_rejects_activation_mismatch_before_source_work()
-> Result<(), Box<dyn std::error::Error>> {
    let source_block = fixture_source_block()?;
    let fixed_tip = BlockId::new(source_block.height, source_block.hash);
    let activations = regtest_activations();
    let mut store_activation_rows = activations.activations().to_vec();
    let latest_activation = store_activation_rows
        .last_mut()
        .ok_or("regtest activation fixture must not be empty")?;
    latest_activation.activation_height = BlockHeight::new(
        latest_activation
            .activation_height
            .value()
            .saturating_add(1),
    );
    let store_activations =
        NetworkUpgradeActivations::new(Network::ZcashRegtest, store_activation_rows)?;
    assert_ne!(&store_activations, activations.as_ref());
    let store_fingerprint =
        store_activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
    let configured_fingerprint =
        activations.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
    let build_plan = CanonicalStoreBuildPlan::complete(
        &store_activations,
        source_block.block_time_seconds.saturating_sub(1),
        fixed_tip,
    )?;
    let history_predecessor = build_plan.history_predecessor().block_id;
    let temporary = TempDir::new()?;
    let builder = RocksDbCanonicalBuilder::create_fresh(
        temporary.path().join("canonical"),
        CanonicalStoreWorkload::Wallet,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let source = SingleBlockSource::new(source_block, history_predecessor);
    let config = CanonicalConstructionConfig::for_local_tests(Duration::from_secs(5), activations);

    let error = load_fresh_canonical_blocks(builder, &source, config)
        .await
        .err()
        .ok_or("activation mismatch must be rejected")?;

    assert!(matches!(
        error,
        CanonicalConstructionError::NetworkUpgradeActivationsMismatch {
            store_fingerprint: persisted,
            configured_fingerprint: configured,
        } if persisted == store_fingerprint && configured == configured_fingerprint
    ));
    assert_eq!(source.request_count(), 0);
    Ok(())
}

fn regtest_activations() -> Arc<NetworkUpgradeActivations> {
    Arc::new(sample_regtest_upgrade_activations())
}
