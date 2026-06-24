//! Private ingest control-plane adapter for writer status, chain events,
//! and mempool surfaces.

use std::{pin::Pin, sync::Arc, time::Instant};

use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, service::interceptor::InterceptedService};
use zinder_core::wire::{decode_rpc_transaction_id_hex, encode_zinder_native_chain_name};
use zinder_core::{
    ChainEpoch, ChainValuePools, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, Network, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint, UnixTimestampMillis,
};
use zinder_proto::capabilities::{CapabilitySurface, capabilities_for_surface};
use zinder_proto::v1::{
    ingest::{
        ServerInfoRequest, ServerInfoResponse, WriterPhase, WriterStatusRequest,
        WriterStatusResponse,
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    ops, wallet,
};
use zinder_runtime::{BearerToken, BearerTokenServerInterceptor};
use zinder_source::{NodeCapability, NodeSource, SourceError};
use zinder_store::{
    ChainEventEncodeError, ChainEventHistoryRequest, ChainEventStreamFamily,
    DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS, MempoolEventHistoryRequest, PrimaryChainStore,
    StreamCursorTokenV1, chain_event_envelope_message, chain_event_stream_family_from_message,
    chain_view_message, mempool_entry_message, mempool_event_envelope_message,
    run_chain_event_stream, status_from_store_error, stream_cursor_from_message_bytes,
    transparent_mempool_output_message, transparent_mempool_spend_message,
    transparent_output_entry_message,
};

use crate::mempool::MempoolIndex;

type IngestControlStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;
type ChainEventsStream = IngestControlStream<wallet::ChainEventEnvelope>;
type MempoolEventsStream = IngestControlStream<wallet::MempoolEventEnvelope>;

/// Default page size for mempool snapshot reads when the request omits one.
const DEFAULT_MEMPOOL_SNAPSHOT_PAGE_SIZE: u32 = 256;

/// Maximum entries returned by a single `MempoolSnapshot` response.
///
/// The native and compat surfaces enforce this cap so a single read cannot
/// exhaust the writer's memory budget; clients requesting larger pages
/// receive a truncated page plus a cursor for the next call.
pub const MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE: u32 = 1024;

const MEMPOOL_SNAPSHOT_CURSOR_LEN: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MempoolSnapshotCursor {
    after_transaction_id: TransactionId,
}

/// Tonic adapter for the private ingest control service.
#[derive(Clone)]
pub struct IngestControlGrpcAdapter {
    network: Network,
    store: PrimaryChainStore,
    mempool_index: Option<MempoolIndex>,
    node_source: Option<Arc<dyn NodeSource>>,
    bearer_token: Option<BearerToken>,
}

impl IngestControlGrpcAdapter {
    /// Creates an ingest-control adapter over the primary store handle.
    ///
    /// The returned adapter does not advertise mempool surfaces; pair the
    /// adapter with [`IngestControlGrpcAdapter::with_mempool`] to expose
    /// `MempoolSnapshot` and `MempoolEvents` once the writer wires the
    /// live `MempoolIndex`.
    #[must_use]
    pub fn new(network: Network, store: PrimaryChainStore) -> Self {
        Self {
            network,
            store,
            mempool_index: None,
            node_source: None,
            bearer_token: None,
        }
    }

    /// Wires the live mempool surfaces into the ingest-control adapter.
    ///
    /// Mempool events are read directly from the underlying chain store; only
    /// the in-memory snapshot index needs to be passed in.
    #[must_use]
    pub fn with_mempool(mut self, mempool_index: MempoolIndex) -> Self {
        self.mempool_index = Some(mempool_index);
        self
    }

    /// Wires source-backed control RPCs into the ingest-control adapter.
    ///
    /// The source handle is owned by the writer process; secondary query
    /// processes proxy through this adapter rather than opening their own
    /// upstream-node connections.
    #[must_use]
    pub fn with_node_source(mut self, node_source: Arc<dyn NodeSource>) -> Self {
        self.node_source = Some(node_source);
        self
    }

    /// Wires a shared-secret bearer token into the ingest-control adapter.
    ///
    /// When set, every gRPC request must carry an `authorization: Bearer
    /// <token>` metadata header that matches `bearer_token`. When unset,
    /// the adapter advertises an open control plane (the default for
    /// localhost-only deployments).
    #[must_use]
    pub fn with_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.bearer_token = Some(bearer_token);
        self
    }

    /// Converts this adapter into a tonic service, wrapping it with the
    /// bearer-token interceptor. When no token is configured, the
    /// interceptor passes every request through unchanged so localhost
    /// deployments behave as before.
    #[must_use]
    pub fn into_server(
        self,
    ) -> InterceptedService<IngestControlServer<Self>, BearerTokenServerInterceptor> {
        let interceptor = BearerTokenServerInterceptor::new(self.bearer_token.clone());
        IngestControlServer::with_interceptor(self, interceptor)
    }

    fn advertised_capabilities(&self) -> Vec<String> {
        let chain_value_pools_supported = self.node_source.as_ref().is_some_and(|source| {
            source
                .capabilities()
                .supports(NodeCapability::ChainValuePools)
        });
        capabilities_for_surface(CapabilitySurface::Ingest)
            .filter(|spec| spec.policy.ingest_satisfied(chain_value_pools_supported))
            .map(|spec| spec.string.to_owned())
            .collect()
    }
}

