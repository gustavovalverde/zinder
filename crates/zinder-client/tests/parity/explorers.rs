//! Block-explorer parity assertions.
//!
//! Block explorers exercise typed `WalletQuery` and federated
//! `explorer.*` surfaces to display per-block, per-transaction, and
//! per-address state. These compile-time assertions ensure the trait surface
//! they depend on stays intact through future refactors.

use zinder_client::{
    BlockHeight, ChainIndex, EndpointBackedIndex, RemoteChainIndex, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentUnspentOutput,
};
use zinder_testkit::FixtureTransactionRows;

use super::{open_remote_chain_index, parity_chain_fixture};

#[test]
fn parity_chain_index_surface_compiles_for_block_explorers() {
    fn assert_base_compiles<T: ChainIndex>() {
        // hash-or-height lookup via BlockSelector
        let _ = T::block_id_by_selector;
        // typed block-header read model
        let _ = T::block_header_by_selector;
        // typed TxStatus with mined / mempool / not found
        let _ = T::transaction_by_id;
        // typed TransparentAddressBalance from the canonical unspent index
        let _ = T::transparent_address_balance;
        // M6 canonical prevout resolution. Explorers and SDKs that decode
        // transaction inputs depend on this staying in the base contract;
        // renaming or removing it is a breaking change.
        let _ = T::transparent_outputs_by_outpoint;
    }
    fn assert_endpoint_compiles<T: EndpointBackedIndex>() {
        // per-address mempool overlays
        let _ = T::transparent_mempool_outputs_by_address;
        let _ = T::transparent_mempool_spends_by_outpoint;
        // live-mempool prevout fallback for chained-mempool input decode
        let _ = T::transparent_mempool_outputs_by_outpoint;
    }
    assert_base_compiles::<RemoteChainIndex>();
    assert_endpoint_compiles::<RemoteChainIndex>();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end fixture covering the release-advertised transparent balance projection."
)]
async fn serves_release_advertised_explorer_transparent_balance_from_fixture() -> eyre::Result<()> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes([0xA7; 32]);
    let transaction_id = TransactionId::from_bytes([0xA8; 32]);
    let base_fixture = parity_chain_fixture(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("fixture must contain block 1"))?;
    let block_height = block.height;
    let block_hash = block.hash;
    let transparent_outpoint = TransparentOutPoint::new(transaction_id, 0);
    let utxo = TransparentUnspentOutput::new(
        address_script_hash,
        vec![0x76, 0xA9],
        transparent_outpoint,
        321,
        block_height,
        block_hash,
    );
    let chain_fixture = base_fixture
        .with_transaction_rows(FixtureTransactionRows::from_raw_transaction(
            transaction_id,
            block_height,
            block_hash,
            0,
            b"explorer-transparent-transaction".to_vec(),
        ))
        .with_address_output_index(utxo.clone());
    let chain_index = open_remote_chain_index(&chain_fixture).await?;

    let balance = chain_index
        .transparent_address_balance(&[address_script_hash])
        .await?;
    assert_eq!(balance.confirmed_zat, 321);
    assert_eq!(balance.unconfirmed_delta_zat, 0);
    assert_eq!(balance.address_count, 1);
    assert_eq!(balance.chain_epoch.visible_tip_height, block_height);

    Ok(())
}
