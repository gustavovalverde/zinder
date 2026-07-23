#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use eyre::eyre;
use tokio_stream::StreamExt as _;
use zinder_client::{
    BlockHeight, BlockHeightRange, BlockSelector, ChainEpochId, ChainIndex, ChainSnapshot,
    DEFAULT_INITIAL_CATCHUP_TIMEOUT, IndexerError, LocalChainIndex, LocalOpenOptions, Network,
    OwnedChainSnapshot, RetryPolicy, ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange,
    TransactionId, TransparentAddressScriptHash, TransparentOutPoint, TransparentUnspentOutput,
    TxStatus,
};
use zinder_core::TransparentSpendFact;
use zinder_materialized_views::{
    MaterializedViewCoverage, MaterializedViewState, MaterializedViewStore,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
};
use zinder_store::ReorgWindowChange;
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, StoreFixture,
    open_test_materialized_view_store_for_canonical, sample_regtest_upgrade_activations,
    seed_transparent_outpoint_spends,
};

struct SweptTransparentSpendStores {
    canonical_store_fixture: StoreFixture,
    materialized_view_store: MaterializedViewStore,
    spent_outpoint: TransparentOutPoint,
    spend: TransparentSpendFact,
    materialized_view_state: MaterializedViewState,
}

fn open_swept_transparent_spend_stores() -> eyre::Result<SweptTransparentSpendStores> {
    let base_chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let output_block = base_chain
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let spending_block = base_chain
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("fixture must contain block 2"))?;
    let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x91; 32]), 0);
    let address_script_hash = TransparentAddressScriptHash::from_bytes([0x92; 32]);
    let output = TransparentUnspentOutput::new(
        address_script_hash,
        vec![0x76, 0xA9],
        spent_outpoint,
        42_000,
        output_block.height,
        output_block.hash,
    );
    let spend = TransparentSpendFact::new(
        spent_outpoint,
        0,
        TransactionId::from_bytes([0x93; 32]),
        0,
        spending_block.height,
        spending_block.hash,
        output.value_zat,
        output.address_script_hash,
        output.block_height,
        output.block_hash,
    );
    let chain = base_chain
        .with_address_output_index(output)
        .with_transparent_spend_fact(spend.clone());
    let canonical_store_fixture =
        StoreFixture::with_chain_committed(&chain, zinder_client::ChainEpochId::new(1))?;
    let materialized_view_store =
        open_test_materialized_view_store_for_canonical(canonical_store_fixture.tempdir_path())?;
    seed_transparent_outpoint_spends(&materialized_view_store, std::slice::from_ref(&spend))?;

    let canonical_store = canonical_store_fixture.chain_store();
    canonical_store.set_transparent_retention_release_height(BlockHeight::new(3))?;
    let sweep = canonical_store.sweep_transparent_retention_once()?;
    assert_eq!(sweep.swept_heights(), 3);
    assert_eq!(sweep.swept_outpoints(), 1);
    let reader = canonical_store.current_chain_epoch_reader()?;
    assert_eq!(
        reader.transparent_retention_deleted_through_height()?,
        Some(BlockHeight::new(3))
    );
    assert!(
        reader
            .transparent_spend_facts_by_outpoints(&[spent_outpoint])?
            .is_empty()
    );
    let chain_epoch = reader.chain_epoch();
    let materialized_view_tip = reader
        .block_header_at(chain_epoch.visible_tip_height)?
        .ok_or_else(|| eyre!("fixture visible-tip header must exist"))?;
    let materialized_view_state = MaterializedViewState {
        chain_epoch_id: chain_epoch.id,
        tip_height: chain_epoch.visible_tip_height,
        tip_hash: materialized_view_tip.block_hash,
        revision: 1,
        coverage: Some(MaterializedViewCoverage {
            complete_from_height: reader.canonical_history_bounds().first_available_height(),
            complete_through_height: chain_epoch.visible_tip_height,
            complete_through_hash: materialized_view_tip.block_hash,
        }),
    };
    drop(reader);

    Ok(SweptTransparentSpendStores {
        canonical_store_fixture,
        materialized_view_store,
        spent_outpoint,
        spend,
        materialized_view_state,
    })
}

async fn open_local_secondary_for_canonical(
    canonical_store_fixture: &StoreFixture,
    secondary_directory_name: &str,
) -> Result<LocalChainIndex, IndexerError> {
    LocalChainIndex::open(LocalOpenOptions {
        storage_path: canonical_store_fixture.tempdir_path().to_path_buf(),
        secondary_path: canonical_store_fixture
            .tempdir_path()
            .join(secondary_directory_name),
        network: Network::ZcashRegtest,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        materialized_view_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        initial_catchup_timeout: DEFAULT_INITIAL_CATCHUP_TIMEOUT,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        utxo_set_commitment_enabled: false,
    })
    .await
}

