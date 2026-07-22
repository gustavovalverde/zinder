//! Consumer-shaped contract tests for the public client surface.
//!
//! Each per-consumer module asserts the typed shape that consumer's contract
//! depends on. Parity here means "Zinder serves the consumer-expected shape",
//! not byte-equivalence with every implementation detail of another indexer.

use std::{sync::Arc, time::Duration};

use zinder_client::{
    BlockHeight, ChainEpochId, DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex, LocalOpenOptions,
    Network,
};
use zinder_testkit::{ChainFixture, StoreFixture, sample_regtest_upgrade_activations};

const PARITY_TREE_STATE_PAYLOAD: &[u8] =
    br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":{"finalState":"000000"}},"orchard":{"commitments":{"finalState":"111111"}}}"#;

mod explorers;
mod zallet;

fn parity_chain_fixture(block_count: u32) -> ChainFixture {
    ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(block_count)
        .with_tree_state_checkpoint_payload_at(
            BlockHeight::new(block_count),
            PARITY_TREE_STATE_PAYLOAD,
        )
}

fn committed_store_fixture(chain_fixture: &ChainFixture) -> eyre::Result<StoreFixture> {
    Ok(StoreFixture::with_chain_committed(
        chain_fixture,
        ChainEpochId::new(1),
    )?)
}

async fn open_local_chain_index(store_fixture: &StoreFixture) -> eyre::Result<LocalChainIndex> {
    Ok(LocalChainIndex::open(LocalOpenOptions {
        storage_path: store_fixture.tempdir_path().to_path_buf(),
        secondary_path: store_fixture.tempdir_path().join("parity-secondary"),
        network: Network::ZcashRegtest,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        materialized_view_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        initial_catchup_timeout: DEFAULT_INITIAL_CATCHUP_TIMEOUT,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        utxo_set_commitment_enabled: false,
    })
    .await?)
}
