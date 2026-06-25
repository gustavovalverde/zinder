#![allow(
    missing_docs,
    reason = "Integration test names describe the compatibility behavior under test."
)]

use async_trait::async_trait;
use eyre::eyre;
use parking_lot::Mutex;
use prost::Message;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Code, Request, transport::Server};
use zebra_chain::{
    parameters::NetworkKind as ZebraNetworkKind, transparent::Address as ZebraTransparentAddress,
};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockSelector, BroadcastDuplicate,
    BroadcastInvalidEncoding, BroadcastRejected, BroadcastRejectionReason, BroadcastUnknown,
    ChainEpochId, ChainTipMetadata, Network, RawTransactionBytes, SUBTREE_LEAF_COUNT,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange,
    TransactionBroadcastResult, TransactionId, TransparentAddressBalance,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact, TransparentOutPoint,
    TransparentOutputsByOutpointResponse, TransparentSpendFact,
    TransparentSpendsByOutpointResponse, TransparentUnspentOutput,
    TransparentUnspentOutputsByOutpointResponse,
};
use zinder_proto::compat::lightwalletd::{
    self, compact_tx_streamer_client::CompactTxStreamerClient,
    compact_tx_streamer_server::CompactTxStreamer,
};

use zinder_query::{
    BlockHeaderResponseValue, BlockIdResponseValue, ChainEvents, CompactBlock, CompactBlockRange,
    LatestBlock, LatestSafeBlock, QueryError, RawTransaction, SubtreeRoots, Transaction,
    TransactionStatus, TransparentAddressTxIds, TransparentAddressTxIdsInRangeRequest,
    TransparentAddressUnspentOutputs, TransparentAddressUnspentOutputsRequest, TreeState,
    WalletQuery, WalletQueryApi,
};
use zinder_store::{ChainEventStreamFamily, StreamCursorTokenV1};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, MockTransactionBroadcaster, StoreFixture,
    open_test_derive_store_for_canonical, sample_regtest_upgrade_activations,
    seed_transparent_address_transaction_history,
};

const ACCEPTANCE_BLOCK_HEIGHT: BlockHeight = BlockHeight::new(1);
const SAPLING_SUBTREE_ROOT_HASH: [u8; 32] = [7; 32];
const DEFAULT_TREE_STATE_PAYLOAD: &[u8] =
    br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":{"finalState":"000000"}},"orchard":{"commitments":{}}}"#;

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "Acceptance test keeps the read-sync RPC matrix together."
)]
async fn lightwalletd_adapter_serves_read_sync_methods() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let latest_block = adapter
        .get_latest_block(Request::new(lightwalletd::ChainSpec {}))
        .await?
        .into_inner();
    let block = adapter
        .get_block(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();
    let block_range = adapter
        .get_block_range(Request::new(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }))
        .await?
        .into_inner();
    let ranged_blocks = collect_stream(block_range).await?;
    let tree_state = adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();
    let latest_tree_state_checkpoint = adapter
        .get_latest_tree_state(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let subtree_roots = adapter
        .get_subtree_roots(Request::new(lightwalletd::GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: lightwalletd::ShieldedProtocol::Sapling as i32,
            max_entries: 1,
        }))
        .await?
        .into_inner();
    let subtree_roots = collect_stream(subtree_roots).await?;
    let lightd_info = adapter
        .get_lightd_info(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();

    assert_eq!(latest_block.height, 1);
    assert_eq!(block.height, 1);
    assert_eq!(block.vtx.len(), 1);
    assert!(
        block.chain_metadata.is_some(),
        "GetBlock must retain commitment-tree sizes"
    );
    assert_eq!(ranged_blocks.len(), 1);
    assert_eq!(ranged_blocks[0].height, 1);
    assert_eq!(ranged_blocks[0].vtx[0].vin.len(), 0);
    assert_eq!(ranged_blocks[0].vtx[0].vout.len(), 0);
    assert!(
        ranged_blocks[0].chain_metadata.is_some(),
        "GetBlockRange must retain commitment-tree sizes"
    );
    assert_eq!(tree_state.sapling_tree, "000000");
    assert_eq!(
        latest_tree_state_checkpoint.sapling_tree,
        tree_state.sapling_tree
    );
    assert_eq!(subtree_roots.len(), 1);
    assert_eq!(subtree_roots[0].completing_block_height, 1);
    assert_eq!(lightd_info.vendor, "Zinder");
    // Regtest collapses to "test" per BIP70 (Zebra's NetworkKind::bip70_network_name).
    // Wallet SDKs match `chainName` against ID_TESTNET, which is "test".
    assert_eq!(lightd_info.chain_name, "test");
    assert_eq!(lightd_info.block_height, 1);
    assert_eq!(lightd_info.estimated_height, 1);
    assert_eq!(
        lightd_info.lightwallet_protocol_version,
        lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT
    );
    assert!(
        !lightd_info.upgrade_name.is_empty(),
        "upgrade_name must be populated from NetworkUpgrade::current"
    );
    assert!(
        lightd_info.upgrade_height > 0,
        "upgrade_height must reflect the active upgrade activation; got {}",
        lightd_info.upgrade_height
    );

    Ok(())
}

