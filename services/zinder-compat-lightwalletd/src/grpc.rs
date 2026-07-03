//! gRPC implementation for the vendored lightwalletd protocol.

use std::{num::NonZeroU32, pin::Pin, sync::Arc};

use prost::Message;
use serde_json::Value;
use tokio_stream::StreamExt as _;
use tokio_stream::{self as stream};
use tonic::{Request, Response, Status};
use zebra_chain::transparent::Address as ZebraTransparentAddress;
use zinder_core::wire::WireDecodeError;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, BlockSelector, BroadcastAccepted, BroadcastDuplicate,
    BroadcastInvalidEncoding, BroadcastQueued, BroadcastRejected, BroadcastUnknown, ChainEpochId,
    CompactBlockArtifact, Network, NetworkUpgradeActivations, RawTransactionBytes,
    ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange, TransactionBroadcastResult,
    TransactionLocation, TransparentAddressScriptHash, TransparentAddressTxIndexArtifact, TxStatus,
    wire::{
        decode_internal_transaction_id, encode_bip70_chain_name, encode_branch_id_hex,
        encode_internal_block_hash, encode_internal_transaction_id, encode_rpc_block_hash_hex,
        encode_rpc_transaction_id_hex,
    },
};
use zinder_proto::compat::lightwalletd::{
    self, LIGHTWALLETD_PROTOCOL_COMMIT, compact_tx_streamer_server,
};
use zinder_proto::v1::wallet::{self as wallet_proto, address_lookup};
use zinder_query::{
    SubtreeRoots, TransparentAddressTxIdsInRangeRequest, TransparentAddressUnspentOutputs,
    TransparentAddressUnspentOutputsRequest, TreeState, WalletQueryApi,
    address_lookup_to_script_hash, status_from_query_error,
};
use zinder_source::transparent_address_matches_network;
use zinder_store::MempoolEvent;

use crate::mempool::{
    MempoolSnapshotPage, MempoolSurfaceError, SharedMempoolSurface, SharedTipChangeWatcher,
    TipChangeWatcherError,
};

type GrpcStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Default maximum subtree roots returned when lightwalletd requests "all entries".
pub const DEFAULT_MAX_LIGHTWALLETD_SUBTREE_ROOTS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);
/// Default maximum transparent UTXOs returned when lightwalletd requests "all entries".
pub const DEFAULT_MAX_LIGHTWALLETD_ADDRESS_UTXOS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);
/// Page size used to drain the native mempool snapshot for lightwalletd.
const LIGHTWALLETD_MEMPOOL_SNAPSHOT_PAGE_SIZE: u32 = 1024;

/// Runtime options for [`LightwalletdGrpcAdapter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightwalletdCompatibilityOptions {
    /// Bound used when `GetSubtreeRoots.maxEntries` is zero.
    ///
    /// Upstream lightwalletd defines zero as "all entries". Zinder keeps the
    /// response bounded so one compatibility request cannot materialize
    /// unbounded history.
    pub max_subtree_roots: NonZeroU32,
    /// Bound used when `GetAddressUtxos.maxEntries` is zero.
    pub max_address_utxos: NonZeroU32,
}

impl Default for LightwalletdCompatibilityOptions {
    fn default() -> Self {
        Self {
            max_subtree_roots: DEFAULT_MAX_LIGHTWALLETD_SUBTREE_ROOTS,
            max_address_utxos: DEFAULT_MAX_LIGHTWALLETD_ADDRESS_UTXOS,
        }
    }
}

/// gRPC adapter from [`WalletQueryApi`] to lightwalletd `CompactTxStreamer`.
#[derive(Clone)]
pub struct LightwalletdGrpcAdapter<QueryApi> {
    query_api: QueryApi,
    options: LightwalletdCompatibilityOptions,
    mempool_surface: Option<SharedMempoolSurface>,
    tip_change_watcher: Option<SharedTipChangeWatcher>,
    network_upgrade_activations: Arc<NetworkUpgradeActivations>,
}

impl<QueryApi: std::fmt::Debug> std::fmt::Debug for LightwalletdGrpcAdapter<QueryApi> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LightwalletdGrpcAdapter")
            .field("query_api", &self.query_api)
            .field("options", &self.options)
            .field("mempool_surface", &self.mempool_surface.is_some())
            .field("tip_change_watcher", &self.tip_change_watcher.is_some())
            .field(
                "network_upgrade_activations",
                &self.network_upgrade_activations.network(),
            )
            .finish()
    }
}

impl<QueryApi> LightwalletdGrpcAdapter<QueryApi> {
    /// Creates a lightwalletd-compatible gRPC adapter.
    ///
    /// `network_upgrade_activations` carries the node-discovered upgrade
    /// table that backs `GetLightdInfo`'s `consensusBranchId`,
    /// `upgradeName`, and `upgradeHeight` fields. In production it is
    /// shared via
    /// `ZebraJsonRpcSource::discover_network_upgrade_activations`; tests use
    /// `zinder_testkit::sample_regtest_upgrade_activations`.
    ///
    /// The returned adapter does not advertise mempool surfaces. Pair the
    /// adapter with [`LightwalletdGrpcAdapter::with_mempool_surface`] to
    /// expose `GetMempoolStream` and `GetMempoolTx`; without one, both
    /// methods return `Status::unavailable`.
    #[must_use]
    pub fn new(
        query_api: QueryApi,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
    ) -> Self {
        Self::with_options(
            query_api,
            network_upgrade_activations,
            LightwalletdCompatibilityOptions::default(),
        )
    }

    /// Creates a lightwalletd-compatible gRPC adapter with explicit options.
    #[must_use]
    pub const fn with_options(
        query_api: QueryApi,
        network_upgrade_activations: Arc<NetworkUpgradeActivations>,
        options: LightwalletdCompatibilityOptions,
    ) -> Self {
        Self {
            query_api,
            options,
            mempool_surface: None,
            tip_change_watcher: None,
            network_upgrade_activations,
        }
    }

    /// Wires a mempool read surface into the adapter.
    #[must_use]
    pub fn with_mempool_surface(mut self, mempool_surface: SharedMempoolSurface) -> Self {
        self.mempool_surface = Some(mempool_surface);
        self
    }