#[tokio::test]
async fn local_snapshot_pins_visible_tip_to_captured_epoch() -> eyre::Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let chain_index = open_local_secondary_for_canonical(
        &store_fixture,
        "zinder-client-secondary-chain-snapshot",
    )
    .await?;

    let snapshot = chain_index.snapshot().await?;
    let visible_tip = snapshot.visible_tip_block().await?;

    assert_eq!(
        snapshot.chain_epoch().id,
        zinder_client::ChainEpochId::new(1)
    );
    assert_eq!(
        visible_tip.height,
        snapshot.chain_epoch().visible_tip_height
    );
    Ok(())
}

#[tokio::test]
async fn borrowed_and_owned_snapshots_support_erased_chain_indexes() -> eyre::Result<()> {
    fn assert_clone_static<T: Clone + Send + Sync + 'static>(_: &T) {}

    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let chain_index = Arc::new(
        open_local_secondary_for_canonical(
            &store_fixture,
            "zinder-client-secondary-snapshot-erased",
        )
        .await?,
    );
    let erased: Arc<dyn ChainIndex> = chain_index;

    let borrowed = ChainSnapshot::capture(erased.as_ref()).await?;
    let (borrowed_copy, borrowed_original) = (borrowed, borrowed);
    assert_eq!(
        borrowed_copy.visible_tip_block().await?,
        borrowed_original.visible_tip_block().await?
    );

    let owned = OwnedChainSnapshot::capture(Arc::clone(&erased)).await?;
    assert_clone_static(&owned);
    let owned_clone = owned.clone();
    assert_eq!(owned.chain_epoch(), owned_clone.chain_epoch());
    assert_eq!(
        owned.visible_tip_block().await?,
        owned_clone.visible_tip_block().await?
    );
    Ok(())
}

#[tokio::test]
async fn local_snapshot_streams_compact_blocks_at_captured_epoch() -> eyre::Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let chain_index = open_local_secondary_for_canonical(
        &store_fixture,
        "zinder-client-secondary-snapshot-stream",
    )
    .await?;
    let snapshot = chain_index.snapshot().await?;

    let mut blocks = snapshot
        .compact_blocks_in_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))
        .await?;
    let mut heights = Vec::new();
    while let Some(block) = blocks.next().await {
        heights.push(block?.height());
    }

    assert_eq!(heights, vec![BlockHeight::new(1), BlockHeight::new(2)]);
    Ok(())
}