/// Both nullifiers-only RPCs must omit everything the deprecated lightwalletd
/// contract excludes.
///
/// The redacted fields are block-level commitment-tree sizes
/// (`chain_metadata`), transparent inputs and outputs, Sapling outputs, and the
/// non-nullifier Orchard action fields. Only the shielded nullifiers survive.
#[tokio::test]
async fn block_nullifiers_omit_commitment_tree_sizes_and_redact_non_nullifier_fields()
-> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let single = adapter
        .get_block_nullifiers(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();
    let ranged = adapter
        .get_block_range_nullifiers(Request::new(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }))
        .await?
        .into_inner();
    let ranged = collect_stream(ranged).await?;

    let ranged_block = ranged
        .first()
        .ok_or_else(|| eyre!("GetBlockRangeNullifiers must return the indexed block"))?;
    assert_nullifiers_only_redaction(&single)?;
    assert_nullifiers_only_redaction(ranged_block)?;

    Ok(())
}

/// Asserts one nullifiers-only compact block carries only shielded nullifiers
/// and has every excluded field cleared.
fn assert_nullifiers_only_redaction(block: &lightwalletd::CompactBlock) -> eyre::Result<()> {
    assert!(
        block.chain_metadata.is_none(),
        "nullifiers-only responses must omit commitment-tree sizes"
    );
    let transaction = block
        .vtx
        .first()
        .ok_or_else(|| eyre!("nullifiers-only block must retain the nullifier-bearing tx"))?;
    assert_eq!(
        transaction.vin.len(),
        0,
        "transparent inputs must be cleared"
    );
    assert_eq!(
        transaction.vout.len(),
        0,
        "transparent outputs must be cleared"
    );
    assert_eq!(
        transaction.outputs.len(),
        0,
        "Sapling outputs must be cleared"
    );
    assert_eq!(
        transaction.spends.len(),
        1,
        "Sapling spend nullifiers must survive"
    );
    assert_eq!(transaction.spends[0].nf, vec![3; 32]);
    let action = transaction
        .actions
        .first()
        .ok_or_else(|| eyre!("Orchard action must survive in nullifier-only form"))?;
    assert_eq!(
        action.nullifier,
        vec![9; 32],
        "Orchard nullifier must survive"
    );
    assert!(action.cmx.is_empty(), "Orchard cmx must be cleared");
    assert!(
        action.ephemeral_key.is_empty(),
        "Orchard ephemeralKey must be cleared"
    );
    assert!(
        action.ciphertext.is_empty(),
        "Orchard ciphertext must be cleared"
    );
    Ok(())
}

/// Records the `at_epoch_id` argument of every canonical read so a test can
/// assert one handler pins all of its reads to a single chain epoch.
#[derive(Clone)]
struct EpochPinRecorder<Inner> {
    inner: Inner,
    recorded_epoch_ids: Arc<Mutex<Vec<Option<ChainEpochId>>>>,
}

impl<Inner> EpochPinRecorder<Inner> {
    fn new(inner: Inner) -> Self {
        Self {
            inner,
            recorded_epoch_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, at_epoch_id: Option<ChainEpochId>) {
        self.recorded_epoch_ids.lock().push(at_epoch_id);
    }

    fn recorded_epoch_ids(&self) -> Vec<Option<ChainEpochId>> {
        self.recorded_epoch_ids.lock().clone()
    }
}

#[async_trait]
impl<Inner: WalletQueryApi + Clone> WalletQueryApi for EpochPinRecorder<Inner> {
    async fn latest_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestBlock, QueryError> {
        self.inner.latest_block(at_epoch_id).await
    }

    async fn latest_safe_block(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<LatestSafeBlock, QueryError> {
        self.record(at_epoch_id);
        self.inner.latest_safe_block(at_epoch_id).await
    }

    async fn block_id_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockIdResponseValue, QueryError> {
        self.record(at_epoch_id);
        self.inner.block_id_by_selector(selector, at_epoch_id).await
    }

    async fn block_header_by_selector(
        &self,
        selector: BlockSelector,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<BlockHeaderResponseValue, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .block_header_by_selector(selector, at_epoch_id)
            .await
    }

    async fn compact_block_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlock, QueryError> {
        self.record(at_epoch_id);
        self.inner.compact_block_at(height, at_epoch_id).await
    }

    async fn compact_blocks_in_range(
        &self,
        block_range: BlockHeightRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<CompactBlockRange, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .compact_blocks_in_range(block_range, at_epoch_id)
            .await
    }

    async fn transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransactionStatus, QueryError> {
        self.record(at_epoch_id);
        self.inner.transaction(transaction_id, at_epoch_id).await
    }

    async fn transaction_at_block_index(
        &self,
        height: BlockHeight,
        tx_index: u64,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<Transaction, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .transaction_at_block_index(height, tx_index, at_epoch_id)
            .await
    }

    async fn raw_transaction(
        &self,
        transaction_id: TransactionId,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<RawTransaction, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .raw_transaction(transaction_id, at_epoch_id)
            .await
    }

    async fn transparent_outputs_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentOutputsByOutpointResponse, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .transparent_outputs_by_outpoint(outpoints, at_epoch_id)
            .await
    }