    /// Wires a tip-change watcher into the adapter so `GetMempoolStream`
    /// closes cleanly on each best-chain tip change, preserving the
    /// lightwalletd Go server's de-facto contract.
    #[must_use]
    pub fn with_tip_change_watcher(mut self, watcher: SharedTipChangeWatcher) -> Self {
        self.tip_change_watcher = Some(watcher);
        self
    }

    /// Wraps this adapter in the generated tonic server type.
    #[must_use]
    pub fn into_server(self) -> compact_tx_streamer_server::CompactTxStreamerServer<Self>
    where
        Self: compact_tx_streamer_server::CompactTxStreamer,
    {
        compact_tx_streamer_server::CompactTxStreamerServer::new(self)
            .max_decoding_message_size(zinder_runtime::MAX_DECODING_MESSAGE_BYTES)
    }
}

impl<QueryApi> LightwalletdGrpcAdapter<QueryApi>
where
    QueryApi: WalletQueryApi + Send + Sync + 'static,
{
    /// Resolves the visible chain epoch id at call time.
    ///
    /// Every lightwalletd handler pins all of its downstream canonical reads
    /// to one epoch so a single logical response cannot straddle a reorg.
    /// This resolves that epoch from the visible tip once at handler entry.
    async fn entry_chain_epoch_id(&self) -> Result<ChainEpochId, Status> {
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        Ok(latest_block.chain_epoch.id)
    }

    /// Resolves a lightwalletd `BlockId` to a typed [`BlockHeight`].
    ///
    /// `BlockId { height, hash }` accepts three legacy shapes: height-only
    /// (`hash` empty), hash-only (`height = 0` with `hash` populated), and
    /// height-with-hash (both populated; the hash is verified after the read).
    /// Hash-only resolves through the canonical best-chain
    /// [`BlockSelector`] resolver, pinned to `at_epoch_id`; height-zero with
    /// empty hash returns `Status::not_found`.
    async fn resolve_block_height(
        &self,
        block_id: &lightwalletd::BlockId,
        at_epoch_id: ChainEpochId,
    ) -> Result<BlockHeight, Status> {
        let height_zero = block_id.height == 0;
        if !block_id.hash.is_empty() && height_zero {
            let hash_bytes: [u8; 32] = block_id
                .hash
                .as_slice()
                .try_into()
                .map_err(|_| Status::invalid_argument("block hash must be 32 bytes"))?;
            let resolved = self
                .query_api
                .block_id_by_selector(
                    BlockSelector::Hash(BlockHash::from_bytes(hash_bytes)),
                    Some(at_epoch_id),
                )
                .await
                .map_err(|error| status_from_query_error(&error))?;
            return Ok(resolved.block_id.height);
        }
        block_height_from_id(block_id)
    }

    /// Reads one compact block for `block_id`, pinning the height resolution
    /// and the block read to a single chain epoch.
    async fn block_at_epoch(
        &self,
        block_id: &lightwalletd::BlockId,
        at_epoch_id: ChainEpochId,
    ) -> Result<lightwalletd::CompactBlock, Status> {
        let height = self.resolve_block_height(block_id, at_epoch_id).await?;
        let compact_block = self
            .query_api
            .compact_block_at(height, Some(at_epoch_id))
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let compact_block = decode_compact_block(&compact_block.compact_block.payload_bytes)?;

        if !block_id.hash.is_empty() && block_id.hash != compact_block.hash {
            return Err(Status::not_found(
                "requested block hash does not match indexed block",
            ));
        }

        Ok(compact_block)
    }

    async fn address_utxos(
        &self,
        request: lightwalletd::GetAddressUtxosArg,
    ) -> Result<Vec<lightwalletd::GetAddressUtxosReply>, Status> {
        if request.addresses.is_empty() {
            return Ok(Vec::new());
        }

        let start_height = u32::try_from(request.start_height)
            .map_err(|_| Status::invalid_argument("startHeight exceeds u32"))?;
        let max_entries =
            NonZeroU32::new(request.max_entries).unwrap_or(self.options.max_address_utxos);
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let mut replies = Vec::new();

        for address in request.addresses {
            let query_request = transparent_address_unspent_outputs_request(
                &address,
                latest_block.chain_epoch.network,
                BlockHeight::new(start_height),
            )?;
            let address_utxos = self
                .query_api
                .transparent_address_unspent_outputs(
                    query_request,
                    Some(latest_block.chain_epoch.id),
                )
                .await
                .map_err(|error| status_from_query_error(&error))?;
            replies.extend(lightwalletd_address_utxos(&address, &address_utxos)?);
        }

        replies.sort_by(|left, right| {
            left.height
                .cmp(&right.height)
                .then_with(|| left.txid.cmp(&right.txid))
                .then_with(|| left.index.cmp(&right.index))
                .then_with(|| left.address.cmp(&right.address))
        });
        replies.truncate(u32_to_usize(max_entries.get()));
        Ok(replies)
    }

    /// Returns lightwalletd `Balance { value_zat: int64 }` from the wallet
    /// plane's transparent-address balance primitive.
    ///
    /// The legacy proto carries one signed `value_zat` field that wallets
    /// interpret as confirmed balance. The compat shim projects the native
    /// balance (confirmed total minus pending mempool outflows, saturating to
    /// zero) into that single field via
    /// [`TransparentAddressBalance::projected_total_zat`], capping at the
    /// `int64` wire ceiling.
    async fn compat_balance_response(
        &self,
        addresses: Vec<String>,
    ) -> Result<Response<lightwalletd::Balance>, Status> {
        if addresses.is_empty() {
            return Err(Status::invalid_argument("addresses list must not be empty"));
        }
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let mut script_hashes = Vec::with_capacity(addresses.len());
        for address in addresses {
            let address_script_hash = address_lookup_to_script_hash(
                Some(wallet_proto::AddressLookup {
                    selector: Some(address_lookup::Selector::Address(address)),
                }),
                latest_block.chain_epoch.network,
            )
            .map_err(|error| status_from_query_error(&error))?;
            script_hashes.push(address_script_hash);
        }
        let balance = self
            .query_api
            .transparent_address_balance(script_hashes, Some(latest_block.chain_epoch.id))
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let value_zat = i64::try_from(balance.projected_total_zat()).map_err(|_| {
            Status::out_of_range(
                "transparent address balance exceeds i64::MAX; \
                 use the native WalletQuery.TransparentAddressBalance surface",
            )
        })?;
        Ok(Response::new(lightwalletd::Balance { value_zat }))
    }
}