#[tonic::async_trait]
impl IngestControl for IngestControlGrpcAdapter {
    type ChainEventsStream = ChainEventsStream;
    type MempoolEventsStream = MempoolEventsStream;

    async fn server_info(
        &self,
        _request: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        let server_info = ops::ServerInfo {
            network: encode_zinder_native_chain_name(self.network).to_owned(),
            service_name: env!("CARGO_PKG_NAME").to_owned(),
            service_version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: self.advertised_capabilities(),
        };
        Ok(Response::new(ServerInfoResponse {
            server_info: Some(server_info),
        }))
    }

    async fn writer_status(
        &self,
        _request: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        let started_at = Instant::now();
        let writer_status_outcome = match self.store.current_chain_epoch() {
            Ok(chain_epoch) => {
                if let Some(chain_epoch) = chain_epoch {
                    record_writer_progress(chain_epoch);
                } else {
                    record_empty_writer_progress(self.network);
                }
                // This endpoint reports chain progress only. Loop phase is
                // exposed through readiness, so the ingest-control proto keeps
                // `WriterPhase::Unspecified` for this response.
                Ok(Response::new(WriterStatusResponse {
                    chain_view: chain_epoch.map(chain_view_message),
                    network_name: encode_zinder_native_chain_name(self.network).to_owned(),
                    phase: WriterPhase::Unspecified.into(),
                    gap_blocks: None,
                    upstream_not_ready: None,
                }))
            }
            Err(error) => Err(status_from_store_error(&error)),
        };
        record_writer_status_request_outcome(started_at, &writer_status_outcome);

        writer_status_outcome
    }

    async fn chain_events(
        &self,
        request: Request<wallet::ChainEventsRequest>,
    ) -> Result<Response<Self::ChainEventsStream>, Status> {
        let request = request.into_inner();
        let from_cursor = stream_cursor_from_message_bytes(request.from_cursor);
        let family = chain_event_stream_family_from_message(request.family)
            .ok_or_else(|| Status::invalid_argument("chain-event stream family is unknown"))?;
        let store = self.store.clone();
        let (event_sender, event_receiver) = mpsc::channel(16);
        tokio::spawn(run_chain_event_stream(
            from_cursor,
            move |cursor| read_chain_event_page(store.clone(), cursor, family),
            event_sender,
        ));

        Ok(Response::new(Box::pin(ReceiverStream::new(event_receiver))))
    }

    async fn mempool_snapshot(
        &self,
        request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        let mempool_index = self
            .mempool_index
            .as_ref()
            .ok_or_else(|| Status::unavailable("mempool surface is not configured"))?;
        let request = request.into_inner();
        let max_entries = bounded_snapshot_page_size(request.max_entries);
        let chain_epoch = self
            .store
            .current_chain_epoch()
            .map_err(|error| status_from_store_error(&error))?
            .ok_or_else(|| Status::unavailable("writer has no visible chain epoch"))?;
        let store_for_retention = self.store.clone();
        let retention_report = tokio::task::spawn_blocking(move || {
            store_for_retention.mempool_event_retention_report()
        })
        .await
        .map_err(|join_error| Status::unavailable(join_error.to_string()))?
        .map_err(|error| status_from_store_error(&error))?;
        let snapshot_sequence = retention_report.current_event_sequence;
        let snapshot_cursor =
            decode_mempool_snapshot_cursor(&request.from_cursor, snapshot_sequence)?;
        let snapshot_page = mempool_index.snapshot_page(
            max_entries,
            snapshot_cursor.map(|cursor| cursor.after_transaction_id),
        );
        let snapshot_age_millis = UnixTimestampMillis::now()
            .value()
            .saturating_sub(snapshot_page.last_updated_at.value());
        let entries = snapshot_page
            .entries
            .iter()
            .map(|entry| mempool_entry_message(entry.as_ref()))
            .collect();
        let next_cursor = snapshot_page
            .next_after_transaction_id
            .map_or_else(Vec::new, |transaction_id| {
                encode_mempool_snapshot_cursor(snapshot_sequence, transaction_id)
            });
        record_mempool_snapshot_age_seconds(snapshot_age_millis);
        Ok(Response::new(wallet::MempoolSnapshotResponse {
            chain_view: Some(zinder_store::chain_view_message(chain_epoch)),
            snapshot_sequence,
            snapshot_age_millis,
            entries,
            next_cursor,
        }))
    }