    async fn transparent_spends_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentSpendsByOutpointResponse, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .transparent_spends_by_outpoint(outpoints, at_epoch_id)
            .await
    }

    async fn transparent_unspent_outputs_by_outpoint(
        &self,
        outpoints: Vec<TransparentOutPoint>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentUnspentOutputsByOutpointResponse, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .transparent_unspent_outputs_by_outpoint(outpoints, at_epoch_id)
            .await
    }

    async fn transparent_address_unspent_outputs(
        &self,
        request: TransparentAddressUnspentOutputsRequest,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressUnspentOutputs, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .transparent_address_unspent_outputs(request, at_epoch_id)
            .await
    }

    async fn transparent_address_tx_ids_in_range(
        &self,
        request: TransparentAddressTxIdsInRangeRequest,
    ) -> Result<TransparentAddressTxIds, QueryError> {
        self.inner
            .transparent_address_tx_ids_in_range(request)
            .await
    }

    async fn transparent_address_balance(
        &self,
        addresses: Vec<TransparentAddressScriptHash>,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TransparentAddressBalance, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .transparent_address_balance(addresses, at_epoch_id)
            .await
    }

    async fn tree_state_at(
        &self,
        height: BlockHeight,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        self.record(at_epoch_id);
        self.inner.tree_state_at(height, at_epoch_id).await
    }

    async fn latest_tree_state_checkpoint(
        &self,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<TreeState, QueryError> {
        self.record(at_epoch_id);
        self.inner.latest_tree_state_checkpoint(at_epoch_id).await
    }

    async fn subtree_roots(
        &self,
        subtree_root_range: SubtreeRootRange,
        at_epoch_id: Option<ChainEpochId>,
    ) -> Result<SubtreeRoots, QueryError> {
        self.record(at_epoch_id);
        self.inner
            .subtree_roots(subtree_root_range, at_epoch_id)
            .await
    }

    async fn chain_events(
        &self,
        from_cursor: Option<StreamCursorTokenV1>,
        family: ChainEventStreamFamily,
    ) -> Result<ChainEvents, QueryError> {
        self.inner.chain_events(from_cursor, family).await
    }

    async fn broadcast_transaction(
        &self,
        raw_transaction: RawTransactionBytes,
    ) -> Result<TransactionBroadcastResult, QueryError> {
        self.inner.broadcast_transaction(raw_transaction).await
    }
}

/// The by-hash `GetTransaction` path issues two canonical reads.
///
/// The status lookup and the raw-transaction fetch must pin to the same chain
/// epoch so the response cannot mix two chain states across a tip move.
#[tokio::test]
async fn get_transaction_by_hash_pins_every_read_to_one_epoch() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x77; 32]);
    let transaction_payload = b"single-epoch-transaction-bytes".to_vec();
    let store_fixture = acceptance_store_fixture_with_transaction_rows(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |block| {
            vec![FixtureTransactionRows::from_raw_transaction(
                transaction_id,
                block.height,
                block.hash,
                0,
                transaction_payload.clone(),
            )]
        },
    )?;
    let recorder = EpochPinRecorder::new(WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    ));
    let adapter = LightwalletdGrpcAdapter::new(
        recorder.clone(),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let transaction = adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: None,
            index: 0,
            hash: transaction_id.as_bytes().to_vec(),
        }))
        .await?
        .into_inner();

    assert_eq!(transaction.data, transaction_payload);
    let recorded_pins = recorder.recorded_epoch_ids();
    assert_eq!(
        recorded_pins.len(),
        2,
        "the by-hash path issues two canonical reads"
    );
    let entry_epoch_id = recorded_pins
        .first()
        .copied()
        .flatten()
        .ok_or_else(|| eyre!("the first canonical read must be pinned to an epoch"))?;
    assert!(
        recorded_pins.iter().all(|pin| *pin == Some(entry_epoch_id)),
        "every read in one handler must pin the same epoch; recorded {recorded_pins:?}"
    );

    Ok(())
}