#[tonic::async_trait]
impl<QueryApi> compact_tx_streamer_server::CompactTxStreamer for LightwalletdGrpcAdapter<QueryApi>
where
    QueryApi: WalletQueryApi + Send + Sync + 'static,
{
    async fn get_latest_block(
        &self,
        _request: Request<lightwalletd::ChainSpec>,
    ) -> Result<Response<lightwalletd::BlockId>, Status> {
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(lightwalletd::BlockId {
            height: u64::from(latest_block.height.value()),
            hash: encode_internal_block_hash(latest_block.block_hash).to_vec(),
        }))
    }

    async fn get_block(
        &self,
        request: Request<lightwalletd::BlockId>,
    ) -> Result<Response<lightwalletd::CompactBlock>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let block_id = request.into_inner();
        Ok(Response::new(
            self.block_at_epoch(&block_id, at_epoch_id).await?,
        ))
    }

    async fn get_block_nullifiers(
        &self,
        request: Request<lightwalletd::BlockId>,
    ) -> Result<Response<lightwalletd::CompactBlock>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let block_id = request.into_inner();
        let compact_block = self.block_at_epoch(&block_id, at_epoch_id).await?;
        Ok(Response::new(prune_compact_block(
            compact_block,
            CompactBlockPoolSelection::shielded(),
            CompactBlockPayloadMode::NullifiersOnly,
        )))
    }

    type GetBlockRangeStream = GrpcStream<lightwalletd::CompactBlock>;

    async fn get_block_range(
        &self,
        request: Request<lightwalletd::BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let block_range_request = request.into_inner();
        let pool_selection = pool_selection_from_request(&block_range_request.pool_types)?;
        let (block_range, is_descending) = block_range_from_request(&block_range_request)?;
        let compact_block_range = self
            .query_api
            .compact_blocks_in_range(block_range, Some(at_epoch_id))
            .await
            .map_err(|error| status_from_query_error(&error))?;
        Ok(Response::new(stream_compact_blocks(
            compact_block_range.compact_blocks,
            is_descending,
            pool_selection,
            CompactBlockPayloadMode::Full,
        )))
    }

    type GetBlockRangeNullifiersStream = GrpcStream<lightwalletd::CompactBlock>;

    async fn get_block_range_nullifiers(
        &self,
        request: Request<lightwalletd::BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeNullifiersStream>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let block_range_request = request.into_inner();
        let (block_range, is_descending) = block_range_from_request(&block_range_request)?;
        let compact_block_range = self
            .query_api
            .compact_blocks_in_range(block_range, Some(at_epoch_id))
            .await
            .map_err(|error| status_from_query_error(&error))?;
        Ok(Response::new(stream_compact_blocks(
            compact_block_range.compact_blocks,
            is_descending,
            CompactBlockPoolSelection::shielded(),
            CompactBlockPayloadMode::NullifiersOnly,
        )))
    }

    async fn get_transaction(
        &self,
        request: Request<lightwalletd::TxFilter>,
    ) -> Result<Response<lightwalletd::RawTransaction>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let filter = request.into_inner();

        let location = if let Some(block_id) = filter.block.as_ref() {
            let height = self.resolve_block_height(block_id, at_epoch_id).await?;
            let response = self
                .query_api
                .transaction_at_block_index(height, filter.index, Some(at_epoch_id))
                .await
                .map_err(|error| status_from_query_error(&error))?;
            response.transaction
        } else {
            let transaction_id = decode_internal_transaction_id(&filter.hash)
                .map_err(|error| wire_decode_error_to_status(&error))?;
            let response = self
                .query_api
                .transaction(transaction_id, Some(at_epoch_id))
                .await
                .map_err(|error| status_from_query_error(&error))?;
            mined_location_from_status(response.status)?
        };

        Ok(Response::new(
            raw_transaction_from_location(&self.query_api, at_epoch_id, location).await?,
        ))
    }

    async fn send_transaction(
        &self,
        request: Request<lightwalletd::RawTransaction>,
    ) -> Result<Response<lightwalletd::SendResponse>, Status> {
        let raw_transaction = RawTransactionBytes::new(request.into_inner().data);
        let broadcast_result = self
            .query_api
            .broadcast_transaction(raw_transaction)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        Ok(Response::new(send_response_from_broadcast_result(
            broadcast_result,
        )))
    }

    type GetTaddressTxidsStream = GrpcStream<lightwalletd::RawTransaction>;

    async fn get_taddress_txids(
        &self,
        request: Request<lightwalletd::TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTxidsStream>, Status> {
        let filter = request.into_inner();
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let typed_request =
            transparent_address_tx_history_request(&filter, latest_block.chain_epoch.network)?;
        let history = transparent_address_tx_history(&self.query_api, typed_request).await?;
        let raw_transactions = history.into_iter().map(|artifact| {
            Ok(lightwalletd::RawTransaction {
                data: encode_internal_transaction_id(artifact.transaction_id).to_vec(),
                height: u64::from(artifact.block_height.value()),
            })
        });
        Ok(Response::new(Box::pin(stream::iter(raw_transactions))))
    }

    type GetTaddressTransactionsStream = GrpcStream<lightwalletd::RawTransaction>;

    async fn get_taddress_transactions(
        &self,
        request: Request<lightwalletd::TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTransactionsStream>, Status> {
        let filter = request.into_inner();
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let at_epoch_id = latest_block.chain_epoch.id;
        let typed_request =
            transparent_address_tx_history_request(&filter, latest_block.chain_epoch.network)?;
        let history = transparent_address_tx_history(&self.query_api, typed_request).await?;
        let mut raw_transactions = Vec::with_capacity(history.len());
        for artifact in history {
            let response = self
                .query_api
                .transaction(artifact.transaction_id, Some(at_epoch_id))
                .await
                .map_err(|error| status_from_query_error(&error))?;
            let location = mined_location_from_status(response.status)?;
            raw_transactions
                .push(raw_transaction_from_location(&self.query_api, at_epoch_id, location).await);
        }
        Ok(Response::new(Box::pin(stream::iter(raw_transactions))))
    }

    async fn get_taddress_balance(
        &self,
        request: Request<lightwalletd::AddressList>,
    ) -> Result<Response<lightwalletd::Balance>, Status> {
        let addresses = request.into_inner().addresses;
        self.compat_balance_response(addresses).await
    }

    async fn get_taddress_balance_stream(
        &self,
        request: Request<tonic::Streaming<lightwalletd::Address>>,
    ) -> Result<Response<lightwalletd::Balance>, Status> {
        let mut stream = request.into_inner();
        let mut addresses: Vec<String> = Vec::new();
        while let Some(received) = stream.next().await {
            let address = received?;
            addresses.push(address.address);
        }
        self.compat_balance_response(addresses).await
    }

    type GetMempoolTxStream = GrpcStream<lightwalletd::CompactTx>;

    async fn get_mempool_tx(
        &self,
        request: Request<lightwalletd::GetMempoolTxRequest>,
    ) -> Result<Response<Self::GetMempoolTxStream>, Status> {
        let mempool_surface = self
            .mempool_surface
            .as_ref()
            .ok_or_else(|| Status::unavailable("mempool surface is not configured"))?
            .clone();
        let request = request.into_inner();
        let exclude_txid_suffixes = request.exclude_txid_suffixes;
        let pool_selection = pool_selection_from_request(&request.pool_types)?;
        let first_page = mempool_surface
            .mempool_snapshot_page(LIGHTWALLETD_MEMPOOL_SNAPSHOT_PAGE_SIZE, None)
            .await
            .map_err(status_from_mempool_surface_error)?;
        let mut entries = mempool_snapshot_entries(mempool_surface, first_page);
        let mut compact_messages = Vec::new();
        while let Some(entry_outcome) = entries.next().await {
            let entry = entry_outcome?;
            if txid_matches_excluded_suffix(
                &encode_internal_transaction_id(entry.transaction_id),
                &exclude_txid_suffixes,
            ) {
                continue;
            }
            let compact_message =
                lightwalletd::CompactTx::decode(entry.compact_transaction_bytes.as_slice())
                    .map_err(|error| {
                        Status::data_loss(format!(
                            "stored compact transaction bytes failed to decode: {error}"
                        ))
                    })?;
            let pruned = prune_compact_transaction(
                compact_message,
                pool_selection,
                CompactBlockPayloadMode::Full,
            );
            if !compact_transaction_has_payload(&pruned) {
                continue;
            }
            compact_messages.push(Ok(pruned));
        }
        Ok(Response::new(Box::pin(stream::iter(compact_messages))))
    }

    type GetMempoolStreamStream = GrpcStream<lightwalletd::RawTransaction>;

    async fn get_mempool_stream(
        &self,
        _request: Request<lightwalletd::Empty>,
    ) -> Result<Response<Self::GetMempoolStreamStream>, Status> {
        let mempool_surface = self
            .mempool_surface
            .as_ref()
            .ok_or_else(|| Status::unavailable("mempool surface is not configured"))?
            .clone();
        // Deliver the current mempool contents from the snapshot walk, then
        // continue with events strictly after the walk's resume anchor. The
        // anchor guarantees at-least-once delivery; a transaction admitted
        // mid-walk may appear both in a later page and as an event.
        let first_page = mempool_surface
            .mempool_snapshot_page(LIGHTWALLETD_MEMPOOL_SNAPSHOT_PAGE_SIZE, None)
            .await
            .map_err(status_from_mempool_surface_error)?;
        let event_stream = mempool_surface
            .mempool_events(first_page.events_resume_cursor.clone())
            .await
            .map_err(status_from_mempool_surface_error)?;
        let snapshot_raw_transactions = mempool_snapshot_entries(mempool_surface, first_page)
            .map(|entry_outcome| entry_outcome.map(|entry| mempool_raw_transaction(&entry)));
        let raw_transaction_stream = snapshot_raw_transactions.chain(
            stream::StreamExt::filter_map(event_stream, project_added_to_raw_transaction),
        );

        let bounded_stream: GrpcStream<lightwalletd::RawTransaction> =
            if let Some(watcher) = self.tip_change_watcher.clone() {
                Box::pin(close_mempool_stream_on_tip_change(
                    raw_transaction_stream,
                    watcher,
                ))
            } else {
                Box::pin(raw_transaction_stream)
            };
        Ok(Response::new(bounded_stream))
    }

    async fn get_tree_state(
        &self,
        request: Request<lightwalletd::BlockId>,
    ) -> Result<Response<lightwalletd::TreeState>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let block_id = request.into_inner();
        let height = self.resolve_block_height(&block_id, at_epoch_id).await?;
        let tree_state = self
            .query_api
            .tree_state_at(height, Some(at_epoch_id))
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(lightwalletd_tree_state(&tree_state)?))
    }

    async fn get_latest_tree_state(
        &self,
        _request: Request<lightwalletd::Empty>,
    ) -> Result<Response<lightwalletd::TreeState>, Status> {
        let tree_state = self
            .query_api
            .latest_tree_state_checkpoint(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(lightwalletd_tree_state(&tree_state)?))
    }

    type GetSubtreeRootsStream = GrpcStream<lightwalletd::SubtreeRoot>;

    async fn get_subtree_roots(
        &self,
        request: Request<lightwalletd::GetSubtreeRootsArg>,
    ) -> Result<Response<Self::GetSubtreeRootsStream>, Status> {
        let at_epoch_id = self.entry_chain_epoch_id().await?;
        let request = request.into_inner();
        let protocol = shielded_protocol_from_request(request.shielded_protocol)?;
        let max_entries =
            NonZeroU32::new(request.max_entries).unwrap_or(self.options.max_subtree_roots);
        let subtree_roots = self
            .query_api
            .subtree_roots(
                SubtreeRootRange::new(
                    protocol,
                    SubtreeRootIndex::new(request.start_index),
                    max_entries,
                ),
                Some(at_epoch_id),
            )
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(stream_items(lightwalletd_subtree_roots(
            &subtree_roots,
        ))))
    }

    async fn get_address_utxos(
        &self,
        request: Request<lightwalletd::GetAddressUtxosArg>,
    ) -> Result<Response<lightwalletd::GetAddressUtxosReplyList>, Status> {
        let replies = self.address_utxos(request.into_inner()).await?;
        Ok(Response::new(lightwalletd::GetAddressUtxosReplyList {
            address_utxos: replies,
        }))
    }

    type GetAddressUtxosStreamStream = GrpcStream<lightwalletd::GetAddressUtxosReply>;

    async fn get_address_utxos_stream(
        &self,
        request: Request<lightwalletd::GetAddressUtxosArg>,
    ) -> Result<Response<Self::GetAddressUtxosStreamStream>, Status> {
        let replies = self.address_utxos(request.into_inner()).await?;
        Ok(Response::new(stream_items(replies)))
    }

    async fn get_lightd_info(
        &self,
        _request: Request<lightwalletd::Empty>,
    ) -> Result<Response<lightwalletd::LightdInfo>, Status> {
        let activations = self.network_upgrade_activations.as_ref();
        let latest_block = self
            .query_api
            .latest_block(None)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        if latest_block.chain_epoch.network != activations.network() {
            return Err(Status::failed_precondition(
                "network upgrade activations do not match the chain epoch network",
            ));
        }

        Ok(Response::new(lightd_info(activations, latest_block.height)))
    }

    async fn ping(
        &self,
        _request: Request<lightwalletd::Duration>,
    ) -> Result<Response<lightwalletd::PingResponse>, Status> {
        Ok(Response::new(lightwalletd::PingResponse {
            entry: 0,
            exit: 0,
        }))
    }
}

