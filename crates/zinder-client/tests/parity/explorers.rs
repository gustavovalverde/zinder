//! Block-explorer parity assertions.
//!
//! Block explorers exercise typed `WalletQuery` and federated
//! `explorer.*` surfaces to display per-block, per-transaction, and
//! per-address state. These compile-time assertions ensure the trait surface
//! they depend on stays intact through future refactors.

use eyre::eyre;
use std::num::NonZeroU32;
use tokio_stream::StreamExt as _;

use zinder_client::{
    BlockHeight, ChainIndex, EndpointBackedIndex, LocalChainIndex, RemoteChainIndex, TransactionId,
    TransparentAddressScriptHash, TransparentAddressTxIdsQuery, TransparentAddressTxIndexArtifact,
    TransparentAddressUnspentOutputsQuery, TransparentOutPoint, TransparentUnspentOutput,
};
use zinder_materialized_views::TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME;
use zinder_store::{ChainEventStreamFamily, EventStreamStartPosition};
use zinder_testkit::{
    open_test_materialized_view_store_for_canonical, seed_transparent_address_transaction_history,
};

use super::{committed_store_fixture, open_local_chain_index, parity_chain_fixture};

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
    assert_base_compiles::<LocalChainIndex>();
    assert_base_compiles::<RemoteChainIndex>();
    assert_endpoint_compiles::<RemoteChainIndex>();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "End-to-end fixture covering the unspent stream, tx history, and balance refusal in one flow."
)]
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
    let utxo = TransparentUnspentOutput::new(
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
    let chain_fixture = base_fixture.with_address_output_index(utxo.clone());
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let materialized_view_store =
        open_test_materialized_view_store_for_canonical(store_fixture.tempdir_path())?;
    seed_transparent_address_transaction_history(
        &materialized_view_store,
        std::slice::from_ref(&tx_history),
    )?;
    let chain_fence = store_fixture
        .chain_store()
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )?
        .cursor
        .ok_or_else(|| eyre!("committed fixture must have a visible chain fence"))?;
    materialized_view_store.put_chain_event_cursor(
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        chain_fence.as_bytes(),
    )?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let mut utxo_stream = chain_index
        .transparent_address_unspent_outputs(TransparentAddressUnspentOutputsQuery {
            address_script_hash,
            start_height: BlockHeight::new(1),
            at_epoch_id: None,
        })
        .await?;
    let mut utxo_items = Vec::new();
    while let Some(stream_item) = utxo_stream.next().await {
        utxo_items.push(stream_item?);
    }
    let mut history = chain_index
        .transparent_address_tx_ids_in_range(TransparentAddressTxIdsQuery {
            address_script_hash,
            start_height: BlockHeight::new(1),
            end_height: BlockHeight::new(1),
            max_entries: Some(
                NonZeroU32::new(10)
                    .ok_or_else(|| eyre::eyre!("test max_entries constant must be non-zero"))?,
            ),
            from_cursor: None,
            descending: false,
        })
        .await?;
    let history_item = history
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing transparent history item"))??;

    let utxo_chain_epoch = utxo_items
        .first()
        .map(|stream_item| stream_item.chain_epoch)
        .ok_or_else(|| eyre::eyre!("unspent stream must not be empty"))?;
    assert_eq!(
        utxo_items
            .iter()
            .map(|stream_item| stream_item.output.clone())
            .collect::<Vec<_>>(),
        vec![utxo]
    );
    assert_eq!(history_item.chain_epoch, utxo_chain_epoch);
    assert_eq!(history_item.artifact, tx_history);
    assert!(history_item.cursor.is_some());
    assert!(history.next().await.is_none());

    let balance = chain_index
        .transparent_address_balance(&[address_script_hash])
        .await?;
    assert_eq!(balance.confirmed_zat, 321);
    assert_eq!(balance.unconfirmed_delta_zat, 0);
    assert_eq!(balance.address_count, 1);
    assert_eq!(balance.chain_epoch, utxo_chain_epoch);

    Ok(())
}

#[tokio::test]
async fn serves_explorer_transparent_outputs_by_outpoint_in_input_order() -> eyre::Result<()> {
    let base_fixture = parity_chain_fixture(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let block_height = block.height;
    let block_hash = block.hash;
    let indexed_transaction_id = TransactionId::from_bytes([0xAC; 32]);
    let script_pub_key = vec![0x76, 0xa9, 0x33, 0x88, 0xac];
    let chain_fixture = base_fixture.with_address_output_index(TransparentUnspentOutput::new(
        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
        script_pub_key,
        TransparentOutPoint::new(indexed_transaction_id, 0),
        8_000_000,
        block_height,
        block_hash,
    ));
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let unknown_transaction_id = TransactionId::from_bytes([0xDD; 32]);
    let outpoints = [
        TransparentOutPoint::new(indexed_transaction_id, 0),
        TransparentOutPoint::new(unknown_transaction_id, 0),
        TransparentOutPoint::new(indexed_transaction_id, 0),
    ];
    let response = chain_index
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await?;

    assert_eq!(response.entries.len(), 3);
    assert_eq!(response.entries[0].outpoint, outpoints[0]);
    assert_eq!(response.entries[1].outpoint, outpoints[1]);
    assert_eq!(response.entries[2].outpoint, outpoints[2]);
    let resolved_prevout = response.entries[0]
        .output
        .as_ref()
        .ok_or_else(|| eyre!("indexed outpoint must resolve to a prevout"))?;
    assert!(resolved_prevout.value_zat > 0);
    assert!(!resolved_prevout.script_pub_key.is_empty());
    assert!(response.entries[1].output.is_none());
    assert_eq!(
        response.entries[0].output, response.entries[2].output,
        "duplicate input outpoints must produce identical resolutions",
    );
    Ok(())
}

#[tokio::test]
async fn rejects_coinbase_sentinel_in_explorer_transparent_outputs_by_outpoint() -> eyre::Result<()>
{
    let chain_fixture = parity_chain_fixture(1);
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let outpoints = [TransparentOutPoint::COINBASE_SENTINEL];
    let canonical_error = match chain_index
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await
    {
        Ok(response) => {
            return Err(eyre!(
                "expected coinbase-sentinel rejection from canonical prevouts, got {response:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(
        canonical_error.to_string().contains("coinbase sentinel"),
        "canonical prevout error must name the coinbase sentinel anti-pattern; got {canonical_error}"
    );
    Ok(())
}