#[tokio::test]
async fn tree_state_returns_not_found_for_non_checkpoint_height() -> eyre::Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let store_fixture = StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 2,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => return Err(eyre!("expected NOT_FOUND, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the compatibility fixture keeps transparent output and spend fact rows together"
)]
async fn get_address_utxos_stream_returns_indexed_unspent_transparent_outputs() -> eyre::Result<()>
{
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x11; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let transaction_id = TransactionId::from_bytes([0x55; 32]);
    let spent_transaction_id = TransactionId::from_bytes([0x66; 32]);
    let spending_transaction_id = TransactionId::from_bytes([0x77; 32]);
    let store_fixture = acceptance_store_fixture_with_transaction_rows_and_transparent(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |_| Vec::new(),
        |block| {
            let unspent_outpoint = TransparentOutPoint::new(transaction_id, 0);
            let spent_outpoint = TransparentOutPoint::new(spent_transaction_id, 1);
            (
                vec![
                    TransparentUnspentOutput::new(
                        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
                        script_pub_key.clone(),
                        unspent_outpoint,
                        12,
                        block.height,
                        block.hash,
                    ),
                    TransparentUnspentOutput::new(
                        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
                        script_pub_key.clone(),
                        spent_outpoint,
                        13,
                        block.height,
                        block.hash,
                    ),
                ],
                vec![TransparentSpendFact::new(
                    spent_outpoint,
                    0,
                    spending_transaction_id,
                    0,
                    block.height,
                    block.hash,
                    13,
                    TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
                    block.height,
                    block.hash,
                )],
            )
        },
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let request = lightwalletd::GetAddressUtxosArg {
        addresses: vec![address.clone()],
        start_height: 1,
        max_entries: 10,
    };
    let list_response = adapter
        .get_address_utxos(Request::new(request.clone()))
        .await?
        .into_inner();
    let stream_response = adapter
        .get_address_utxos_stream(Request::new(request))
        .await?
        .into_inner();
    let streamed_utxos = collect_stream(stream_response).await?;
    let lightd_info = adapter
        .get_lightd_info(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();

    assert_eq!(list_response.address_utxos, streamed_utxos);
    assert_eq!(streamed_utxos.len(), 1);
    assert_eq!(streamed_utxos[0].address, address);
    assert_eq!(streamed_utxos[0].txid, transaction_id.as_bytes().to_vec());
    assert_eq!(streamed_utxos[0].index, 0);
    assert_eq!(streamed_utxos[0].script, script_pub_key);
    assert_eq!(streamed_utxos[0].value_zat, 12);
    assert_eq!(streamed_utxos[0].height, 1);
    assert!(lightd_info.taddr_support);

    Ok(())
}

/// Regression: txid bytes emitted by `GetAddressUtxos` must be accepted verbatim
/// by `GetTransaction(TxFilter { hash, ... })`.
///
/// The 2026-05-12 parity run surfaced a `NotFound` when wallets rebound the
/// bytes round-trip; the cause was an unnecessary byte reversal at the input
/// handler. Lightwalletd-go documents the wire contract at
/// `frontend/service.go:792`: txid `bytes` fields are Zcash internal
/// little-endian, the same byte order [`TransactionId::as_bytes`] returns.
#[tokio::test]
async fn get_address_utxos_txid_round_trips_through_get_transaction_by_hash() -> eyre::Result<()> {
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x44; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let transaction_id = TransactionId::from_bytes([0x77; 32]);
    let transaction_payload = b"round-trip-transaction-bytes".to_vec();
    let store_fixture = acceptance_store_fixture_with_transaction_rows_and_transparent(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |block| {
            vec![FixtureTransactionRows::from_raw_transaction(
                transaction_id,
                block.height,
                block.hash,
                0,
                transaction_payload.clone(),
            )]
        },
        |block| {
            (
                vec![TransparentUnspentOutput::new(
                    TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
                    script_pub_key.clone(),
                    TransparentOutPoint::new(transaction_id, 0),
                    21,
                    block.height,
                    block.hash,
                )],
                Vec::new(),
            )
        },
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let utxos = adapter
        .get_address_utxos(Request::new(lightwalletd::GetAddressUtxosArg {
            addresses: vec![address],
            start_height: 1,
            max_entries: 10,
        }))
        .await?
        .into_inner();
    let utxo = utxos
        .address_utxos
        .first()
        .ok_or_else(|| eyre!("expected one UTXO in the round-trip fixture"))?;

    let transaction = adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: None,
            index: 0,
            hash: utxo.txid.clone(),
        }))
        .await?
        .into_inner();

    assert_eq!(transaction.height, 1);
    assert_eq!(transaction.data, transaction_payload);

    Ok(())
}

