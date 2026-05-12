//! Block-explorer parity assertions.
//!
//! Block explorers exercise typed `WalletQuery` and federated
//! `derive.explorer.*` surfaces to display per-block, per-transaction, and
//! per-address state. These compile-time assertions ensure the trait surface
//! they depend on stays intact through future refactors.

use std::num::NonZeroU32;

use eyre::eyre;
use tokio_stream::StreamExt as _;
use zinder_client::{
    BlockHeight, ChainIndex, IndexerError, LocalChainIndex, RemoteChainIndex, TransactionArtifact,
    TransactionId, TransparentAddressScriptHash, TransparentAddressTxIdsQuery,
    TransparentAddressTxIndexArtifact, TransparentAddressUtxoArtifact,
    TransparentAddressUtxosQuery, TransparentOutPoint,
};
use zinder_testkit::{
    P2pkhSpendArgs, TransparentAddress as TestkitTransparentAddress, TransparentTestKey,
};

use super::{committed_store_fixture, open_local_chain_index, parity_chain_fixture};

const PARITY_PREVOUT_FIXTURE_SEED: [u8; 32] = [
    0xC0, 0xFF, 0xEE, 0xC0, 0xDE, 0xCA, 0xFE, 0xBA, 0xBE, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE,
    0xF0, 0x0F, 0xED, 0xCB, 0xA9, 0x87, 0x65, 0x43, 0x21, 0x0F, 0xE0, 0xDC, 0xBA, 0x98, 0x76, 0x54,
];

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
        // M6 canonical prevout resolution and live-mempool prevout fallback.
        // Explorers and SDKs that decode transaction inputs depend on this
        // pair staying in the contract; renaming or removing either is a
        // breaking change.
        let _ = T::transparent_prevouts;
        let _ = T::transparent_mempool_prevouts;
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

#[tokio::test]
async fn serves_explorer_transparent_prevouts_in_input_order() -> eyre::Result<()> {
    let signer = TransparentTestKey::from_seed(&PARITY_PREVOUT_FIXTURE_SEED)?;
    let recipient = TestkitTransparentAddress::PublicKeyHash([0x33; 20]);
    let raw_transaction_bytes = signer.build_p2pkh_spend(&P2pkhSpendArgs {
        coinbase_txid_be: [0xBB; 32],
        coinbase_vout: 0,
        coinbase_value_zats: 8_000_000,
        recipient: &recipient,
        target_height: 1,
    })?;
    let base_fixture = parity_chain_fixture(1);
    let block = base_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let indexed_transaction_id = TransactionId::from_bytes([0xAC; 32]);
    let transaction_artifact = TransactionArtifact::new(
        indexed_transaction_id,
        block.height,
        block.hash,
        raw_transaction_bytes,
    );
    let chain_fixture = base_fixture.with_transaction_artifact(transaction_artifact);
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let unknown_transaction_id = TransactionId::from_bytes([0xDD; 32]);
    let outpoints = [
        TransparentOutPoint::new(indexed_transaction_id, 0),
        TransparentOutPoint::new(unknown_transaction_id, 0),
        TransparentOutPoint::new(indexed_transaction_id, 0),
    ];
    let response = chain_index.transparent_prevouts(&outpoints, None).await?;

    assert_eq!(response.entries.len(), 3);
    assert_eq!(response.entries[0].outpoint, outpoints[0]);
    assert_eq!(response.entries[1].outpoint, outpoints[1]);
    assert_eq!(response.entries[2].outpoint, outpoints[2]);
    let resolved_prevout = response.entries[0]
        .prevout
        .as_ref()
        .ok_or_else(|| eyre!("indexed outpoint must resolve to a prevout"))?;
    assert!(resolved_prevout.value_zat > 0);
    assert!(!resolved_prevout.script_pub_key.is_empty());
    assert!(response.entries[1].prevout.is_none());
    assert_eq!(
        response.entries[0].prevout, response.entries[2].prevout,
        "duplicate input outpoints must produce identical resolutions",
    );
    Ok(())
}

#[tokio::test]
async fn rejects_coinbase_sentinel_in_explorer_transparent_prevouts() -> eyre::Result<()> {
    let chain_fixture = parity_chain_fixture(1);
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let outpoints = [TransparentOutPoint::COINBASE_SENTINEL];
    let canonical_error = match chain_index.transparent_prevouts(&outpoints, None).await {
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

    let mempool_error = match chain_index.transparent_mempool_prevouts(&outpoints).await {
        Ok(response) => {
            return Err(eyre!(
                "expected coinbase-sentinel rejection from mempool prevouts, got {response:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(
        matches!(
            &mempool_error,
            IndexerError::RemoteEndpointUnconfigured { .. }
        ) || mempool_error.to_string().contains("coinbase sentinel"),
        "mempool prevout error must either reject the coinbase sentinel or report \
         a missing remote-proxy endpoint when no IngestControl is wired; got {mempool_error}"
    );
    Ok(())
}
