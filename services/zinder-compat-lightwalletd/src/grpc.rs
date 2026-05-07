//! gRPC implementation for the vendored lightwalletd protocol.

use std::{num::NonZeroU32, pin::Pin};

use prost::Message;
use serde_json::Value;
use tokio_stream::{self as stream};
use tonic::{Request, Response, Status};
use zebra_chain::{
    block::Height as ZebraBlockHeight,
    parameters::{
        Network as ZebraNetwork, NetworkKind as ZebraNetworkKind, NetworkUpgrade,
        testnet::RegtestParameters,
    },
    transparent::Address as ZebraTransparentAddress,
};
use zinder_core::{
    BlockHeight, BlockHeightRange, BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding,
    BroadcastRejected, BroadcastUnknown, CompactBlockArtifact, Network, RawTransactionBytes,
    ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange, TransactionBroadcastResult,
    TransactionId,
};
use zinder_proto::compat::lightwalletd::{
    self, LIGHTWALLETD_PROTOCOL_COMMIT, compact_tx_streamer_server,
};
use zinder_query::{
    SubtreeRoots, TransparentAddressUtxos, TransparentAddressUtxosRequest, TreeState,
    WalletQueryApi, status_from_query_error,
};
use zinder_store::MempoolEvent;

use crate::mempool::{
    MempoolSurfaceError, SharedMempoolSurface, SharedTipChangeWatcher, TipChangeWatcherError,
};

type GrpcStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Default maximum subtree roots returned when lightwalletd requests "all entries".
pub const DEFAULT_MAX_LIGHTWALLETD_SUBTREE_ROOTS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);
/// Default maximum transparent UTXOs returned when lightwalletd requests "all entries".
pub const DEFAULT_MAX_LIGHTWALLETD_ADDRESS_UTXOS: NonZeroU32 = NonZeroU32::MIN.saturating_add(999);
/// Page size used to drain the native mempool snapshot for lightwalletd.
const LIGHTWALLETD_MEMPOOL_SNAPSHOT_PAGE_SIZE: u32 = 1024;

// Intentionally unimplemented until the owning milestones land:
// transparent history and balance wait for the full transparent-address surface;
// mempool transaction streams wait for M3.

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
}

impl<QueryApi: std::fmt::Debug> std::fmt::Debug for LightwalletdGrpcAdapter<QueryApi> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LightwalletdGrpcAdapter")
            .field("query_api", &self.query_api)
            .field("options", &self.options)
            .field("mempool_surface", &self.mempool_surface.is_some())
            .field("tip_change_watcher", &self.tip_change_watcher.is_some())
            .finish()
    }
}

impl<QueryApi> LightwalletdGrpcAdapter<QueryApi> {
    /// Creates a lightwalletd-compatible gRPC adapter.
    ///
    /// The returned adapter does not advertise mempool surfaces. Pair the
    /// adapter with [`LightwalletdGrpcAdapter::with_mempool_surface`] to
    /// expose `GetMempoolStream` and `GetMempoolTx`; without one, both
    /// methods return `Status::unavailable`.
    #[must_use]
    pub fn new(query_api: QueryApi) -> Self {
        Self::with_options(query_api, LightwalletdCompatibilityOptions::default())
    }

    /// Creates a lightwalletd-compatible gRPC adapter with explicit options.
    #[must_use]
    pub const fn with_options(
        query_api: QueryApi,
        options: LightwalletdCompatibilityOptions,
    ) -> Self {
        Self {
            query_api,
            options,
            mempool_surface: None,
            tip_change_watcher: None,
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
    }
}

impl<QueryApi> LightwalletdGrpcAdapter<QueryApi>
where
    QueryApi: WalletQueryApi + Send + Sync + 'static,
{
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
            .latest_block()
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let mut replies = Vec::new();

        for address in request.addresses {
            let query_request = transparent_address_utxos_request(
                address,
                latest_block.chain_epoch.network,
                BlockHeight::new(start_height),
                max_entries,
            )?;
            let address_utxos = self
                .query_api
                .transparent_address_utxos_at_epoch(query_request, Some(latest_block.chain_epoch))
                .await
                .map_err(|error| status_from_query_error(&error))?;
            replies.extend(lightwalletd_address_utxos(&address_utxos)?);
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
            .latest_block()
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(lightwalletd::BlockId {
            height: u64::from(latest_block.height.value()),
            hash: latest_block.block_hash.as_bytes().to_vec(),
        }))
    }