#[tokio::test]
async fn get_address_utxos_applies_max_entries_across_address_set() -> eyre::Result<()> {
    let transparent_address_a =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x21; 20]);
    let transparent_address_b =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x22; 20]);
    let address_a = transparent_address_a.to_string();
    let address_b = transparent_address_b.to_string();
    let script_pub_key_a = transparent_address_a.script().as_raw_bytes().to_vec();
    let script_pub_key_b = transparent_address_b.script().as_raw_bytes().to_vec();
    let first_transaction_id = TransactionId::from_bytes([0x10; 32]);
    let second_transaction_id = TransactionId::from_bytes([0x20; 32]);
    let truncated_transaction_id = TransactionId::from_bytes([0x30; 32]);
    let store_fixture = acceptance_store_fixture_with_transaction_rows_and_transparent(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |_| Vec::new(),
        |block| {
            (
                vec![
                    TransparentUnspentOutput::new(
                        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key_a),
                        script_pub_key_a.clone(),
                        TransparentOutPoint::new(truncated_transaction_id, 0),
                        30,
                        block.height,
                        block.hash,
                    ),
                    TransparentUnspentOutput::new(
                        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key_b),
                        script_pub_key_b.clone(),
                        TransparentOutPoint::new(first_transaction_id, 0),
                        10,
                        block.height,
                        block.hash,
                    ),
                    TransparentUnspentOutput::new(
                        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key_b),
                        script_pub_key_b.clone(),
                        TransparentOutPoint::new(second_transaction_id, 1),
                        20,
                        block.height,
                        block.hash,
                    ),
                ],
                Vec::new(),
            )
        },
    )?;
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(store_fixture.chain_store().clone(), (), activations.clone()),
        activations,
    );

    let request = lightwalletd::GetAddressUtxosArg {
        addresses: vec![address_a, address_b.clone()],
        start_height: 1,
        max_entries: 2,
    };
    let list_response = adapter
        .get_address_utxos(Request::new(request.clone()))
        .await?
        .into_inner();
    let stream_response = adapter
        .get_address_utxos_stream(Request::new(request))
        .await?
        .into_inner();
    let streamed_utxos = collect_stream(stream_response).await?;
    let returned_txids: Vec<_> = streamed_utxos
        .iter()
        .map(|utxo| utxo.txid.clone())
        .collect();

    assert_eq!(list_response.address_utxos, streamed_utxos);
    assert_eq!(streamed_utxos.len(), 2);
    assert_eq!(
        returned_txids,
        vec![
            first_transaction_id.as_bytes().to_vec(),
            second_transaction_id.as_bytes().to_vec(),
        ]
    );
    assert!(streamed_utxos.iter().all(|utxo| utxo.address == address_b));

    Ok(())
}

#[tokio::test]
async fn get_taddress_history_drains_native_pages() -> eyre::Result<()> {
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x31; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    let (store_fixture, derive_store) =
        acceptance_store_fixture_with_transaction_rows_and_tx_history(
            DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
            |block| {
                (0..1001)
                    .map(|index| {
                        FixtureTransactionRows::from_raw_transaction(
                            tx_id_for_index(index),
                            block.height,
                            block.hash,
                            index,
                            tx_payload_for_index(index),
                        )
                    })
                    .collect()
            },
            |block| {
                (0..1001)
                    .map(|index| {
                        TransparentAddressTxIndexArtifact::new(
                            address_script_hash,
                            block.height,
                            index,
                            tx_id_for_index(index),
                            block.hash,
                        )
                    })
                    .collect()
            },
        )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        )
        .with_derive_store(derive_store),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let request = transparent_address_block_filter(address);

    let txids = adapter
        .get_taddress_txids(Request::new(request.clone()))
        .await?
        .into_inner();
    let txids = collect_stream(txids).await?;
    let transactions = adapter
        .get_taddress_transactions(Request::new(request))
        .await?
        .into_inner();
    let transactions = collect_stream(transactions).await?;

    assert_eq!(txids.len(), 1001);
    assert_eq!(transactions.len(), 1001);
    assert_eq!(txids[0].data, tx_id_for_index(0).as_bytes().to_vec());
    assert_eq!(txids[1000].data, tx_id_for_index(1000).as_bytes().to_vec());
    assert_eq!(transactions[0].data, tx_payload_for_index(0));
    assert_eq!(transactions[1000].data, tx_payload_for_index(1000));

    Ok(())
}

#[tokio::test]
async fn send_transaction_forwards_accepted_to_zero_error_code() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let transaction_id = TransactionId::from_bytes([0x42; 32]);
    let broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            broadcaster,
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let response = adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0xde, 0xad, 0xbe, 0xef],
            height: 0,
        }))
        .await?
        .into_inner();

    assert_eq!(response.error_code, 0);
    let mut expected_id = transaction_id.as_bytes();
    expected_id.reverse();
    assert_eq!(response.error_message, hex::encode(expected_id));

    Ok(())
}

