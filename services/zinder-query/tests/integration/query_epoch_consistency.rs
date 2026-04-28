#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use eyre::eyre;
use parking_lot::Mutex;
use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpochId, ShieldedProtocol, SubtreeRootIndex,
    SubtreeRootRange, TreeStateArtifact,
};
use zinder_query::{
    WalletQuery, WalletQueryApi, latest_block_response, latest_tree_state_response,
    subtree_roots_response, tree_state_response,
};
use zinder_store::{
    ChainEpochArtifacts, ChainEpochReadApi, ChainEpochReader, ChainEventEnvelope,
    ChainEventHistoryRequest, PrimaryChainStore, StoreError,
};
use zinder_testkit::StoreFixture;

use crate::common::{compact_block_with_tree_sizes, synthetic_chain_epoch};

#[tokio::test]
async fn compact_block_range_stays_bound_to_reader_epoch_if_current_epoch_advances()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block.clone()],
    ))?;

    let read_api = CommitAfterReaderReadApi::new(
        store,
        Some(ChainEpochArtifacts::new(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        )),
    );
    let wallet_query = WalletQuery::new(read_api, ());

    let compact_block_range = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(1),
        ))
        .await?;

    assert_eq!(compact_block_range.chain_epoch, first_epoch);
    assert_eq!(
        compact_block_range.compact_blocks,
        vec![first_compact_block]
    );

    Ok(())
}

#[tokio::test]
async fn latest_block_response_stays_bound_to_reader_epoch_if_current_epoch_advances()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let read_api = CommitAfterReaderReadApi::new(
        store,
        Some(ChainEpochArtifacts::new(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        )),
    );
    let wallet_query = WalletQuery::new(read_api, ());

    let response = latest_block_response(&wallet_query, None).await?;
    let response_chain_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("missing response chain epoch"))?;
    let latest_block = response
        .latest_block
        .ok_or_else(|| eyre!("missing latest block metadata"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, first_epoch.id.value());
    assert_eq!(latest_block.height, first_epoch.tip_height.value());
    assert_eq!(latest_block.block_hash, first_epoch.tip_hash.as_bytes());

    Ok(())
}

#[tokio::test]
async fn tree_state_response_stays_bound_to_reader_epoch_if_current_epoch_advances()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let first_tree_state = TreeStateArtifact::new(
        first_block.height,
        first_block.block_hash,
        b"tree-state-1".to_vec(),
    );
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(first_epoch, vec![first_block], vec![first_compact_block])
            .with_tree_states(vec![first_tree_state.clone()]),
    )?;

    let read_api = CommitAfterReaderReadApi::new(
        store,
        Some(ChainEpochArtifacts::new(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        )),
    );
    let wallet_query = WalletQuery::new(read_api, ());

    let response = tree_state_response(&wallet_query, first_tree_state.height, None).await?;
    let response_chain_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("missing response chain epoch"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, first_epoch.id.value());
    assert_eq!(response.height, first_tree_state.height.value());
    assert_eq!(response.block_hash, first_tree_state.block_hash.as_bytes());
    assert_eq!(response.payload_bytes, first_tree_state.payload_bytes);

    Ok(())
}

#[tokio::test]
async fn latest_tree_state_response_stays_bound_to_reader_epoch_if_current_epoch_advances()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let first_tree_state = TreeStateArtifact::new(
        first_block.height,
        first_block.block_hash,
        b"tree-state-1".to_vec(),
    );
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(first_epoch, vec![first_block], vec![first_compact_block])
            .with_tree_states(vec![first_tree_state.clone()]),
    )?;

    let read_api = CommitAfterReaderReadApi::new(
        store,
        Some(ChainEpochArtifacts::new(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        )),
    );
    let wallet_query = WalletQuery::new(read_api, ());

    let response = latest_tree_state_response(&wallet_query, None).await?;
    let response_chain_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("missing response chain epoch"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, first_epoch.id.value());
    assert_eq!(response.height, first_tree_state.height.value());
    assert_eq!(response.block_hash, first_tree_state.block_hash.as_bytes());
    assert_eq!(response.payload_bytes, first_tree_state.payload_bytes);

    Ok(())
}

#[tokio::test]
async fn subtree_roots_response_stays_bound_to_reader_epoch_if_current_epoch_advances()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, _first_compact_block) = synthetic_chain_epoch(1, 1);
    let first_compact_block =
        compact_block_with_tree_sizes(first_block.height, first_block.block_hash, 0, 0);
    let (second_epoch, second_block, _second_compact_block) = synthetic_chain_epoch(2, 2);
    let second_compact_block =
        compact_block_with_tree_sizes(second_block.height, second_block.block_hash, 65_536, 0);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let read_api = CommitAfterReaderReadApi::new(
        store,
        Some(ChainEpochArtifacts::new(
            second_epoch,
            vec![second_block],
            vec![second_compact_block],
        )),
    );
    let wallet_query = WalletQuery::new(read_api, ());

    let response = subtree_roots_response(
        &wallet_query,
        SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            std::num::NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        ),
        None,
    )
    .await?;
    let response_chain_epoch = response
        .chain_epoch
        .ok_or_else(|| eyre!("missing response chain epoch"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, first_epoch.id.value());
    assert!(response.subtree_roots.is_empty());

    Ok(())
}

#[derive(Clone)]
struct CommitAfterReaderReadApi {
    store: PrimaryChainStore,
    pending_commit: Arc<Mutex<Option<ChainEpochArtifacts>>>,
}

impl CommitAfterReaderReadApi {
    fn new(store: PrimaryChainStore, pending_commit: Option<ChainEpochArtifacts>) -> Self {
        Self {
            store,
            pending_commit: Arc::new(Mutex::new(pending_commit)),
        }
    }
}

impl ChainEpochReadApi for CommitAfterReaderReadApi {
    fn current_chain_epoch_reader(&self) -> Result<ChainEpochReader<'_>, StoreError> {
        let reader = self.store.current_chain_epoch_reader()?;
        let pending_artifacts = self.pending_commit.lock().take();
        if let Some(artifacts) = pending_artifacts {
            self.store.commit_chain_epoch(artifacts)?;
        }

        Ok(reader)
    }

    fn chain_epoch_reader_at(
        &self,
        chain_epoch: ChainEpochId,
    ) -> Result<ChainEpochReader<'_>, StoreError> {
        self.store.chain_epoch_reader_at(chain_epoch)
    }

    fn chain_event_history(
        &self,
        request: ChainEventHistoryRequest<'_>,
    ) -> Result<Vec<ChainEventEnvelope>, StoreError> {
        self.store.chain_event_history(request)
    }
}