async fn transparent_address_tx_history<QueryApi>(
    query_api: &QueryApi,
    mut request: TransparentAddressTxIdsInRangeRequest,
) -> Result<Vec<TransparentAddressTxIndexArtifact>, Status>
where
    QueryApi: WalletQueryApi + ?Sized,
{
    let mut artifacts = Vec::new();
    loop {
        let response = query_api
            .transparent_address_tx_ids_in_range(request.clone())
            .await
            .map_err(|error| status_from_query_error(&error))?;
        artifacts.extend(response.artifacts);
        let Some(next_cursor) = response.next_cursor else {
            return Ok(artifacts);
        };
        request.from_cursor = Some(next_cursor);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompactBlockPoolSelection {
    sapling: bool,
    orchard: bool,
    transparent: bool,
}

impl CompactBlockPoolSelection {
    const fn shielded() -> Self {
        Self {
            sapling: true,
            orchard: true,
            transparent: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactBlockPayloadMode {
    Full,
    NullifiersOnly,
}

fn stream_items<T: Send + 'static>(items: Vec<T>) -> GrpcStream<T> {
    Box::pin(stream::iter(items.into_iter().map(Ok)))
}

fn stream_compact_blocks(
    compact_blocks: Vec<CompactBlockArtifact>,
    is_descending: bool,
    pool_selection: CompactBlockPoolSelection,
    payload_mode: CompactBlockPayloadMode,
) -> GrpcStream<lightwalletd::CompactBlock> {
    let compact_blocks: Box<dyn Iterator<Item = CompactBlockArtifact> + Send> = if is_descending {
        Box::new(compact_blocks.into_iter().rev())
    } else {
        Box::new(compact_blocks.into_iter())
    };

    Box::pin(stream::iter(compact_blocks.map(move |compact_block| {
        decode_compact_block(&compact_block.payload_bytes)
            .map(|compact_block| prune_compact_block(compact_block, pool_selection, payload_mode))
    })))
}

fn decode_compact_block(payload_bytes: &[u8]) -> Result<lightwalletd::CompactBlock, Status> {
    lightwalletd::CompactBlock::decode(payload_bytes).map_err(|source| {
        Status::data_loss(format!(
            "indexed compact block payload is not a lightwalletd CompactBlock: {source}"
        ))
    })
}

fn prune_compact_transaction(
    mut transaction: lightwalletd::CompactTx,
    pool_selection: CompactBlockPoolSelection,
    payload_mode: CompactBlockPayloadMode,
) -> lightwalletd::CompactTx {
    if !pool_selection.sapling || payload_mode == CompactBlockPayloadMode::NullifiersOnly {
        transaction.outputs.clear();
    }
    if !pool_selection.sapling {
        transaction.spends.clear();
    }
    if !pool_selection.orchard {
        transaction.actions.clear();
    } else if payload_mode == CompactBlockPayloadMode::NullifiersOnly {
        for action in &mut transaction.actions {
            action.cmx.clear();
            action.ephemeral_key.clear();
            action.ciphertext.clear();
        }
    }
    if !pool_selection.transparent || payload_mode == CompactBlockPayloadMode::NullifiersOnly {
        transaction.vin.clear();
        transaction.vout.clear();
    }
    transaction
}

fn prune_compact_block(
    mut compact_block: lightwalletd::CompactBlock,
    pool_selection: CompactBlockPoolSelection,
    payload_mode: CompactBlockPayloadMode,
) -> lightwalletd::CompactBlock {
    // The nullifiers-only contract excludes commitment tree sizes: the
    // GetBlockNullifiers / GetBlockRangeNullifiers responses must not leak the
    // witness-construction tree sizes carried in chain_metadata.
    if payload_mode == CompactBlockPayloadMode::NullifiersOnly {
        compact_block.chain_metadata = None;
    }
    compact_block.vtx = compact_block
        .vtx
        .into_iter()
        .map(|transaction| prune_compact_transaction(transaction, pool_selection, payload_mode))
        .filter(compact_transaction_has_payload)
        .collect();
    compact_block
}

fn compact_transaction_has_payload(transaction: &lightwalletd::CompactTx) -> bool {
    !transaction.spends.is_empty()
        || !transaction.outputs.is_empty()
        || !transaction.actions.is_empty()
        || !transaction.vin.is_empty()
        || !transaction.vout.is_empty()
}

fn pool_selection_from_request(pool_types: &[i32]) -> Result<CompactBlockPoolSelection, Status> {
    if pool_types.is_empty() {
        return Ok(CompactBlockPoolSelection::shielded());
    }

    let mut pool_selection = CompactBlockPoolSelection {
        sapling: false,
        orchard: false,
        transparent: false,
    };
    for pool_type in pool_types {
        match lightwalletd::PoolType::try_from(*pool_type) {
            Ok(lightwalletd::PoolType::Sapling) => pool_selection.sapling = true,
            Ok(lightwalletd::PoolType::Orchard) => pool_selection.orchard = true,
            Ok(lightwalletd::PoolType::Transparent) => pool_selection.transparent = true,
            Ok(lightwalletd::PoolType::Invalid) | Err(_) => {
                return Err(Status::invalid_argument(
                    "poolTypes contains an unknown pool",
                ));
            }
        }
    }

    Ok(pool_selection)
}

fn block_range_from_request(
    request: &lightwalletd::BlockRange,
) -> Result<(BlockHeightRange, bool), Status> {
    let start = request
        .start
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("range.start is required"))
        .and_then(block_height_from_id)?;
    let end = request
        .end
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("range.end is required"))
        .and_then(block_height_from_id)?;
    let is_descending = start > end;
    let block_range = if is_descending {
        BlockHeightRange::inclusive(end, start)
    } else {
        BlockHeightRange::inclusive(start, end)
    };

    Ok((block_range, is_descending))
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TxStatus is #[non_exhaustive]; non-mined statuses map to gRPC NOT_FOUND on the lightwalletd wire because lightwalletd's RawTransaction shape is mined-only."
)]
fn mined_location_from_status(status: TxStatus) -> Result<TransactionLocation, Status> {
    match status {
        TxStatus::Mined(mined) => Ok(mined.location),
        TxStatus::NotFound | TxStatus::InMempool(_) | TxStatus::ConflictingChain => Err(
            Status::not_found("transaction is not mined in the canonical chain"),
        ),
        _ => Err(Status::not_found(
            "transaction status is not representable on the lightwalletd wire",
        )),
    }
}

async fn raw_transaction_from_location<Q: WalletQueryApi + ?Sized>(
    query_api: &Q,
    chain_epoch_id: ChainEpochId,
    location: TransactionLocation,
) -> Result<lightwalletd::RawTransaction, Status> {
    let raw_transaction = query_api
        .raw_transaction(location.transaction_id, Some(chain_epoch_id))
        .await
        .map_err(|error| status_from_query_error(&error))?;
    if raw_transaction.transaction.location != location {
        return Err(Status::data_loss(
            "stored raw transaction location does not match the indexed transaction location",
        ));
    }
    Ok(lightwalletd::RawTransaction {
        data: raw_transaction.transaction.raw_transaction_bytes,
        height: u64::from(location.block_height.value()),
    })
}

fn block_height_from_id(block_id: &lightwalletd::BlockId) -> Result<BlockHeight, Status> {
    let height = u32::try_from(block_id.height)
        .map_err(|_| Status::invalid_argument("block height exceeds u32"))?;
    if height == 0 {
        if !block_id.hash.is_empty() {
            return Err(Status::invalid_argument(
                "hash-only block lookups must be resolved through resolve_block_height",
            ));
        }
        return Err(Status::not_found("block height 0 is not indexed"));
    }

    Ok(BlockHeight::new(height))
}

/// Convert a [`WireDecodeError`] into a tonic [`Status`] for the
/// lightwalletd-compatibility surface.
///
/// Length and hex-decoding failures map to `INVALID_ARGUMENT` (the caller
/// supplied a malformed byte or string field). Enum and dialect-string
/// failures use the same code: the caller's wire input did not match any
/// known value for the surface they targeted.
fn wire_decode_error_to_status(error: &WireDecodeError) -> Status {
    Status::invalid_argument(error.to_string())
}

fn shielded_protocol_from_request(protocol: i32) -> Result<ShieldedProtocol, Status> {
    match lightwalletd::ShieldedProtocol::try_from(protocol) {
        Ok(lightwalletd::ShieldedProtocol::Sapling) => Ok(ShieldedProtocol::Sapling),
        Ok(lightwalletd::ShieldedProtocol::Orchard) => Ok(ShieldedProtocol::Orchard),
        Err(_) => Err(Status::invalid_argument("shieldedProtocol is unknown")),
    }
}

fn transparent_address_tx_history_request(
    filter: &lightwalletd::TransparentAddressBlockFilter,
    network: Network,
) -> Result<TransparentAddressTxIdsInRangeRequest, Status> {
    let address_text = filter.address.as_str();
    let zebra_address: ZebraTransparentAddress = address_text.parse().map_err(|source| {
        Status::invalid_argument(format!("transparent address is invalid: {source}"))
    })?;
    if !transparent_address_matches_network(zebra_address.network_kind(), network) {
        return Err(Status::invalid_argument(
            "transparent address network does not match server network",
        ));
    }
    let script_pub_key = zebra_address.script().as_raw_bytes().to_vec();
    if script_pub_key.is_empty() {
        return Err(Status::invalid_argument(
            "transparent address does not produce a receivable script",
        ));
    }
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    let range = filter
        .range
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("range is required"))?;
    let start_block = range
        .start
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("range.start is required"))?;
    let end_block = range
        .end
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("range.end is required"))?;
    let start_height = block_height_from_id(start_block)?;
    let end_height = block_height_from_id(end_block)?;
    if start_height > end_height {
        return Err(Status::invalid_argument(
            "range.start.height must not exceed range.end.height",
        ));
    }
    Ok(TransparentAddressTxIdsInRangeRequest {
        address_script_hash,
        start_height,
        end_height,
        max_entries: NonZeroU32::MIN.saturating_add(999),
        from_cursor: None,
        descending: false,
    })
}

