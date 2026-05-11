//! Block-explorer parity assertions.
//!
//! Block explorers exercise typed `WalletQuery` and federated
//! `derive.explorer.*` surfaces to display per-block, per-transaction, and
//! per-address state. These compile-time assertions ensure the trait surface
//! they depend on stays intact through future refactors.

use std::num::NonZeroU32;

use tokio_stream::StreamExt as _;
use zinder_client::{
    BlockHeight, ChainIndex, LocalChainIndex, RemoteChainIndex, TransactionId,
    TransparentAddressScriptHash, TransparentAddressTxIdsQuery, TransparentAddressTxIndexArtifact,
    TransparentAddressUtxoArtifact, TransparentAddressUtxosQuery, TransparentOutPoint,
};

use super::{committed_store_fixture, open_local_chain_index, parity_chain_fixture};

#[test]
fn parity_chain_index_surface_compiles_for_block_explorers() {
    fn assert_compiles<T: ChainIndex>() {
        // hash-or-height lookup via BlockSelector
        let _ = T::block_id_by_selector;
        // typed block-header read model
        let _ = T::block_header_by_selector;
        // typed TxStatus with mined / mempool / conflicting
        let _ = T::transaction_by_id;
        // per-address mempool overlays
        let _ = T::transparent_mempool_outputs_by_address;
        let _ = T::transparent_mempool_spend_by_outpoint;
        // typed TransparentAddressBalance via federated derive
        let _ = T::transparent_address_balance;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}

#[tokio::test]
async fn serves_explorer_transparent_indexes_from_fixture() -> eyre::Result<()> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes([0xA7; 32]);
    let transaction_id = TransactionId::from_bytes([0xA8; 32]);
    let base_fixture = parity_chain_fixture(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("fixture must contain block 1"))?;
    let block_height = block.height;
    let block_hash = block.hash;
    let transparent_outpoint = TransparentOutPoint::new(transaction_id, 0);
    let utxo = TransparentAddressUtxoArtifact::new(
        address_script_hash,
        vec![0x76, 0xA9],
        transparent_outpoint,
        321,
        block_height,
        block_hash,
    );
    let tx_history = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        block_height,
        0,
        transaction_id,
        block_hash,
    );
    let chain_fixture = base_fixture
        .with_transparent_address_utxo(utxo.clone())
        .with_transparent_address_tx_index(tx_history);
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let utxos = chain_index
        .transparent_address_utxos(
            TransparentAddressUtxosQuery {
                address_script_hash,
                start_height: BlockHeight::new(1),
                max_entries: Some(
                    NonZeroU32::new(10)
                        .ok_or_else(|| eyre::eyre!("test max_entries constant must be non-zero"))?,
                ),
                from_cursor: None,
            },
            None,
        )
        .await?;
    let mut history = chain_index
        .transparent_address_tx_ids_in_range(
            TransparentAddressTxIdsQuery {
                address_script_hash,
                start_height: BlockHeight::new(1),
                end_height: BlockHeight::new(1),
                max_entries: Some(
                    NonZeroU32::new(10)
                        .ok_or_else(|| eyre::eyre!("test max_entries constant must be non-zero"))?,
                ),
                from_cursor: None,
                descending: false,
            },
            Some(utxos.chain_epoch),
        )
        .await?;
    let history_item = history
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing transparent history item"))??;

    assert_eq!(utxos.utxos, vec![utxo]);
    assert!(utxos.next_cursor.is_none());
    assert_eq!(history_item.chain_epoch, utxos.chain_epoch);
    assert_eq!(history_item.artifact, tx_history);
    assert!(history_item.cursor.is_none());
    assert!(history.next().await.is_none());
    assert!(matches!(
        chain_index
            .transparent_address_balance(&[address_script_hash], Some(utxos.chain_epoch))
            .await,
        Err(zinder_client::IndexerError::RemoteEndpointUnconfigured {
            operation: "transparent_address_balance"
        })
    ));

    Ok(())
}