#[tokio::test]
async fn local_snapshot_builds_epoch_pinned_transparent_address_query() -> eyre::Result<()> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes([0xC1; 32]);
    let base_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let expected_output = TransparentUnspentOutput::new(
        address_script_hash,
        vec![0x76, 0xA9],
        TransparentOutPoint::new(TransactionId::from_bytes([0xC2; 32]), 0),
        42_000,
        block.height,
        block.hash,
    );
    let chain_fixture = base_fixture.with_address_output_index(expected_output.clone());
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let chain_index = open_local_secondary_for_canonical(
        &store_fixture,
        "zinder-client-secondary-snapshot-address",
    )
    .await?;
    let snapshot = chain_index.snapshot().await?;

    let mut outputs = snapshot
        .transparent_address_unspent_outputs(address_script_hash, BlockHeight::new(0))
        .await?;
    let output = outputs
        .next()
        .await
        .ok_or_else(|| eyre!("fixture output must be present"))??;

    assert_eq!(output.chain_epoch, snapshot.chain_epoch());
    assert_eq!(output.output, expected_output);
    assert!(outputs.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn stale_local_snapshot_fails_closed_after_secondary_observes_reorg() -> eyre::Result<()> {
    let original_chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let replacement_chain = original_chain
        .fork_at(BlockHeight::new(2))?
        .extend_blocks(1);
    let mut initial_epoch = original_chain
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("original chain must contain an epoch"))?;
    initial_epoch.chain_epoch.settled_tip_height = BlockHeight::new(0);
    initial_epoch.chain_epoch.settled_tip_hash = Network::ZcashRegtest.genesis_hash();
    let store_fixture = StoreFixture::open()?;
    store_fixture
        .chain_store()
        .commit_chain_epoch(initial_epoch)?;
    let chain_index = open_local_secondary_for_canonical(
        &store_fixture,
        "zinder-client-secondary-stale-snapshot",
    )
    .await?;
    let snapshot = chain_index.snapshot().await?;
    let original_tip = snapshot.visible_tip_block().await?;

    let mut replacement_epoch = replacement_chain
        .chain_epoch_artifacts(ChainEpochId::new(2))
        .ok_or_else(|| eyre!("replacement chain must contain an epoch"))?;
    replacement_epoch.chain_epoch.settled_tip_height = BlockHeight::new(0);
    replacement_epoch.chain_epoch.settled_tip_hash = Network::ZcashRegtest.genesis_hash();
    replacement_epoch
        .block_headers
        .retain(|header| header.height == BlockHeight::new(2));
    replacement_epoch.block_replay_envelopes = replacement_chain
        .block_replay_envelopes()
        .into_iter()
        .skip(1)
        .collect();
    replacement_epoch
        .compact_blocks
        .retain(|block| block.height() == BlockHeight::new(2));
    replacement_epoch.reorg_window_change = ReorgWindowChange::Replace {
        from_height: BlockHeight::new(2),
    };
    store_fixture
        .chain_store()
        .commit_chain_epoch(replacement_epoch)?;

    let replacement_tip = chain_index.visible_tip_block(None).await?;
    assert_eq!(replacement_tip.height, original_tip.height);
    assert_ne!(replacement_tip.hash, original_tip.hash);

    let stale_read = snapshot
        .block_id_by_selector(BlockSelector::from_height(original_tip.height))
        .await;
    let Err(error) = stale_read else {
        return Err(eyre!(
            "a stale local snapshot crossed into a replacement epoch"
        ));
    };

    assert!(
        matches!(error, IndexerError::ChainEpochPinUnavailable),
        "unexpected stale-pin error: {error:?}"
    );
    assert_eq!(error.retry_policy(), RetryPolicy::RefreshChainEpoch);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the regression keeps snapshot capture, secondary catchup, and one stale transparent-spend read together"
)]
async fn stale_local_snapshot_transparent_spend_read_returns_epoch_refresh() -> eyre::Result<()> {
    let original_chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let replacement_chain = original_chain
        .fork_at(BlockHeight::new(2))?
        .extend_blocks(1);
    let mut initial_epoch = original_chain
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("original chain must contain an epoch"))?;
    initial_epoch.chain_epoch.settled_tip_height = BlockHeight::new(0);
    initial_epoch.chain_epoch.settled_tip_hash = Network::ZcashRegtest.genesis_hash();
    let store_fixture = StoreFixture::open()?;
    store_fixture
        .chain_store()
        .commit_chain_epoch(initial_epoch)?;
    let chain_index = open_local_secondary_for_canonical(
        &store_fixture,
        "zinder-client-secondary-stale-snapshot-spend",
    )
    .await?;
    let snapshot = chain_index.snapshot().await?;

    let mut replacement_epoch = replacement_chain
        .chain_epoch_artifacts(ChainEpochId::new(2))
        .ok_or_else(|| eyre!("replacement chain must contain an epoch"))?;
    replacement_epoch.chain_epoch.settled_tip_height = BlockHeight::new(0);
    replacement_epoch.chain_epoch.settled_tip_hash = Network::ZcashRegtest.genesis_hash();
    replacement_epoch
        .block_headers
        .retain(|header| header.height == BlockHeight::new(2));
    replacement_epoch.block_replay_envelopes = replacement_chain
        .block_replay_envelopes()
        .into_iter()
        .skip(1)
        .collect();
    replacement_epoch
        .compact_blocks
        .retain(|block| block.height() == BlockHeight::new(2));
    replacement_epoch.reorg_window_change = ReorgWindowChange::Replace {
        from_height: BlockHeight::new(2),
    };
    store_fixture
        .chain_store()
        .commit_chain_epoch(replacement_epoch)?;
    chain_index.visible_tip_block(None).await?;

    let stale_read = snapshot
        .transparent_spends_by_outpoint(&[TransparentOutPoint::new(
            TransactionId::from_bytes([0xD1; 32]),
            0,
        )])
        .await;
    let Err(error) = stale_read else {
        return Err(eyre!(
            "a stale transparent-spend read crossed into the replacement epoch"
        ));
    };

    assert!(matches!(error, IndexerError::ChainEpochPinUnavailable));
    assert_eq!(error.retry_policy(), RetryPolicy::RefreshChainEpoch);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "single integration scenario keeps secondary-store reads on one pinned fixture"
)]
async fn local_chain_index_reads_typed_values_from_secondary_store() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x44; 32]);
    let base_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let transaction_block = base_fixture
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("fixture must contain block 2"))?;
    let transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        transaction_block.height,
        transaction_block.hash,
        0,
        b"transaction-payload".to_vec(),
    );
    let transaction_location = transaction_rows.location;
    let chain_fixture = base_fixture.with_transaction_rows(transaction_rows);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let chain_index = LocalChainIndex::open(LocalOpenOptions {
        storage_path: store_fixture.tempdir_path().to_path_buf(),
        secondary_path: store_fixture.tempdir_path().join("zinder-client-secondary"),
        network: Network::ZcashRegtest,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        materialized_view_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        initial_catchup_timeout: DEFAULT_INITIAL_CATCHUP_TIMEOUT,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        utxo_set_commitment_enabled: false,
    })
    .await?;

    let current_epoch = chain_index.current_epoch().await?;
    let visible_tip_block = chain_index.visible_tip_block(None).await?;
    let compact_block = chain_index
        .compact_block_at(BlockHeight::new(1), None)
        .await?;
    let tree_state = chain_index.latest_tree_state_checkpoint(None).await?;
    let subtree_roots = chain_index
        .subtree_roots_in_range(
            SubtreeRootRange::new(
                ShieldedProtocol::Sapling,
                SubtreeRootIndex::new(0),
                NonZeroU32::MIN,
            ),
            None,
        )
        .await?;
    let mined_transaction = chain_index.transaction_by_id(transaction_id, None).await?;
    let missing_transaction = chain_index
        .transaction_by_id(TransactionId::from_bytes([0x55; 32]), None)
        .await?;
    let mut compact_block_stream = chain_index
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await?;
    let mut compact_block_count = 0;
    while let Some(compact_block_result) = compact_block_stream.next().await {
        compact_block_result?;
        compact_block_count += 1;
    }

    assert_eq!(current_epoch.visible_tip_height, BlockHeight::new(2));
    assert_eq!(visible_tip_block.height, BlockHeight::new(2));
    assert_eq!(compact_block.height(), BlockHeight::new(1));
    assert_eq!(tree_state.height, BlockHeight::new(2));
    assert_eq!(subtree_roots.len(), 1);
    let TxStatus::Mined(mined) = mined_transaction else {
        return Err(eyre!(
            "expected mined transaction, got {mined_transaction:?}"
        ));
    };
    assert_eq!(
        mined.location.transaction_id,
        transaction_location.transaction_id
    );
    assert_eq!(
        mined.location.block_height,
        transaction_location.block_height
    );
    assert_eq!(mined.location.block_hash, transaction_location.block_hash);
    assert_eq!(mined.location.tx_index_in_block, 0);
    assert_eq!(missing_transaction, TxStatus::NotFound);
    assert_eq!(compact_block_count, 2);

    Ok(())
}

