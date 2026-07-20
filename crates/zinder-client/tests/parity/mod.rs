//! Consumer-release certification tests for the public client contracts.
//!
//! Each per-consumer module asserts the typed shape that consumer's contract
//! depends on. Parity here means "Zinder serves the consumer-expected shape",
//! not byte-equivalence with every implementation detail of another indexer.

use std::{sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use zebra_chain::{
    parameters::NetworkKind as ZebraNetworkKind, transparent::Address as ZebraTransparentAddress,
};
use zinder_client::{
    BlockHeight, ChainEpochId, DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex, LocalOpenOptions,
    Network, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentUnspentOutput,
};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::{ChainTipMetadata, SUBTREE_LEAF_COUNT};
use zinder_query::{LightwalletdServingQuery, WalletServingReadPair};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, MockTransactionBroadcaster, StoreFixture,
    WalletServingStoreFixture, sample_regtest_upgrade_activations,
};

const PARITY_TREE_STATE_PAYLOAD: &[u8] =
    br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":{"finalState":"000000"}},"orchard":{"commitments":{"finalState":"111111"}}}"#;

mod explorers;
mod lightwalletd_operators;
mod zallet;
mod zodl;

fn parity_chain_fixture(block_count: u32) -> ChainFixture {
    ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(block_count)
        .with_tip_metadata_override(ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0, 0))
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

struct TransparentAddressServingFixture {
    store_fixture: WalletServingStoreFixture,
    address: String,
    script_pub_key: Vec<u8>,
    transaction_id: TransactionId,
    raw_transaction_bytes: Vec<u8>,
    value_zat: i64,
    block_height: BlockHeight,
}

fn build_transparent_address_serving_fixture() -> eyre::Result<TransparentAddressServingFixture> {
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x11; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    let transaction_id = TransactionId::from_bytes([0x55; 32]);
    let raw_transaction_bytes = b"transparent-address-history-payload".to_vec();
    let value_zat = 123_i64;
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let block = chain
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("transparent fixture must contain block 1"))?
        .clone();
    let block_height = block.height;
    let chain = chain
        .with_transaction_rows(FixtureTransactionRows::from_raw_transaction(
            transaction_id,
            block.height,
            block.hash,
            0,
            raw_transaction_bytes.clone(),
        ))
        .with_address_output_index(TransparentUnspentOutput::new(
            address_script_hash,
            script_pub_key.clone(),
            TransparentOutPoint::new(transaction_id, 0),
            u64::try_from(value_zat)?,
            block.height,
            block.hash,
        ));
    let activations = sample_regtest_upgrade_activations();
    let store_fixture = WalletServingStoreFixture::from_chain(&chain, &activations)?;
    Ok(TransparentAddressServingFixture {
        store_fixture,
        address,
        script_pub_key,
        transaction_id,
        raw_transaction_bytes,
        value_zat,
        block_height,
    })
}

fn build_transparent_address_adapter(
    fixture: &mut TransparentAddressServingFixture,
) -> eyre::Result<LightwalletdGrpcAdapter<LightwalletdServingQuery<MockTransactionBroadcaster>>> {
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let (canonical_reader, wallet_reader) = fixture.store_fixture.take_readers()?;
    let serving_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(canonical_reader),
        Arc::new(wallet_reader),
    )?);
    let serving_pair_slot = Arc::new(ArcSwap::from(serving_pair));
    let query = LightwalletdServingQuery::from_serving_pair_slot(
        serving_pair_slot.clone(),
        MockTransactionBroadcaster::broadcast_disabled(),
        activations.clone(),
    );
    Ok(LightwalletdGrpcAdapter::new(query, activations)
        .with_serving_pair_slot(serving_pair_slot)
        .with_transparent_address_support())
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