    async fn mempool_events(
        &self,
        request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        if self.mempool_index.is_none() {
            return Err(Status::unavailable("mempool surface is not configured"));
        }
        let store = self.store.clone();
        let request = request.into_inner();
        let from_cursor = stream_cursor_from_message_bytes(request.from_cursor);
        let (event_sender, event_receiver) = mpsc::channel(16);
        tokio::spawn(stream_mempool_events(store, from_cursor, event_sender));
        Ok(Response::new(Box::pin(ReceiverStream::new(event_receiver))))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        let mempool_index = self
            .mempool_index
            .as_ref()
            .ok_or_else(|| Status::unavailable("mempool surface is not configured"))?;
        let request = request.into_inner();
        let address_script_hash = script_hash_from_lookup(request.address)?;
        let max_entries = bounded_point_lookup_max_entries(request.max_entries);
        let chain_epoch = self
            .store
            .current_chain_epoch()
            .map_err(|error| status_from_store_error(&error))?
            .ok_or_else(|| Status::unavailable("writer has no visible chain epoch"))?;
        let outputs = mempool_index
            .transparent_outputs_by_address(address_script_hash, max_entries)
            .iter()
            .map(transparent_mempool_output_message)
            .collect();
        Ok(Response::new(
            wallet::TransparentMempoolOutputsByAddressResponse {
                chain_view: Some(chain_view_message(chain_epoch)),
                outputs,
            },
        ))
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendsByOutpointResponse>, Status> {
        let mempool_index = self
            .mempool_index
            .as_ref()
            .ok_or_else(|| Status::unavailable("mempool surface is not configured"))?;
        let request = request.into_inner();
        let mut request_outpoints = request.outpoints;
        request_outpoints.truncate(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST);
        let outpoints = request_outpoints
            .into_iter()
            .map(|message| outpoint_from_request_message(Some(message)))
            .collect::<Result<Vec<_>, _>>()?;
        let chain_epoch = self
            .store
            .current_chain_epoch()
            .map_err(|error| status_from_store_error(&error))?
            .ok_or_else(|| Status::unavailable("writer has no visible chain epoch"))?;
        let spends = outpoints
            .into_iter()
            .filter_map(|outpoint| mempool_index.transparent_spend_by_outpoint(outpoint))
            .map(|spend| transparent_mempool_spend_message(&spend))
            .collect();
        Ok(Response::new(
            wallet::TransparentMempoolSpendsByOutpointResponse {
                chain_view: Some(chain_view_message(chain_epoch)),
                spends,
            },
        ))
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        let mempool_index = self
            .mempool_index
            .as_ref()
            .ok_or_else(|| Status::unavailable("mempool surface is not configured"))?;
        let request = request.into_inner();
        let mut request_outpoints = request.outpoints;
        request_outpoints.truncate(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST);
        let outpoints = request_outpoints
            .into_iter()
            .map(|message| outpoint_from_request_message(Some(message)))
            .collect::<Result<Vec<_>, _>>()?;
        let chain_epoch = self
            .store
            .current_chain_epoch()
            .map_err(|error| status_from_store_error(&error))?
            .ok_or_else(|| Status::unavailable("writer has no visible chain epoch"))?;
        let entries = mempool_index
            .transparent_outputs_by_outpoints(&outpoints)
            .into_iter()
            .map(transparent_output_entry_message)
            .collect();
        Ok(Response::new(
            wallet::TransparentOutputsByOutpointResponse {
                chain_view: Some(chain_view_message(chain_epoch)),
                entries,
            },
        ))
    }

    async fn chain_value_pools_at_tip(
        &self,
        _request: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        let node_source = self
            .node_source
            .as_ref()
            .ok_or_else(|| Status::unavailable("node source is not configured"))?;
        if !node_source
            .capabilities()
            .supports(NodeCapability::ChainValuePools)
        {
            return Err(Status::failed_precondition(
                "upstream node does not advertise chain_value_pools",
            ));
        }
        let chain_epoch = self
            .store
            .current_chain_epoch()
            .map_err(|error| status_from_store_error(&error))?
            .ok_or_else(|| Status::unavailable("writer has no visible chain epoch"))?;
        let value_pools = node_source
            .fetch_chain_value_pools_at_tip()
            .await
            .map_err(|error| status_from_source_error(&error))?;
        Ok(Response::new(chain_value_pools_response(
            chain_epoch,
            value_pools,
        )))
    }
}

/// Default cap for transparent-mempool point lookups when the request omits one.
const DEFAULT_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES: u32 = 256;

/// Hard cap enforced by the writer regardless of caller-requested page size.
const MAX_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES: u32 = 1024;

fn chain_value_pools_response(
    chain_epoch: ChainEpoch,
    value_pools: ChainValuePools,
) -> wallet::ChainValuePoolsAtTipResponse {
    wallet::ChainValuePoolsAtTipResponse {
        chain_view: Some(chain_view_message(chain_epoch)),
        pools: value_pools
            .pools
            .into_iter()
            .map(|pool| wallet::ChainValuePool {
                id: pool.id,
                monitored: pool.monitored,
                chain_value_zat: pool.chain_value_zat,
            })
            .collect(),
        tip_height: value_pools.tip_height.value(),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only source errors with public gRPC semantics need distinct codes here."
)]
fn status_from_source_error(error: &SourceError) -> Status {
    match error {
        SourceError::NodeUnavailable { reason } => Status::unavailable(reason.clone()),
        SourceError::NodeCapabilityMissing { capability } => {
            Status::failed_precondition(format!("upstream node is missing {capability}"))
        }
        SourceError::SourceProtocolMismatch { reason } => Status::data_loss(*reason),
        _ => Status::internal(error.to_string()),
    }
}

const fn bounded_point_lookup_max_entries(requested: Option<u32>) -> u32 {
    let Some(requested) = requested else {
        return DEFAULT_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES;
    };
    if requested == 0 {
        DEFAULT_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES
    } else if requested > MAX_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES {
        MAX_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES
    } else {
        requested
    }
}

/// Decode an [`wallet::AddressLookup`] into a [`TransparentAddressScriptHash`],
/// rejecting the parsed-`Address` selector.
///
/// The ingest control-plane accepts only the script-hash form; callers must
/// pre-resolve transparent addresses through `WalletQueryApi`. Parsing
/// addresses here would couple ingest to `zebra-chain` for a selector that
/// never appears in practice on this surface.
fn script_hash_from_lookup(
    address: Option<wallet::AddressLookup>,
) -> Result<TransparentAddressScriptHash, Status> {
    let lookup = address.ok_or_else(|| Status::invalid_argument("address selector is required"))?;
    let selector = lookup
        .selector
        .ok_or_else(|| Status::invalid_argument("address selector is empty"))?;
    match selector {
        wallet::address_lookup::Selector::ScriptHash(bytes) => {
            let hash_bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Status::invalid_argument("address.script_hash must be 32 bytes"))?;
            Ok(TransparentAddressScriptHash::from_bytes(hash_bytes))
        }
        wallet::address_lookup::Selector::Address(_) => Err(Status::invalid_argument(
            "ingest-control accepts only the script_hash selector; \
             callers must pre-resolve transparent addresses",
        )),
    }
}