#[tokio::test]
async fn local_chain_index_resolves_swept_spend_from_materialized_view() -> eyre::Result<()> {
    let swept_stores = open_swept_transparent_spend_stores()?;
    swept_stores.materialized_view_store.put_consumer_state(
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
        swept_stores.materialized_view_state,
    )?;
    let chain_index = open_local_secondary_for_canonical(
        &swept_stores.canonical_store_fixture,
        "zinder-client-secondary-swept-spend",
    )
    .await?;

    let response = chain_index
        .transparent_spends_by_outpoint(&[swept_stores.spent_outpoint], None)
        .await?;

    assert_eq!(response.spends.len(), 1);
    assert_eq!(
        response.spends[0].spending_transaction_id,
        swept_stores.spend.spending_transaction_id
    );
    assert_eq!(
        response.spends[0].spending_block_height,
        swept_stores.spend.block_height
    );
    Ok(())
}

#[tokio::test]
async fn local_chain_index_refuses_swept_miss_without_projection_coverage() -> eyre::Result<()> {
    let swept_stores = open_swept_transparent_spend_stores()?;
    let chain_index = open_local_secondary_for_canonical(
        &swept_stores.canonical_store_fixture,
        "zinder-client-secondary-missing-spend-coverage",
    )
    .await?;

    let outcome = chain_index
        .transparent_spends_by_outpoint(&[swept_stores.spent_outpoint], None)
        .await;

    assert!(matches!(
        outcome,
        Err(IndexerError::FailedPrecondition { reason })
            if reason.contains("does not cover every canonical deletion")
    ));
    Ok(())
}