fn transparent_address_unspent_outputs_request(
    address: &str,
    network: Network,
    start_height: BlockHeight,
) -> Result<TransparentAddressUnspentOutputsRequest, Status> {
    let transparent_address = address
        .parse::<ZebraTransparentAddress>()
        .map_err(|source| {
            Status::invalid_argument(format!("transparent address is invalid: {source}"))
        })?;
    if !transparent_address_matches_network(transparent_address.network_kind(), network) {
        return Err(Status::invalid_argument(
            "transparent address network does not match server network",
        ));
    }

    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    if script_pub_key.is_empty() {
        return Err(Status::invalid_argument(
            "transparent address does not produce a receivable script",
        ));
    }

    Ok(TransparentAddressUnspentOutputsRequest {
        address_script_hash: TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
        start_height,
    })
}

fn lightwalletd_address_utxos(
    address: &str,
    address_utxos: &TransparentAddressUnspentOutputs,
) -> Result<Vec<lightwalletd::GetAddressUtxosReply>, Status> {
    address_utxos
        .outputs
        .iter()
        .map(|utxo| {
            Ok(lightwalletd::GetAddressUtxosReply {
                address: address.to_owned(),
                txid: encode_internal_transaction_id(utxo.outpoint.transaction_id).to_vec(),
                index: i32::try_from(utxo.outpoint.output_index)
                    .map_err(|_| Status::data_loss("transparent output index exceeds i32"))?,
                script: utxo.script_pub_key.clone(),
                value_zat: i64::try_from(utxo.value_zat)
                    .map_err(|_| Status::data_loss("transparent output value exceeds i64"))?,
                height: u64::from(utxo.block_height.value()),
            })
        })
        .collect()
}