    async fn get_block(
        &self,
        request: Request<lightwalletd::BlockId>,
    ) -> Result<Response<lightwalletd::CompactBlock>, Status> {
        let block_id = request.into_inner();
        let height = block_height_from_id(&block_id)?;
        let compact_block = self
            .query_api
            .compact_block_at(height)
            .await
            .map_err(|error| status_from_query_error(&error))?;
        let compact_block = decode_compact_block(&compact_block.compact_block.payload_bytes)?;

        if !block_id.hash.is_empty() && block_id.hash != compact_block.hash {
            return Err(Status::not_found(
                "requested block hash does not match indexed block",
            ));
        }

        Ok(Response::new(compact_block))
    }

    async fn get_block_nullifiers(
        &self,
        request: Request<lightwalletd::BlockId>,
    ) -> Result<Response<lightwalletd::CompactBlock>, Status> {
        let compact_block = self.get_block(request).await?.into_inner();
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
        let block_range_request = request.into_inner();
        let pool_selection = pool_selection_from_request(&block_range_request.pool_types)?;
        let (block_range, is_descending) = block_range_from_request(&block_range_request)?;
        let compact_block_range = self
            .query_api
            .compact_block_range(block_range)
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
        let block_range_request = request.into_inner();
        let (block_range, is_descending) = block_range_from_request(&block_range_request)?;
        let compact_block_range = self
            .query_api
            .compact_block_range(block_range)
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
        let filter = request.into_inner();

        let transaction = if let Some(block_id) = filter.block.as_ref() {
            let height = block_height_from_id(block_id)?;
            self.query_api
                .transaction_at_block_index(height, filter.index)
                .await
                .map_err(|error| status_from_query_error(&error))?
        } else {
            let transaction_id = transaction_id_from_lightwalletd_hash(&filter.hash)?;
            self.query_api
                .transaction(transaction_id)
                .await
                .map_err(|error| status_from_query_error(&error))?
        };

        Ok(Response::new(lightwalletd::RawTransaction {
            data: transaction.transaction.payload_bytes,
            height: u64::from(transaction.transaction.block_height.value()),
        }))
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
        _request: Request<lightwalletd::TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTxidsStream>, Status> {
        Err(Status::unimplemented(
            "GetTaddressTxids is outside the indexed transparent-address history surface",
        ))
    }

    type GetTaddressTransactionsStream = GrpcStream<lightwalletd::RawTransaction>;

    async fn get_taddress_transactions(
        &self,
        _request: Request<lightwalletd::TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTransactionsStream>, Status> {
        Err(Status::unimplemented(
            "GetTaddressTransactions is outside the indexed transparent-address history surface",
        ))
    }

    async fn get_taddress_balance(
        &self,
        _request: Request<lightwalletd::AddressList>,
    ) -> Result<Response<lightwalletd::Balance>, Status> {
        Err(Status::unimplemented(
            "GetTaddressBalance is outside the transparent balance surface",
        ))
    }

    async fn get_taddress_balance_stream(
        &self,
        _request: Request<tonic::Streaming<lightwalletd::Address>>,
    ) -> Result<Response<lightwalletd::Balance>, Status> {
        Err(Status::unimplemented(
            "GetTaddressBalanceStream is outside the transparent balance surface",
        ))
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
        let exclude_txid_suffixes = request.into_inner().exclude_txid_suffixes;
        let mut compact_messages = Vec::new();
        let mut next_cursor = None;
        loop {
            let snapshot_page = mempool_surface
                .mempool_snapshot_page(LIGHTWALLETD_MEMPOOL_SNAPSHOT_PAGE_SIZE, next_cursor)
                .await
                .map_err(status_from_mempool_surface_error)?;
            for entry in snapshot_page.entries {
                if txid_matches_excluded_suffix(
                    &entry.transaction_id.as_bytes(),
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
                compact_messages.push(Ok(compact_message));
            }
            if snapshot_page.next_cursor.is_none() {
                break;
            }
            next_cursor = snapshot_page.next_cursor;
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
        let live_tail_sequence = mempool_surface
            .mempool_snapshot_page(1, None)
            .await
            .map_err(status_from_mempool_surface_error)?
            .snapshot_sequence;
        let event_stream = mempool_surface
            .mempool_events(None)
            .await
            .map_err(status_from_mempool_surface_error)?;
        let raw_transaction_stream = stream::StreamExt::filter_map(event_stream, move |event| {
            project_added_after_sequence_to_raw_transaction(event, live_tail_sequence)
        });

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
        let height = block_height_from_id(&request.into_inner())?;
        let tree_state = self
            .query_api
            .tree_state_at(height)
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
            .latest_tree_state()
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(lightwalletd_tree_state(&tree_state)?))
    }

    type GetSubtreeRootsStream = GrpcStream<lightwalletd::SubtreeRoot>;

    async fn get_subtree_roots(
        &self,
        request: Request<lightwalletd::GetSubtreeRootsArg>,
    ) -> Result<Response<Self::GetSubtreeRootsStream>, Status> {
        let request = request.into_inner();
        let protocol = shielded_protocol_from_request(request.shielded_protocol)?;
        let max_entries =
            NonZeroU32::new(request.max_entries).unwrap_or(self.options.max_subtree_roots);
        let subtree_roots = self
            .query_api
            .subtree_roots(SubtreeRootRange::new(
                protocol,
                SubtreeRootIndex::new(request.start_index),
                max_entries,
            ))
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
        let latest_block = self
            .query_api
            .latest_block()
            .await
            .map_err(|error| status_from_query_error(&error))?;

        Ok(Response::new(lightd_info(
            latest_block.chain_epoch.network,
            latest_block.height,
        )?))
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

fn prune_compact_block(
    mut compact_block: lightwalletd::CompactBlock,
    pool_selection: CompactBlockPoolSelection,
    payload_mode: CompactBlockPayloadMode,
) -> lightwalletd::CompactBlock {
    for transaction in &mut compact_block.vtx {
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
    }

    compact_block.vtx.retain(compact_transaction_has_payload);
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

fn block_height_from_id(block_id: &lightwalletd::BlockId) -> Result<BlockHeight, Status> {
    if !block_id.hash.is_empty() && block_id.height == 0 {
        return Err(Status::unimplemented(
            "hash-only block lookups are outside the indexed block lookup surface",
        ));
    }

    let height = u32::try_from(block_id.height)
        .map_err(|_| Status::invalid_argument("block height exceeds u32"))?;
    if height == 0 {
        return Err(Status::not_found("block height 0 is not indexed"));
    }

    Ok(BlockHeight::new(height))
}

fn transaction_id_from_lightwalletd_hash(hash: &[u8]) -> Result<TransactionId, Status> {
    let mut bytes: [u8; 32] = hash
        .try_into()
        .map_err(|_| Status::invalid_argument("transaction hash must be 32 bytes"))?;
    // Lightwalletd encodes transaction hashes in display order; canonical
    // TransactionId bytes are the internal little-endian byte order.
    bytes.reverse();
    Ok(TransactionId::from_bytes(bytes))
}

fn shielded_protocol_from_request(protocol: i32) -> Result<ShieldedProtocol, Status> {
    match lightwalletd::ShieldedProtocol::try_from(protocol) {
        Ok(lightwalletd::ShieldedProtocol::Sapling) => Ok(ShieldedProtocol::Sapling),
        Ok(lightwalletd::ShieldedProtocol::Orchard) => Ok(ShieldedProtocol::Orchard),
        Err(_) => Err(Status::invalid_argument("shieldedProtocol is unknown")),
    }
}

fn transparent_address_utxos_request(
    address: String,
    network: Network,
    start_height: BlockHeight,
    max_entries: NonZeroU32,
) -> Result<TransparentAddressUtxosRequest, Status> {
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

    Ok(TransparentAddressUtxosRequest {
        address,
        script_pub_key,
        start_height,
        max_entries,
    })
}

fn transparent_address_matches_network(address_kind: ZebraNetworkKind, network: Network) -> bool {
    match network {
        Network::ZcashMainnet => address_kind == ZebraNetworkKind::Mainnet,
        Network::ZcashTestnet => address_kind == ZebraNetworkKind::Testnet,
        Network::ZcashRegtest => {
            matches!(
                address_kind,
                ZebraNetworkKind::Testnet | ZebraNetworkKind::Regtest
            )
        }
        _ => false,
    }
}

fn lightwalletd_address_utxos(
    address_utxos: &TransparentAddressUtxos,
) -> Result<Vec<lightwalletd::GetAddressUtxosReply>, Status> {
    address_utxos
        .utxos
        .iter()
        .map(|utxo| {
            Ok(lightwalletd::GetAddressUtxosReply {
                address: address_utxos.address.clone(),
                txid: utxo.outpoint.transaction_id.as_bytes().to_vec(),
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
        network: lightwalletd_chain_name(tree_state.chain_epoch.network)?.to_owned(),
        height: u64::from(tree_state.height.value()),
        hash: display_block_hash(tree_state.block_hash),
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
            completing_block_hash: subtree_root.completing_block_hash.as_bytes().to_vec(),
            completing_block_height: u64::from(subtree_root.completing_block_height.value()),
        })
        .collect()
}

fn lightd_info(
    network: Network,
    tip_height: BlockHeight,
) -> Result<lightwalletd::LightdInfo, Status> {
    let zebra_network = zebra_network(network)?;
    let current_upgrade =
        NetworkUpgrade::current(&zebra_network, ZebraBlockHeight(tip_height.value()));
    let consensus_branch_id = current_upgrade.branch_id().map(u32::from).map_or_else(
        || "00000000".to_owned(),
        |branch_id| format!("{branch_id:08x}"),
    );

    Ok(lightwalletd::LightdInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        vendor: "Zinder".to_owned(),
        taddr_support: true,
        chain_name: lightwalletd_chain_name(network)?.to_owned(),
        sapling_activation_height: u64::from(zebra_network.sapling_activation_height().0),
        consensus_branch_id,
        block_height: u64::from(tip_height.value()),
        git_commit: String::new(),
        branch: String::new(),
        build_date: String::new(),
        build_user: String::new(),
        estimated_height: u64::from(tip_height.value()),
        zcashd_build: String::new(),
        zcashd_subversion: String::new(),
        donation_address: String::new(),
        upgrade_name: String::new(),
        upgrade_height: 0,
        lightwallet_protocol_version: LIGHTWALLETD_PROTOCOL_COMMIT.to_owned(),
    })
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
                error_message: display_transaction_id(transaction_id),
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

fn display_block_hash(block_hash: zinder_core::BlockHash) -> String {
    let mut bytes = block_hash.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
}

fn display_transaction_id(transaction_id: zinder_core::TransactionId) -> String {
    let mut bytes = transaction_id.as_bytes();
    bytes.reverse();
    hex::encode(bytes)
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
fn lightwalletd_chain_name(network: Network) -> Result<&'static str, Status> {
    match network {
        Network::ZcashMainnet => Ok("main"),
        Network::ZcashTestnet => Ok("test"),
        Network::ZcashRegtest => Ok("regtest"),
        _ => Err(Status::failed_precondition(
            "network is not supported by lightwalletd compatibility",
        )),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "non-exhaustive core networks must fail closed until Zebra mapping exists"
)]
fn zebra_network(network: Network) -> Result<ZebraNetwork, Status> {
    match network {
        Network::ZcashMainnet => Ok(ZebraNetwork::Mainnet),
        Network::ZcashTestnet => Ok(ZebraNetwork::new_default_testnet()),
        Network::ZcashRegtest => Ok(ZebraNetwork::new_regtest(RegtestParameters::default())),
        _ => Err(Status::failed_precondition(
            "network is not supported by lightwalletd compatibility",
        )),
    }
}

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
fn project_added_after_sequence_to_raw_transaction(
    event_outcome: Result<zinder_store::MempoolEventEnvelope, MempoolSurfaceError>,
    after_event_sequence: u64,
) -> Option<Result<lightwalletd::RawTransaction, Status>> {
    match event_outcome {
        Ok(envelope) if envelope.event_sequence <= after_event_sequence => None,
        Ok(envelope) => match envelope.event {
            MempoolEvent::Added { entry } => Some(Ok(lightwalletd::RawTransaction {
                data: entry.raw_transaction_bytes.as_slice().to_vec(),
                // Lightwalletd contract: the height field on a mempool
                // raw transaction is "the latest block height" at the
                // moment the transaction was observed. The Android SDK
                // ignores the value and uses GetLatestBlock for tip
                // tracking; Zinder reports the chain epoch's tip height
                // recorded on the entry.
                height: u64::from(entry.first_seen_chain_epoch.tip_height.value()),
            })),
            _ => None,
        },
        Err(error) => Some(Err(status_from_mempool_surface_error(error))),
    }
}