fn transaction_id_from_rpc_hex(rpc_hex: &str) -> Result<TransactionId, Status> {
    decode_rpc_transaction_id_hex(rpc_hex)
        .map_err(|error| Status::invalid_argument(error.to_string()))
}

fn outpoint_from_request_message(
    message: Option<wallet::OutPoint>,
) -> Result<TransparentOutPoint, Status> {
    let message = message.ok_or_else(|| Status::invalid_argument("outpoint is required"))?;
    let transaction_id = transaction_id_from_rpc_hex(&message.transaction_id)?;
    Ok(TransparentOutPoint::new(
        transaction_id,
        message.output_index,
    ))
}

async fn stream_mempool_events(
    store: PrimaryChainStore,
    mut from_cursor: Option<StreamCursorTokenV1>,
    event_sender: mpsc::Sender<Result<wallet::MempoolEventEnvelope, Status>>,
) {
    loop {
        // RocksDB reads are synchronous; offload off the runtime worker so
        // the orchestrator and other async tasks keep making progress.
        let page_store = store.clone();
        let page_cursor = from_cursor.clone();
        let page_outcome = match tokio::task::spawn_blocking(move || {
            let request = MempoolEventHistoryRequest::new(
                page_cursor.as_ref(),
                DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS,
            );
            page_store.mempool_event_history(request)
        })
        .await
        {
            Ok(page_outcome) => page_outcome,
            Err(join_error) => {
                let _ = event_sender
                    .send(Err(Status::unavailable(join_error.to_string())))
                    .await;
                return;
            }
        };
        match page_outcome {
            Ok(envelopes) => {
                let truncated = u32::try_from(envelopes.len())
                    .is_ok_and(|count| count >= DEFAULT_MAX_MEMPOOL_EVENT_HISTORY_EVENTS.get());
                for envelope in envelopes {
                    let proto_outcome = mempool_event_envelope_message(&envelope);
                    let send_outcome = match proto_outcome {
                        Ok(message) => {
                            from_cursor = Some(envelope.cursor.clone());
                            event_sender.send(Ok(message)).await
                        }
                        Err(error) => {
                            event_sender
                                .send(Err(status_from_chain_event_encode_error(error)))
                                .await
                        }
                    };
                    if send_outcome.is_err() {
                        return;
                    }
                }
                if !truncated {
                    // Exit immediately when the receiver drops so server
                    // shutdown does not have to wait out the poll interval.
                    tokio::select! {
                        () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                        () = event_sender.closed() => return,
                    }
                }
            }
            Err(error) => {
                let _ = event_sender
                    .send(Err(status_from_store_error(&error)))
                    .await;
                return;
            }
        }
    }
}