fn lightwalletd_tree_state(tree_state: &TreeState) -> Result<lightwalletd::TreeState, Status> {
    let payload: Value = serde_json::from_slice(&tree_state.payload_bytes).map_err(|source| {
        Status::data_loss(format!("indexed tree-state payload is not JSON: {source}"))
    })?;

    Ok(lightwalletd::TreeState {
        network: encode_bip70_chain_name(tree_state.chain_epoch.network).to_owned(),
        height: u64::from(tree_state.height.value()),
        hash: encode_rpc_block_hash_hex(tree_state.block_hash),
        time: tree_state_time(&payload)?,
        sapling_tree: tree_state_pool_final_state(&payload, "sapling")?,
        orchard_tree: tree_state_pool_final_state(&payload, "orchard")?,
    })
}

fn tree_state_time(payload: &Value) -> Result<u32, Status> {
    let Some(time_value) = payload.get("time") else {
        return Err(Status::data_loss("tree-state payload is missing time"));
    };
    let Some(time) = time_value.as_u64() else {
        return Err(Status::data_loss(
            "tree-state time must be a non-negative integer",
        ));
    };

    u32::try_from(time).map_err(|_| Status::data_loss("tree-state time exceeds u32"))
}

fn tree_state_pool_final_state(payload: &Value, pool_name: &'static str) -> Result<String, Status> {
    let Some(pool) = payload.get(pool_name) else {
        return Ok(String::new());
    };
    let Some(pool_fields) = pool.as_object() else {
        return Err(Status::data_loss(format!(
            "{pool_name} tree-state pool must be a JSON object"
        )));
    };
    let Some(commitments) = pool_fields.get("commitments") else {
        return Ok(String::new());
    };

    if let Some(final_state) = commitments.get("finalState").and_then(Value::as_str) {
        return Ok(final_state.to_owned());
    }

    match commitments {
        Value::Object(fields) if fields.is_empty() => Ok(String::new()),
        Value::Object(_) => Err(Status::data_loss(format!(
            "{pool_name} tree-state commitments are missing finalState"
        ))),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            Err(Status::data_loss(format!(
                "{pool_name} tree-state commitments must be a JSON object"
            )))
        }
    }
}

