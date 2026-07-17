//! Consumer-release certification tests for the public client contracts.
//!
//! Each per-consumer module asserts the typed shape that consumer's contract
//! depends on. Parity here means "Zinder serves the consumer-expected shape",
//! not byte-equivalence with every implementation detail of another indexer.

use std::{sync::Arc, time::Duration};

use zebra_chain::{
    parameters::NetworkKind as ZebraNetworkKind, transparent::Address as ZebraTransparentAddress,
};
use zinder_client::{
    BlockHeight, ChainEpochId, DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex, LocalOpenOptions,
    Network, TransactionId, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentOutPoint, TransparentUnspentOutput,
};
use zinder_core::{ChainTipMetadata, SUBTREE_LEAF_COUNT, wire::encode_height_key_ascending};
use zinder_derive::{
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
};
use zinder_store::{ChainEventStreamFamily, EventStreamStartPosition, RawBlobRetention};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, StoreFixture, open_test_derive_store_for_canonical,
    sample_regtest_upgrade_activations, seed_transparent_address_transaction_history,
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

struct TransparentAddressHistoryFixture {
    store_fixture: StoreFixture,
    derive_store: zinder_derive::DeriveStore,
    address: String,
    script_pub_key: Vec<u8>,
    transaction_id: TransactionId,
    raw_transaction_bytes: Vec<u8>,
    value_zat: i64,
    block_height: BlockHeight,
}

fn transparent_address_history_fixture() -> eyre::Result<TransparentAddressHistoryFixture> {
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x11; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    let transaction_id = TransactionId::from_bytes([0x55; 32]);
    let base_fixture =
        parity_chain_fixture(1).with_raw_blob_retention(RawBlobRetention::Transactions);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("fixture must contain block 1"))?;
    let block_height = block.height;
    let block_hash = block.hash;
    let raw_transaction_bytes = b"transparent-address-history-payload".to_vec();
    let value_zat = 123_i64;
    let transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        block_height,
        block_hash,
        0,
        raw_transaction_bytes.clone(),
    );
    let tx_history = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        block_height,
        0,
        transaction_id,
        block_hash,
    );
    let chain_fixture = base_fixture
        .with_transaction_rows(transaction_rows)
        .with_address_output_index(TransparentUnspentOutput::new(
            address_script_hash,
            script_pub_key.clone(),
            TransparentOutPoint::new(transaction_id, 0),
            u64::try_from(value_zat)?,
            block_height,
            block_hash,
        ));
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    seed_transparent_address_transaction_history(&derive_store, std::slice::from_ref(&tx_history))?;
    derive_store.put_consumer(
        TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
        &encode_height_key_ascending(block_height),
        &[],
    )?;
    let projection_cursor = store_fixture
        .chain_store()
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Tip,
        )?
        .cursor
        .ok_or_else(|| eyre::eyre!("committed fixture must expose a live-tail cursor"))?;
    for projection in [
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
    ] {
        derive_store.put_chain_event_cursor(projection, projection_cursor.as_bytes())?;
    }
    Ok(TransparentAddressHistoryFixture {
        store_fixture,
        derive_store,
        address,
        script_pub_key,
        transaction_id,
        raw_transaction_bytes,
        value_zat,
        block_height,
    })
}

async fn open_local_chain_index(store_fixture: &StoreFixture) -> eyre::Result<LocalChainIndex> {
    Ok(LocalChainIndex::open(LocalOpenOptions {
        storage_path: store_fixture.tempdir_path().to_path_buf(),
        secondary_path: store_fixture.tempdir_path().join("parity-secondary"),
        network: Network::ZcashRegtest,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        derive_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        initial_catchup_timeout: DEFAULT_INITIAL_CATCHUP_TIMEOUT,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        utxo_set_commitment_enabled: false,
    })
    .await?)
}