fn record_mempool_snapshot_age_seconds(snapshot_age_millis: u64) {
    let elapsed_seconds = duration_seconds_from_millis(snapshot_age_millis);
    metrics::gauge!("zinder_mempool_snapshot_age_seconds").set(elapsed_seconds);
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; snapshot age is diagnostic"
)]
fn duration_seconds_from_millis(millis: u64) -> f64 {
    (millis as f64) / 1_000.0
}

const fn bounded_snapshot_page_size(requested: u32) -> u32 {
    if requested == 0 {
        DEFAULT_MEMPOOL_SNAPSHOT_PAGE_SIZE
    } else if requested > MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE {
        MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE
    } else {
        requested
    }
}

fn decode_mempool_snapshot_cursor(
    cursor_bytes: &[u8],
    current_snapshot_sequence: u64,
) -> Result<Option<MempoolSnapshotCursor>, Status> {
    if cursor_bytes.is_empty() {
        return Ok(None);
    }
    if cursor_bytes.len() != MEMPOOL_SNAPSHOT_CURSOR_LEN {
        return Err(Status::invalid_argument(
            "mempool snapshot cursor is invalid",
        ));
    }

    let sequence_bytes = <[u8; 8]>::try_from(&cursor_bytes[..8])
        .map_err(|_| Status::invalid_argument("mempool snapshot cursor sequence is invalid"))?;
    let snapshot_sequence = u64::from_be_bytes(sequence_bytes);
    if snapshot_sequence > current_snapshot_sequence {
        return Err(Status::invalid_argument(
            "mempool snapshot cursor is ahead of retained history",
        ));
    }

    let transaction_id_bytes = <[u8; 32]>::try_from(&cursor_bytes[8..]).map_err(|_| {
        Status::invalid_argument("mempool snapshot cursor transaction id is invalid")
    })?;
    Ok(Some(MempoolSnapshotCursor {
        after_transaction_id: TransactionId::from_bytes(transaction_id_bytes),
    }))
}

fn encode_mempool_snapshot_cursor(
    snapshot_sequence: u64,
    after_transaction_id: TransactionId,
) -> Vec<u8> {
    let mut cursor_bytes = Vec::with_capacity(MEMPOOL_SNAPSHOT_CURSOR_LEN);
    cursor_bytes.extend_from_slice(&snapshot_sequence.to_be_bytes());
    cursor_bytes.extend_from_slice(&after_transaction_id.as_bytes());
    cursor_bytes
}