#[tokio::test]
async fn send_transaction_maps_invalid_encoding_to_minus_22() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let broadcaster = MockTransactionBroadcaster::returning(
        TransactionBroadcastResult::InvalidEncoding(BroadcastInvalidEncoding {
            error_code: None,
            message: "TX decode failed".to_owned(),
        }),
    );
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            broadcaster,
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let response = adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0xff],
            height: 0,
        }))
        .await?
        .into_inner();

    assert_eq!(response.error_code, -22);
    assert_eq!(response.error_message, "TX decode failed");

    Ok(())
}

#[tokio::test]
async fn send_transaction_maps_duplicate_and_rejected_codes() -> eyre::Result<()> {
    let cases = [
        (
            TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
                error_code: None,
                message: "transaction already in mempool".to_owned(),
            }),
            -27,
            "transaction already in mempool",
        ),
        (
            TransactionBroadcastResult::Rejected(BroadcastRejected {
                kind: BroadcastRejectionReason::Unknown,
                error_code: None,
                message: "bad-txns-invalid".to_owned(),
            }),
            -26,
            "bad-txns-invalid",
        ),
        (
            TransactionBroadcastResult::Unknown(BroadcastUnknown {
                error_code: None,
                message: "node returned unclassified".to_owned(),
            }),
            -1,
            "node returned unclassified",
        ),
    ];

    for (broadcast_result, expected_code, expected_message) in cases {
        let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
        let broadcaster = MockTransactionBroadcaster::returning(broadcast_result);
        let adapter = LightwalletdGrpcAdapter::new(
            WalletQuery::new(
                store_fixture.chain_store().clone(),
                broadcaster,
                Arc::new(sample_regtest_upgrade_activations()),
            ),
            Arc::new(sample_regtest_upgrade_activations()),
        );

        let response = adapter
            .send_transaction(Request::new(lightwalletd::RawTransaction {
                data: vec![0x00],
                height: 0,
            }))
            .await?
            .into_inner();

        assert_eq!(response.error_code, expected_code);
        assert_eq!(response.error_message, expected_message);
    }

    Ok(())
}

#[tokio::test]
async fn send_transaction_forwards_node_error_code_when_present() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let broadcaster = MockTransactionBroadcaster::returning(TransactionBroadcastResult::Rejected(
        BroadcastRejected {
            kind: BroadcastRejectionReason::Unknown,
            error_code: Some(-25),
            message: "missing-input".to_owned(),
        },
    ));
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            broadcaster,
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let response = adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0x00],
            height: 0,
        }))
        .await?
        .into_inner();

    assert_eq!(response.error_code, -25);
    assert_eq!(response.error_message, "missing-input");

    Ok(())
}

#[tokio::test]
async fn send_transaction_reports_broadcast_disabled() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0x00],
            height: 0,
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected disabled error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::FailedPrecondition);

    Ok(())
}

#[tokio::test]
async fn generated_lightwalletd_client_streams_over_grpc_transport() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    )
    .into_server();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    {
        let mut client = CompactTxStreamerClient::connect(format!("http://{server_addr}")).await?;
        let latest_block = client
            .get_latest_block(lightwalletd::ChainSpec {})
            .await?
            .into_inner();
        let mut compact_blocks = client
            .get_block_range(lightwalletd::BlockRange {
                start: Some(lightwalletd::BlockId {
                    height: latest_block.height,
                    hash: Vec::new(),
                }),
                end: Some(lightwalletd::BlockId {
                    height: latest_block.height,
                    hash: Vec::new(),
                }),
                pool_types: Vec::new(),
            })
            .await?
            .into_inner();
        let tree_state = client
            .get_latest_tree_state(lightwalletd::Empty {})
            .await?
            .into_inner();

        let compact_block = compact_blocks
            .message()
            .await?
            .ok_or_else(|| eyre!("missing compact block from compatibility stream"))?;

        assert!(compact_blocks.message().await?.is_none());
        assert_eq!(latest_block.height, 1);
        assert_eq!(compact_block.height, latest_block.height);
        assert_eq!(tree_state.height, latest_block.height);
    }

    let _ = shutdown_tx.send(());
    server_handle.await??;
    Ok(())
}