fn lightwalletd_subtree_roots(subtree_roots: &SubtreeRoots) -> Vec<lightwalletd::SubtreeRoot> {
    subtree_roots
        .subtree_roots
        .iter()
        .map(|subtree_root| lightwalletd::SubtreeRoot {
            root_hash: subtree_root.root_hash.as_bytes().to_vec(),
            completing_block_hash: encode_internal_block_hash(subtree_root.completing_block_hash)
                .to_vec(),
            completing_block_height: u64::from(subtree_root.completing_block_height.value()),
        })
        .collect()
}

fn lightd_info(
    activations: &NetworkUpgradeActivations,
    tip_height: BlockHeight,
) -> lightwalletd::LightdInfo {
    let current = activations.active_at(tip_height);
    let consensus_branch_id = current.map_or_else(
        || "00000000".to_owned(),
        |activation| encode_branch_id_hex(activation.branch_id),
    );
    let upgrade_name = current
        .map(|activation| activation.name.clone())
        .unwrap_or_default();
    let upgrade_height = current.map_or(0, |activation| {
        u64::from(activation.activation_height.value())
    });
    let sapling_activation_height = activations
        .activation_height_by_name("Sapling")
        .map_or(0, |height| u64::from(height.value()));

    lightwalletd::LightdInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        vendor: "Zinder".to_owned(),
        taddr_support: true,
        chain_name: encode_bip70_chain_name(activations.network()).to_owned(),
        sapling_activation_height,
        consensus_branch_id,
        block_height: u64::from(tip_height.value()),
        git_commit: option_env!("LIGHTWALLETD_COMPAT_BUILD_GIT_COMMIT")
            .unwrap_or_default()
            .to_owned(),
        branch: String::new(),
        build_date: String::new(),
        build_user: String::new(),
        estimated_height: u64::from(tip_height.value()),
        // Deliberately empty: Zinder is not zcashd; impersonating a build
        // version misleads operators inspecting the field.
        zcashd_build: String::new(),
        zcashd_subversion: String::new(),
        // Operator-configured in lightwalletd-go. Zinder has no donation
        // address config, so the empty string preserves the unset convention.
        donation_address: String::new(),
        upgrade_name,
        upgrade_height,
        lightwallet_protocol_version: LIGHTWALLETD_PROTOCOL_COMMIT.to_owned(),
    }
}

