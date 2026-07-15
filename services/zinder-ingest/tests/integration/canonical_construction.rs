#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tempfile::TempDir;
use zinder_core::{BlockHeight, BlockId, Network};
use zinder_ingest::{
    CanonicalConstructionConfig, CanonicalConstructionError, load_fresh_canonical_block_replay,
};
use zinder_source::{
    NodeCapabilities, NodeSource, SourceBlock, SourceChainSegment, SourceChainSegmentLimits,
    SourceError,
};
use zinder_store::{
    CanonicalStoreBuildPlan, CanonicalStoreError, CanonicalStoreWorkload, RocksDbCanonicalBuilder,
    RocksDbCanonicalStore, RocksDbResourceBudget,
};
use zinder_testkit::sample_regtest_upgrade_activations;

use super::fixture_block::fixture_source_block;

#[derive(Clone)]
struct SingleBlockSource {
    block: SourceBlock,
    expected_predecessor: BlockId,
}

#[async_trait]
impl NodeSource for SingleBlockSource {
    fn capabilities(&self) -> NodeCapabilities {
        NodeCapabilities::default()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
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
        if limits.cursor.block_id() != Some(self.expected_predecessor) {
            return Err(SourceError::SourceProtocolMismatch {
                reason: "canonical construction did not anchor its first request",
            });
        }
        Ok(SourceChainSegment::connected_blocks([self.block.clone()]))
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        Ok(BlockId::new(self.block.height, self.block.hash))
    }
}

#[tokio::test]
async fn canonical_replay_reaches_fixed_source_tip_without_wallet_state_writes()
-> Result<(), Box<dyn std::error::Error>> {
    let source_block = fixture_source_block()?;
    assert_eq!(source_block.height, BlockHeight::new(1));
    assert_eq!(
        source_block.parent_hash,
        Network::ZcashRegtest.genesis_hash()
    );
    let fixed_tip = BlockId::new(source_block.height, source_block.hash);
    let build_plan = CanonicalStoreBuildPlan::complete(Network::ZcashRegtest, fixed_tip)?;
    let temporary = TempDir::new()?;
    let store_path = temporary.path().join("canonical");
    let builder = RocksDbCanonicalBuilder::create_fresh(
        &store_path,
        CanonicalStoreWorkload::Wallet,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let source = SingleBlockSource {
        block: source_block,
        expected_predecessor: build_plan.history_predecessor(),
    };
    let config = CanonicalConstructionConfig::for_local_tests(
        Duration::from_secs(5),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let builder = load_fresh_canonical_block_replay(builder, &source, config).await?;
    assert_eq!(builder.build_plan().build_tip(), fixed_tip);
    drop(builder);

    let error = RocksDbCanonicalStore::open_ready(
        &store_path,
        Network::ZcashRegtest,
        CanonicalStoreWorkload::Wallet,
        RocksDbResourceBudget::for_local_tests(),
    )
    .err()
    .ok_or("replay-only construction must remain BUILDING")?;
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
    let build_plan = CanonicalStoreBuildPlan::complete(Network::ZcashRegtest, fixed_tip)?;
    let temporary = TempDir::new()?;
    let builder = RocksDbCanonicalBuilder::create_fresh(
        temporary.path().join("canonical"),
        CanonicalStoreWorkload::Wallet,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let source = SingleBlockSource {
        block: SourceBlock {
            network: Network::ZcashMainnet,
            ..source_block
        },
        expected_predecessor: build_plan.history_predecessor(),
    };
    let config = CanonicalConstructionConfig::for_local_tests(
        Duration::from_secs(5),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let error = load_fresh_canonical_block_replay(builder, &source, config)
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