async fn read_chain_event_page(
    store: PrimaryChainStore,
    cursor: Option<StreamCursorTokenV1>,
    family: ChainEventStreamFamily,
) -> Result<Vec<wallet::ChainEventEnvelope>, Status> {
    let event_history = tokio::task::spawn_blocking(move || {
        store.chain_event_history(ChainEventHistoryRequest::new_for_family(
            cursor.as_ref(),
            family,
            zinder_store::DEFAULT_MAX_CHAIN_EVENT_HISTORY_EVENTS,
        ))
    })
    .await
    .map_err(|join_error| Status::unavailable(join_error.to_string()))?
    .map_err(|error| status_from_store_error(&error))?;

    event_history
        .iter()
        .map(|event_envelope| {
            chain_event_envelope_message(event_envelope)
                .map_err(status_from_chain_event_encode_error)
        })
        .collect()
}

fn status_from_chain_event_encode_error(error: ChainEventEncodeError) -> Status {
    match error {
        ChainEventEncodeError::UnsupportedChainEvent { event } => Status::unavailable(event),
        _ => Status::unavailable("unknown chain event encode error"),
    }
}

fn record_writer_status_request_outcome(
    started_at: Instant,
    writer_status_outcome: &Result<Response<WriterStatusResponse>, Status>,
) {
    metrics::histogram!(
        "zinder_ingest_writer_status_request_duration_seconds",
        "status" => outcome_status(writer_status_outcome),
        "error_class" => tonic_status_error_class(writer_status_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_ingest_writer_status_request_total",
        "status" => outcome_status(writer_status_outcome),
        "error_class" => tonic_status_error_class(writer_status_outcome.as_ref().err())
    )
    .increment(1);
    metrics::gauge!("zinder_ingest_writer_status_available").set(
        if writer_status_outcome.is_ok() {
            1.0
        } else {
            0.0
        },
    );
}

fn record_writer_progress(chain_epoch: ChainEpoch) {
    metrics::gauge!(
        "zinder_ingest_writer_has_chain_epoch",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(1.0);
    metrics::gauge!(
        "zinder_ingest_writer_chain_epoch_id",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u64_to_f64(chain_epoch.id.value()));
    metrics::gauge!(
        "zinder_ingest_writer_tip_height",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u32_to_f64(chain_epoch.tip_height.value()));
    metrics::gauge!(
        "zinder_ingest_writer_safe_tip_height",
        "network" => encode_zinder_native_chain_name(chain_epoch.network)
    )
    .set(u32_to_f64(chain_epoch.safe_tip_height.value()));
}

fn record_empty_writer_progress(network: Network) {
    metrics::gauge!(
        "zinder_ingest_writer_has_chain_epoch",
        "network" => encode_zinder_native_chain_name(network)
    )
    .set(0.0);
    metrics::gauge!(
        "zinder_ingest_writer_chain_epoch_id",
        "network" => encode_zinder_native_chain_name(network)
    )
    .set(0.0);
    metrics::gauge!(
        "zinder_ingest_writer_tip_height",
        "network" => encode_zinder_native_chain_name(network)
    )
    .set(0.0);
    metrics::gauge!(
        "zinder_ingest_writer_safe_tip_height",
        "network" => encode_zinder_native_chain_name(network)
    )
    .set(0.0);
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn tonic_status_error_class(error: Option<&Status>) -> &'static str {
    match error.map(Status::code) {
        None | Some(tonic::Code::Ok) => "none",
        Some(tonic::Code::Cancelled) => "cancelled",
        Some(tonic::Code::Unknown) => "unknown",
        Some(tonic::Code::InvalidArgument) => "invalid_argument",
        Some(tonic::Code::DeadlineExceeded) => "deadline_exceeded",
        Some(tonic::Code::NotFound) => "not_found",
        Some(tonic::Code::AlreadyExists) => "already_exists",
        Some(tonic::Code::PermissionDenied) => "permission_denied",
        Some(tonic::Code::ResourceExhausted) => "resource_exhausted",
        Some(tonic::Code::FailedPrecondition) => "failed_precondition",
        Some(tonic::Code::Aborted) => "aborted",
        Some(tonic::Code::OutOfRange) => "out_of_range",
        Some(tonic::Code::Unimplemented) => "unimplemented",
        Some(tonic::Code::Internal) => "internal",
        Some(tonic::Code::Unavailable) => "unavailable",
        Some(tonic::Code::DataLoss) => "data_loss",
        Some(tonic::Code::Unauthenticated) => "unauthenticated",
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; chain progress values are diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; block heights are diagnostic"
)]
fn u32_to_f64(sample: u32) -> f64 {
    f64::from(sample)
}