/// Maps a typed broadcast outcome to the lightwalletd `SendResponse` shape.
///
/// Stable error-code scheme so wallet clients can pattern-match without parsing
/// the message string:
///
/// * `0` accepted; `errorMessage` carries the node-reported transaction id hex.
/// * `-22` invalid encoding (Bitcoin/Zcash convention).
/// * `-26` rejected by node policy.
/// * `-27` already in mempool or chain (duplicate).
/// * `-1` unclassified node response.
///
/// When the node reports its own `error_code`, that code is forwarded
/// instead of the default; clients that already track node codes do not
/// need a Zinder-specific table.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non-exhaustive broadcast results from zinder-core must degrade conservatively"
)]
fn send_response_from_broadcast_result(
    broadcast_result: TransactionBroadcastResult,
) -> lightwalletd::SendResponse {
    match broadcast_result {
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id }) => {
            lightwalletd::SendResponse {
                error_code: 0,
                error_message: encode_rpc_transaction_id_hex(transaction_id),
            }
        }
        TransactionBroadcastResult::InvalidEncoding(BroadcastInvalidEncoding {
            error_code,
            message,
        }) => lightwalletd::SendResponse {
            error_code: classified_send_error_code(error_code, -22),
            error_message: message,
        },
        TransactionBroadcastResult::Rejected(BroadcastRejected {
            error_code,
            message,
            kind: _,
        }) => lightwalletd::SendResponse {
            error_code: classified_send_error_code(error_code, -26),
            error_message: message,
        },
        TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
            error_code,
            message,
        }) => lightwalletd::SendResponse {
            error_code: classified_send_error_code(error_code, -27),
            error_message: message,
        },
        TransactionBroadcastResult::Queued(BroadcastQueued { message }) => {
            // lightwalletd's SendResponse has no queued concept; surface
            // Zebra's underlying -25 Verify code so legacy wallets see the
            // same error code they would have received from zcashd while the
            // download queue drains.
            lightwalletd::SendResponse {
                error_code: -25,
                error_message: message,
            }
        }
        TransactionBroadcastResult::Unknown(BroadcastUnknown {
            error_code,
            message,
        }) => lightwalletd::SendResponse {
            error_code: classified_send_error_code(error_code, -1),
            error_message: message,
        },
        _ => lightwalletd::SendResponse {
            error_code: -1,
            error_message: "unclassified transaction broadcast response".to_owned(),
        },
    }
}

fn classified_send_error_code(reported_code: Option<i64>, default_code: i32) -> i32 {
    reported_code
        .and_then(|code| i32::try_from(code).ok())
        .unwrap_or(default_code)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non-exhaustive core networks must fail closed until lightwalletd mapping exists"
)]
fn close_mempool_stream_on_tip_change<S>(
    raw_transaction_stream: S,
    watcher: SharedTipChangeWatcher,
) -> tokio_stream::wrappers::ReceiverStream<Result<lightwalletd::RawTransaction, Status>>
where
    S: tonic::codegen::tokio_stream::Stream<Item = Result<lightwalletd::RawTransaction, Status>>
        + Send
        + 'static,
{
    let (output_sender, output_receiver) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        tokio::pin!(raw_transaction_stream);
        let mut tip_change_signal = Box::pin(watcher.await_tip_change());
        loop {
            tokio::select! {
                outcome = tonic::codegen::tokio_stream::StreamExt::next(&mut raw_transaction_stream) => {
                    match outcome {
                        Some(raw_transaction_outcome) => {
                            if output_sender.send(raw_transaction_outcome).await.is_err() {
                                return;
                            }
                        }
                        None => {
                            // Underlying mempool source ended; let the
                            // gRPC stream end naturally.
                            return;
                        }
                    }
                }
                tip_change_outcome = &mut tip_change_signal => {
                    match tip_change_outcome {
                        Ok(()) => {
                            tracing::debug!(
                                target: "zinder::compat_lightwalletd",
                                event = "mempool_stream_close_on_tip_change",
                                "GetMempoolStream closing on observed best-chain tip change"
                            );
                        }
                        Err(error @ TipChangeWatcherError::SignalClosed) => {
                            tracing::warn!(
                                target: "zinder::compat_lightwalletd",
                                event = "mempool_stream_tip_signal_closed",
                                error = %error,
                                "tip-change signal source closed; GetMempoolStream will end"
                            );
                        }
                    }
                    return;
                }
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(output_receiver)
}

/// Streams live-mempool entries page by page, starting with the
/// already-fetched first page and fetching later pages only as the consumer
/// pulls, so the walk never materializes the whole mempool.
fn mempool_snapshot_entries(
    mempool_surface: SharedMempoolSurface,
    first_page: MempoolSnapshotPage,
) -> tokio_stream::wrappers::ReceiverStream<Result<zinder_core::MempoolEntry, Status>> {
    let (entry_sender, entry_receiver) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let mut page = first_page;
        loop {
            for entry in page.entries {
                if entry_sender.send(Ok(entry)).await.is_err() {
                    return;
                }
            }
            let Some(cursor) = page.next_cursor else {
                return;
            };
            page = match mempool_surface
                .mempool_snapshot_page(LIGHTWALLETD_MEMPOOL_SNAPSHOT_PAGE_SIZE, Some(cursor))
                .await
            {
                Ok(next_page) => next_page,
                Err(error) => {
                    let _ = entry_sender
                        .send(Err(status_from_mempool_surface_error(error)))
                        .await;
                    return;
                }
            };
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(entry_receiver)
}

fn status_from_mempool_surface_error(error: MempoolSurfaceError) -> Status {
    match error {
        MempoolSurfaceError::Unavailable { reason } => Status::unavailable(reason),
        MempoolSurfaceError::CursorInvalid => Status::invalid_argument("mempool cursor is invalid"),
        MempoolSurfaceError::CursorExpired => Status::failed_precondition("mempool cursor expired"),
    }
}

fn txid_matches_excluded_suffix(transaction_id: &[u8; 32], exclude_suffixes: &[Vec<u8>]) -> bool {
    exclude_suffixes
        .iter()
        .any(|suffix| !suffix.is_empty() && transaction_id.ends_with(suffix))
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "MempoolEvent is #[non_exhaustive]; the lightwalletd compat shim only projects Added events into RawTransaction, so all current and future non-Added variants are filtered out of the GetMempoolStream projection."
)]
fn project_added_to_raw_transaction(
    event_outcome: Result<zinder_store::MempoolEventEnvelope, MempoolSurfaceError>,
) -> Option<Result<lightwalletd::RawTransaction, Status>> {
    match event_outcome {
        Ok(envelope) => match envelope.event {
            MempoolEvent::Added { entry } => Some(Ok(mempool_raw_transaction(&entry))),
            _ => None,
        },
        Err(error) => Some(Err(status_from_mempool_surface_error(error))),
    }
}

fn mempool_raw_transaction(entry: &zinder_core::MempoolEntry) -> lightwalletd::RawTransaction {
    lightwalletd::RawTransaction {
        data: entry.raw_transaction_bytes.as_slice().to_vec(),
        // Lightwalletd contract: the height field on a mempool raw
        // transaction is "the latest block height" at the moment the
        // transaction was observed. The Android SDK ignores the value and
        // uses GetLatestBlock for tip tracking; Zinder reports the chain
        // epoch's tip height recorded on the entry.
        height: u64::from(entry.first_seen_chain_epoch.visible_tip_height.value()),
    }
}
