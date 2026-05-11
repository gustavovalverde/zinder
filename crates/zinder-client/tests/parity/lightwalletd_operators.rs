//! Public lightwalletd operator parity assertions.
//!
//! `zec.rocks`-style operators expose the lightwalletd compat surface to
//! third-party clients. These compile-time assertions ensure the typed
//! `ChainIndex` methods that back compat surfaces remain on the trait.
//!
//! Cross-references: [Serving public lightwalletd clients](../../../../docs/reference/serving-public-lightwalletd-clients.md).

use tokio_stream::StreamExt as _;
use tonic::Request;
use zebra_chain::{
    parameters::NetworkKind as ZebraNetworkKind, transparent::Address as ZebraTransparentAddress,
};
use zinder_client::{
    ChainIndex, LocalChainIndex, RemoteChainIndex, TransactionArtifact, TransactionId,
    TransparentAddressTxIndexArtifact, TransparentAddressUtxoArtifact, TransparentOutPoint,
};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_proto::compat::lightwalletd::{self, compact_tx_streamer_server::CompactTxStreamer};
use zinder_query::{WalletQuery, transparent_address_script_hash};
use zinder_testkit::StoreFixture;

use super::{committed_store_fixture, parity_chain_fixture};

#[test]
fn parity_chain_index_surface_compiles_for_lightwalletd_operators() {
    fn assert_compiles<T: ChainIndex>() {
        // GetTaddressBalance backed by typed TransparentAddressBalance
        let _ = T::transparent_address_balance;
        // GetBlock and GetTreeState hash-only paths via BlockSelector
        let _ = T::block_id_by_selector;
        // GetSubtreeRoots typed bytes + typed pool enum
        let _ = T::subtree_roots_in_range;
        // SendTransaction typed bytes path
        let _ = T::broadcast_transaction;
        // TipAdvanced typed signal for the live chain-event stream
        let _ = T::chain_events;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}

#[tokio::test]
async fn serves_public_transparent_address_shape_from_fixture() -> eyre::Result<()> {
    let fixture = public_operator_fixture()?;
    let adapter = LightwalletdGrpcAdapter::new(WalletQuery::new(
        fixture.store_fixture.chain_store().clone(),
        (),
    ));

    let utxo_request = lightwalletd::GetAddressUtxosArg {
        addresses: vec![fixture.address.clone()],
        start_height: 1,
        max_entries: 10,
    };
    let utxo_list = adapter
        .get_address_utxos(Request::new(utxo_request.clone()))
        .await?
        .into_inner();
    let mut utxo_stream = adapter
        .get_address_utxos_stream(Request::new(utxo_request))
        .await?
        .into_inner();
    let first_streamed_utxo = utxo_stream
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing streamed UTXO"))??;
    let history_filter = address_history_filter(fixture.address.clone());
    let mut txids = adapter
        .get_taddress_txids(Request::new(history_filter.clone()))
        .await?
        .into_inner();
    let mut transactions = adapter
        .get_taddress_transactions(Request::new(history_filter))
        .await?
        .into_inner();

    assert_eq!(utxo_list.address_utxos, vec![first_streamed_utxo.clone()]);
    assert!(utxo_stream.next().await.is_none());
    assert_eq!(first_streamed_utxo.address, fixture.address.as_str());
    assert_eq!(
        first_streamed_utxo.txid,
        fixture.transaction_id.as_bytes().to_vec()
    );
    assert_eq!(first_streamed_utxo.index, 0);
    assert_eq!(
        first_streamed_utxo.script,
        fixture.script_pub_key.as_slice()
    );
    assert_eq!(first_streamed_utxo.value_zat, 123);
    assert_eq!(first_streamed_utxo.height, 1);

    let txid_response = txids
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing transparent txid response"))??;
    let transaction_response = transactions
        .next()
        .await
        .ok_or_else(|| eyre::eyre!("missing transparent transaction response"))??;
    assert_eq!(
        txid_response.data,
        fixture.transaction_id.as_bytes().to_vec()
    );
    assert_eq!(transaction_response.data, fixture.transaction.payload_bytes);
    assert_eq!(transaction_response.height, 1);
    assert!(txids.next().await.is_none());
    assert!(transactions.next().await.is_none());

    Ok(())
}

struct PublicOperatorFixture {
    store_fixture: StoreFixture,
    address: String,
    script_pub_key: Vec<u8>,
    transaction_id: TransactionId,
    transaction: TransactionArtifact,
}

fn public_operator_fixture() -> eyre::Result<PublicOperatorFixture> {
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x11; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let address_script_hash = transparent_address_script_hash(&script_pub_key);
    let transaction_id = TransactionId::from_bytes([0x55; 32]);
    let base_fixture = parity_chain_fixture(1);
    let block = base_fixture
        .block_at(zinder_client::BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("fixture must contain block 1"))?;
    let block_height = block.height;
    let block_hash = block.hash;
    let transaction = TransactionArtifact::new(
        transaction_id,
        block_height,
        block_hash,
        b"operator-transaction-payload".to_vec(),
    );
    let chain_fixture = base_fixture
        .with_transaction_artifact(transaction.clone())
        .with_transparent_address_utxo(TransparentAddressUtxoArtifact::new(
            address_script_hash,
            script_pub_key.clone(),
            TransparentOutPoint::new(transaction_id, 0),
            123,
            block_height,
            block_hash,
        ))
        .with_transparent_address_tx_index(TransparentAddressTxIndexArtifact::new(
            address_script_hash,
            block_height,
            0,
            transaction_id,
            block_hash,
        ));
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    Ok(PublicOperatorFixture {
        store_fixture,
        address,
        script_pub_key,
        transaction_id,
        transaction,
    })
}

fn address_history_filter(address: String) -> lightwalletd::TransparentAddressBlockFilter {
    lightwalletd::TransparentAddressBlockFilter {
        address,
        range: Some(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }),
    }
}