#[tokio::test]
async fn tree_state_reports_missing_non_empty_final_state_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":{"size":1}},"orchard":{"commitments":{}}}"#
            .to_vec(),
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!(
                "expected malformed tree-state error, got {response:?}"
            ));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_rejects_zero_empty_block_id_without_reader_fallback() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 0,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => return Err(eyre!("expected block 0 error, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);

    Ok(())
}

#[tokio::test]
async fn tree_state_treats_absent_pool_and_empty_commitments_as_empty() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"orchard":{"commitments":{}}}"#.to_vec(),
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let tree_state = adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();

    assert_eq!(tree_state.sapling_tree, "");
    assert_eq!(tree_state.orchard_tree, "");

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_wrong_pool_shape_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"sapling":[]}"#.to_vec(),
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected malformed pool error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_wrong_commitments_shape_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":[]}}"#.to_vec(),
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!(
                "expected malformed commitments error, got {response:?}"
            ));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_missing_time_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"sapling":{"commitments":{}},"orchard":{"commitments":{}}}"#
            .to_vec(),
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected missing-time error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_wrong_time_type_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":"1296694002","sapling":{"commitments":{}},"orchard":{"commitments":{}}}"#
            .to_vec(),
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected wrong-time-type error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn ping_returns_zero_entry_and_exit() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let response = adapter
        .ping(Request::new(lightwalletd::Duration { interval_us: 0 }))
        .await?
        .into_inner();

    assert_eq!(response.entry, 0);
    assert_eq!(response.exit, 0);

    Ok(())
}

#[tokio::test]
async fn get_transaction_by_block_index_returns_indexed_transaction() -> eyre::Result<()> {
    let acceptance_txid_bytes = [2u8; 32];
    let store_fixture = acceptance_store_fixture_with_transaction_rows(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |block| {
            vec![FixtureTransactionRows::from_raw_transaction(
                TransactionId::from_bytes(acceptance_txid_bytes),
                block.height,
                block.hash,
                0,
                b"acceptance-transaction-bytes".to_vec(),
            )]
        },
    )?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let response = adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            index: 0,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();

    assert_eq!(response.height, 1);
    assert_eq!(response.data, b"acceptance-transaction-bytes");

    Ok(())
}

#[tokio::test]
async fn get_transaction_by_block_index_returns_not_found_for_unknown_index() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter = LightwalletdGrpcAdapter::new(
        WalletQuery::new(
            store_fixture.chain_store().clone(),
            (),
            Arc::new(sample_regtest_upgrade_activations()),
        ),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let status = match adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            index: 99,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected not-found error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);

    Ok(())
}

async fn collect_stream<T, Stream>(mut stream: Stream) -> Result<Vec<T>, tonic::Status>
where
    Stream: tonic::codegen::tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    use tonic::codegen::tokio_stream::StreamExt;

    let mut values = Vec::new();
    while let Some(stream_item) = stream.next().await {
        values.push(stream_item?);
    }
    Ok(values)
}

fn acceptance_store_fixture(tree_state_payload: Vec<u8>) -> eyre::Result<StoreFixture> {
    acceptance_store_fixture_with_transaction_rows(tree_state_payload, |_| Vec::new())
}

fn acceptance_store_fixture_with_transaction_rows<TransactionsFn>(
    tree_state_payload: Vec<u8>,
    build_transaction_rows: TransactionsFn,
) -> eyre::Result<StoreFixture>
where
    TransactionsFn: FnOnce(&zinder_testkit::FixtureBlock) -> Vec<FixtureTransactionRows>,
{
    acceptance_store_fixture_with_transaction_rows_and_transparent(
        tree_state_payload,
        build_transaction_rows,
        |_| (Vec::new(), Vec::new()),
    )
}

fn acceptance_store_fixture_with_transaction_rows_and_transparent<TransactionsFn, TransparentFn>(
    tree_state_payload: Vec<u8>,
    build_transaction_rows: TransactionsFn,
    build_transparent_artifacts: TransparentFn,
) -> eyre::Result<StoreFixture>
where
    TransactionsFn: FnOnce(&zinder_testkit::FixtureBlock) -> Vec<FixtureTransactionRows>,
    TransparentFn: FnOnce(
        &zinder_testkit::FixtureBlock,
    ) -> (Vec<TransparentUnspentOutput>, Vec<TransparentSpendFact>),
{
    let base_fixture = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .with_tip_metadata_override(ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0))
        .with_tree_state_checkpoint_payload_at(ACCEPTANCE_BLOCK_HEIGHT, tree_state_payload);
    let acceptance_block = base_fixture
        .block_at(ACCEPTANCE_BLOCK_HEIGHT)
        .ok_or_else(|| eyre!("acceptance fixture must include the height 1 block"))?
        .clone();
    let block_hash = acceptance_block.hash;
    let parent_hash = acceptance_block.parent_hash;
    let block_time_seconds = acceptance_block.block_time_seconds;
    let transaction_rows = build_transaction_rows(&acceptance_block);
    let (address_output_index, transparent_spend_facts) =
        build_transparent_artifacts(&acceptance_block);

    let mut chain_fixture = base_fixture
        .with_compact_block_payload_at(
            ACCEPTANCE_BLOCK_HEIGHT,
            acceptance_compact_block_payload(block_hash, parent_hash, block_time_seconds),
        )
        .with_sapling_subtree_root(SubtreeRootArtifact::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            SubtreeRootHash::from_bytes(SAPLING_SUBTREE_ROOT_HASH),
            ACCEPTANCE_BLOCK_HEIGHT,
            block_hash,
        ));
    for transaction_rows in transaction_rows {
        chain_fixture = chain_fixture.with_transaction_rows(transaction_rows);
    }
    for address_output_index in address_output_index {
        chain_fixture = chain_fixture.with_address_output_index(address_output_index);
    }
    for transparent_spend_fact in transparent_spend_facts {
        chain_fixture = chain_fixture.with_transparent_spend_fact(transparent_spend_fact);
    }

    Ok(StoreFixture::with_chain_committed(
        &chain_fixture,
        ChainEpochId::new(1),
    )?)
}

fn acceptance_store_fixture_with_transaction_rows_and_tx_history<TransactionsFn, TxHistoryFn>(
    tree_state_payload: Vec<u8>,
    build_transaction_rows: TransactionsFn,
    build_tx_history: TxHistoryFn,
) -> eyre::Result<(StoreFixture, zinder_derive::DeriveStore)>
where
    TransactionsFn: FnOnce(&zinder_testkit::FixtureBlock) -> Vec<FixtureTransactionRows>,
    TxHistoryFn: FnOnce(&zinder_testkit::FixtureBlock) -> Vec<TransparentAddressTxIndexArtifact>,
{
    let base_fixture = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .with_tip_metadata_override(ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0))
        .with_tree_state_checkpoint_payload_at(ACCEPTANCE_BLOCK_HEIGHT, tree_state_payload);
    let acceptance_block = base_fixture
        .block_at(ACCEPTANCE_BLOCK_HEIGHT)
        .ok_or_else(|| eyre!("acceptance fixture must include the height 1 block"))?
        .clone();
    let block_hash = acceptance_block.hash;
    let parent_hash = acceptance_block.parent_hash;
    let block_time_seconds = acceptance_block.block_time_seconds;
    let transaction_rows = build_transaction_rows(&acceptance_block);
    let tx_history = build_tx_history(&acceptance_block);

    let mut chain_fixture = base_fixture
        .with_compact_block_payload_at(
            ACCEPTANCE_BLOCK_HEIGHT,
            acceptance_compact_block_payload(block_hash, parent_hash, block_time_seconds),
        )
        .with_sapling_subtree_root(SubtreeRootArtifact::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            SubtreeRootHash::from_bytes(SAPLING_SUBTREE_ROOT_HASH),
            ACCEPTANCE_BLOCK_HEIGHT,
            block_hash,
        ));
    for transaction_rows in transaction_rows {
        chain_fixture = chain_fixture.with_transaction_rows(transaction_rows);
    }

    let store_fixture = StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    seed_transparent_address_transaction_history(&derive_store, &tx_history)?;

    Ok((store_fixture, derive_store))
}

fn acceptance_compact_block_payload(
    block_hash: BlockHash,
    parent_hash: BlockHash,
    block_time_seconds: u32,
) -> Vec<u8> {
    lightwalletd::CompactBlock {
        proto_version: 1,
        height: 1,
        hash: block_hash.as_bytes().to_vec(),
        prev_hash: parent_hash.as_bytes().to_vec(),
        time: block_time_seconds,
        header: Vec::new(),
        vtx: vec![lightwalletd::CompactTx {
            index: 0,
            txid: vec![2; 32],
            fee: 0,
            spends: vec![lightwalletd::CompactSaplingSpend { nf: vec![3; 32] }],
            outputs: vec![lightwalletd::CompactSaplingOutput {
                cmu: vec![4; 32],
                ephemeral_key: vec![5; 32],
                ciphertext: vec![6; 52],
            }],
            actions: vec![lightwalletd::CompactOrchardAction {
                nullifier: vec![9; 32],
                cmx: vec![10; 32],
                ephemeral_key: vec![11; 32],
                ciphertext: vec![12; 52],
            }],
            vin: vec![lightwalletd::CompactTxIn {
                prevout_txid: vec![8; 32],
                prevout_index: 1,
            }],
            vout: vec![lightwalletd::TxOut {
                value: 5,
                script_pub_key: vec![0x51],
            }],
        }],
        chain_metadata: Some(lightwalletd::ChainMetadata {
            sapling_commitment_tree_size: SUBTREE_LEAF_COUNT,
            orchard_commitment_tree_size: 0,
        }),
    }
    .encode_to_vec()
}

fn transparent_address_block_filter(
    address: String,
) -> lightwalletd::TransparentAddressBlockFilter {
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

fn tx_id_for_index(index: u32) -> TransactionId {
    let mut bytes = [0; 32];
    bytes[..4].copy_from_slice(&index.to_be_bytes());
    TransactionId::from_bytes(bytes)
}

fn tx_payload_for_index(index: u32) -> Vec<u8> {
    format!("tx-payload-{index}").into_bytes()
}
