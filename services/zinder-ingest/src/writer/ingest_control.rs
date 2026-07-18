//! Version-1 `IngestControl` composition over the canonical writer channel.
//!
//! This adapter never opens a canonical primary or a derive store. Exact
//! writer status and tip-event replay are serviced by the follower that owns
//! the admitted `RocksDbCanonicalStore`; resumable mempool history and cursor
//! retention are also owned by that canonical `RocksDB` event log. Only the
//! current low-latency mempool index is process-local, and it remains hidden
//! until its source snapshot completes.

use std::{num::NonZeroU32, pin::Pin, sync::Arc, time::Duration};

use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, service::interceptor::InterceptedService};
use zinder_core::{
    ChainEpoch, ChainValuePools, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, Network, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint,
    wire::{decode_rpc_transaction_id_hex, encode_zinder_native_chain_name},
};
use zinder_proto::{
    capabilities::{CapabilitySurface, INGEST_CONTROL_CHAIN_EVENTS_V1, capabilities_for_surface},
    v1::{
        ingest::{
            MempoolTransactionRequest, ServerInfoRequest, ServerInfoResponse, WriterPhase,
            WriterStatusRequest, WriterStatusResponse,
            ingest_control_server::{IngestControl, IngestControlServer},
        },
        ops, wallet,
    },
};
use zinder_runtime::{
    BearerToken, BearerTokenServerInterceptor, Readiness, ReadinessCause, ReadinessReport,
};
use zinder_source::{NodeCapability, NodeSource, SourceError};
use zinder_store::{
    CanonicalEventKind, block_tip_message, chain_epoch_message, chain_view_message,
    event_stream_start_from_message, mempool_entry_message, transparent_mempool_output_message,
    transparent_mempool_spend_message, transparent_output_entry_message,
};

use crate::{
    CanonicalControlHandle, LiveMempoolOwner,
    writer::control::{CanonicalIngestEvent, CanonicalWriterSnapshot},
};

type IngestControlStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;
type ChainEventsStream = IngestControlStream<wallet::ChainEventEnvelope>;
type MempoolEventsStream = IngestControlStream<wallet::MempoolEventEnvelope>;

const DEFAULT_MEMPOOL_SNAPSHOT_PAGE_SIZE: u32 = 256;
/// Hard response bound shared by the private writer control APIs.
pub const MAX_MEMPOOL_SNAPSHOT_PAGE_SIZE: u32 = 1_024;
const DEFAULT_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES: u32 = 256;
const MAX_TRANSPARENT_MEMPOOL_POINT_LOOKUP_MAX_ENTRIES: u32 = 1_024;
const CANONICAL_EVENT_PAGE_SIZE: NonZeroU32 = NonZeroU32::MIN.saturating_add(63);
const EVENT_STREAM_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Version-1 adapter for query and compatibility clients that consume the
/// ingest writer's control plane.
#[derive(Clone)]
pub struct CanonicalIngestControlGrpcAdapter {
    network: Network,
    canonical: CanonicalControlHandle,
    mempool: LiveMempoolOwner,
    node_source: Arc<dyn NodeSource>,
    bearer_token: Option<BearerToken>,
    readiness: Readiness,
}

impl CanonicalIngestControlGrpcAdapter {
    /// Binds one canonical writer command channel, one live mempool owner,
    /// and the source handle already owned by the ingest runtime.
    #[must_use]
    pub fn new(
        network: Network,
        canonical: CanonicalControlHandle,
        mempool: LiveMempoolOwner,
        node_source: Arc<dyn NodeSource>,
        readiness: Readiness,
    ) -> Self {
        Self {
            network,
            canonical,
            mempool,
            node_source,
            bearer_token: None,
            readiness,
        }
    }

    /// Applies the optional private-control bearer token to every RPC.
    #[must_use]
    pub fn with_bearer_token(mut self, bearer_token: Option<BearerToken>) -> Self {
        self.bearer_token = bearer_token;
        self
    }

    /// Builds the authenticated tonic service for the same listener as
    /// `CanonicalControl`.
    #[must_use]
    pub fn into_server(
        self,
    ) -> InterceptedService<IngestControlServer<Self>, BearerTokenServerInterceptor> {
        let interceptor = BearerTokenServerInterceptor::new(self.bearer_token.clone());
        let server = IngestControlServer::new(self)
            .max_decoding_message_size(zinder_runtime::MAX_DECODING_MESSAGE_BYTES);
        InterceptedService::new(server, interceptor)
    }

    fn advertised_capabilities(&self) -> Vec<String> {
        let chain_value_pools_supported = self
            .node_source
            .capabilities()
            .supports(NodeCapability::ChainValuePools);
        capabilities_for_surface(CapabilitySurface::Ingest)
            .filter(|spec| spec.string != INGEST_CONTROL_CHAIN_EVENTS_V1)
            .filter(|spec| spec.policy.ingest_satisfied(chain_value_pools_supported))
            .map(|spec| spec.string.to_owned())
            .collect()
    }

    async fn current_chain_epoch(&self) -> Result<CanonicalWriterSnapshot, Status> {
        self.canonical.chain_epoch().await
    }
}

#[tonic::async_trait]
impl IngestControl for CanonicalIngestControlGrpcAdapter {
    type ChainEventsStream = ChainEventsStream;
    type MempoolEventsStream = MempoolEventsStream;

    async fn server_info(
        &self,
        _request: Request<ServerInfoRequest>,
    ) -> Result<Response<ServerInfoResponse>, Status> {
        Ok(Response::new(ServerInfoResponse {
            server_info: Some(ops::ServerInfo {
                network: encode_zinder_native_chain_name(self.network).to_owned(),
                service_name: env!("CARGO_PKG_NAME").to_owned(),
                service_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: self.advertised_capabilities(),
                contract_revision: zinder_proto::CONTRACT_REVISION,
                projection_preset: "wallet".to_owned(),
                projection_identities: Vec::new(),
            }),
        }))
    }

    async fn writer_status(
        &self,
        _request: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        let snapshot = self.current_chain_epoch().await?;
        Ok(Response::new(writer_status_response(
            self.network,
            snapshot.chain_epoch,
            &self.readiness.report(),
        )))
    }

    async fn chain_events(
        &self,
        request: Request<wallet::ChainEventsRequest>,
    ) -> Result<Response<Self::ChainEventsStream>, Status> {
        let request = request.into_inner();
        if request.family != wallet::ChainEventStreamFamily::Tip as i32 {
            return Err(Status::unimplemented(
                "version-1 ingest control supports only the tip chain-event family",
            ));
        }
        if !request.address_filter.is_empty() {
            return Err(Status::unimplemented(
                "version-1 ingest control does not retain address-filtered chain events",
            ));
        }
        let after_cursor = match request
            .start
            .ok_or_else(|| Status::invalid_argument("event stream start is required"))?
            .position
            .ok_or_else(|| Status::invalid_argument("event stream start position is required"))?
        {
            wallet::event_stream_start::Position::AfterCursor(cursor) => Some(cursor),
            wallet::event_stream_start::Position::EarliestRetained(_) => None,
            wallet::event_stream_start::Position::LiveTail(_) => {
                let snapshot = self.current_chain_epoch().await?;
                (snapshot.fence.chain_event_sequence() != 0)
                    .then(|| {
                        zinder_store::CanonicalEventCursor::at(
                            snapshot.fence.chain_event_sequence(),
                        )
                        .map(|cursor| cursor.as_bytes().to_vec())
                        .map_err(|_| {
                            Status::internal("canonical writer returned an invalid event sequence")
                        })
                    })
                    .transpose()?
            }
        };
        Ok(Response::new(spawn_chain_event_stream(
            self.canonical.clone(),
            after_cursor,
        )))
    }

    async fn mempool_snapshot(
        &self,
        request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        let request = request.into_inner();
        let page = self
            .mempool
            .snapshot_page(
                &self.canonical,
                bounded_snapshot_page_size(request.max_entries),
                request.from_cursor,
            )
            .await?;
        let snapshot = self.current_chain_epoch().await?;
        Ok(Response::new(wallet::MempoolSnapshotResponse {
            chain_view: Some(chain_view_message(snapshot.chain_epoch)),
            events_resume_cursor: page.events_resume_cursor,
            snapshot_age_millis: page.snapshot_age_millis,
            entries: page
                .entries
                .iter()
                .map(|entry| mempool_entry_message(entry.as_ref()))
                .collect(),
            next_cursor: page.next_cursor,
        }))
    }

    async fn mempool_transaction(
        &self,
        request: Request<MempoolTransactionRequest>,
    ) -> Result<Response<wallet::TransactionStatusResponse>, Status> {
        let transaction_id = transaction_id_from_rpc_hex(&request.into_inner().transaction_id)?;
        let entry = self.mempool.entry_for(transaction_id)?.ok_or_else(|| {
            Status::not_found("transaction is not visible in the live mempool index")
        })?;
        let snapshot = self.current_chain_epoch().await?;
        Ok(Response::new(wallet::TransactionStatusResponse {
            chain_view: Some(chain_view_message(snapshot.chain_epoch)),
            location: Some(wallet::TransactionLocation {
                location: Some(wallet::transaction_location::Location::InMempool(
                    wallet::MempoolTransaction {
                        payload_bytes: entry.raw_transaction_bytes.as_slice().to_vec(),
                        first_seen_unix_seconds: i64::try_from(
                            entry.first_seen_unix_millis.value() / 1_000,
                        )
                        .unwrap_or(i64::MAX),
                    },
                )),
            }),
        }))
    }

    async fn mempool_events(
        &self,
        request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        let request = request.into_inner();
        if request.family != wallet::MempoolEventStreamFamily::Mempool as i32 {
            return Err(Status::unimplemented(
                "version-1 ingest control supports only the mempool event family",
            ));
        }
        let start = event_stream_start_from_message(request.start)
            .ok_or_else(|| Status::invalid_argument("event stream start is required"))?;
        let after_cursor = self
            .mempool
            .resolve_event_start(&self.canonical, start)
            .await?;
        Ok(Response::new(spawn_mempool_event_stream(
            self.mempool.clone(),
            self.canonical.clone(),
            after_cursor,
        )))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        let request = request.into_inner();
        let address_script_hash = script_hash_from_lookup(request.address)?;
        let outputs = self
            .mempool
            .transparent_outputs_by_address(
                address_script_hash,
                bounded_point_lookup_max_entries(request.max_entries),
            )?
            .iter()
            .map(transparent_mempool_output_message)
            .collect();
        let snapshot = self.current_chain_epoch().await?;
        Ok(Response::new(
            wallet::TransparentMempoolOutputsByAddressResponse {
                chain_view: Some(chain_view_message(snapshot.chain_epoch)),
                outputs,
            },
        ))
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendsByOutpointResponse>, Status> {
        let mut outpoints = request.into_inner().outpoints;
        outpoints.truncate(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST);
        let outpoints = outpoints
            .into_iter()
            .map(|outpoint| outpoint_from_request_message(Some(outpoint)))
            .collect::<Result<Vec<_>, _>>()?;
        let spends = self
            .mempool
            .transparent_spends_by_outpoint(outpoints)?
            .iter()
            .map(transparent_mempool_spend_message)
            .collect();
        let snapshot = self.current_chain_epoch().await?;
        Ok(Response::new(
            wallet::TransparentMempoolSpendsByOutpointResponse {
                chain_view: Some(chain_view_message(snapshot.chain_epoch)),
                spends,
            },
        ))
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        request: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        let mut outpoints = request.into_inner().outpoints;
        outpoints.truncate(MAX_TRANSPARENT_OUTPUTS_PER_REQUEST);
        let outpoints = outpoints
            .into_iter()
            .map(|outpoint| outpoint_from_request_message(Some(outpoint)))
            .collect::<Result<Vec<_>, _>>()?;
        let entries = self
            .mempool
            .transparent_outputs_by_outpoints(&outpoints)?
            .into_iter()
            .map(transparent_output_entry_message)
            .collect();
        let snapshot = self.current_chain_epoch().await?;
        Ok(Response::new(
            wallet::TransparentOutputsByOutpointResponse {
                chain_view: Some(chain_view_message(snapshot.chain_epoch)),
                entries,
            },
        ))
    }

    async fn chain_value_pools_at_tip(
        &self,
        _request: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        if !self
            .node_source
            .capabilities()
            .supports(NodeCapability::ChainValuePools)
        {
            return Err(Status::unimplemented(
                "upstream node does not advertise chain_value_pools",
            ));
        }
        let snapshot = self.current_chain_epoch().await?;
        let value_pools = self
            .node_source
            .fetch_chain_value_pools_at_tip()
            .await
            .map_err(|error| status_from_source_error(&error))?;
        Ok(Response::new(chain_value_pools_response(
            snapshot.chain_epoch,
            value_pools,
        )))
    }
}

fn spawn_chain_event_stream(
    canonical: CanonicalControlHandle,
    mut after_cursor: Option<Vec<u8>>,
) -> ChainEventsStream {
    let (event_sender, event_receiver) = mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            match canonical
                .ingest_event_page(after_cursor.clone(), CANONICAL_EVENT_PAGE_SIZE)
                .await
            {
                Ok(page) if page.events.is_empty() => {
                    tokio::select! {
                        () = tokio::time::sleep(EVENT_STREAM_IDLE_POLL_INTERVAL) => {}
                        () = event_sender.closed() => return,
                    }
                }
                Ok(page) => {
                    for event in page.events {
                        after_cursor = Some(event.cursor.as_bytes().to_vec());
                        let message = canonical_event_message(event);
                        if event_sender.send(message).await.is_err() {
                            return;
                        }
                    }
                }
                Err(status) => {
                    let _send_result = event_sender.send(Err(status)).await;
                    return;
                }
            }
        }
    });
    Box::pin(ReceiverStream::new(event_receiver))
}

fn spawn_mempool_event_stream(
    mempool: LiveMempoolOwner,
    canonical: CanonicalControlHandle,
    mut after_cursor: Option<zinder_store::StreamCursorTokenV1>,
) -> MempoolEventsStream {
    let (event_sender, event_receiver) = mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            match mempool.event_page(&canonical, after_cursor.clone()).await {
                Ok(events) if events.is_empty() => {
                    tokio::select! {
                        () = tokio::time::sleep(EVENT_STREAM_IDLE_POLL_INTERVAL) => {}
                        () = event_sender.closed() => return,
                    }
                }
                Ok(events) => {
                    for event in events {
                        after_cursor = Some(zinder_store::StreamCursorTokenV1::from_bytes(
                            event.cursor.clone(),
                        ));
                        if event_sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(status) => {
                    let _send_result = event_sender.send(Err(status)).await;
                    return;
                }
            }
        }
    });
    Box::pin(ReceiverStream::new(event_receiver))
}

fn canonical_event_message(
    event: CanonicalIngestEvent,
) -> Result<wallet::ChainEventEnvelope, Status> {
    let event_body = match event.kind {
        CanonicalEventKind::Committed => {
            wallet::chain_event_envelope::Event::ChainCommitted(wallet::ChainCommitted {
                committed: Some(chain_epoch_committed_message(
                    event.resulting_epoch,
                    event.committed_range,
                )),
            })
        }
        CanonicalEventKind::Reorged => {
            let previous_epoch = event.previous_epoch.ok_or_else(|| {
                Status::failed_precondition("retained reorg event has no previous canonical epoch")
            })?;
            let reverted_range = event.reverted_range.ok_or_else(|| {
                Status::failed_precondition("retained reorg event has no reverted range")
            })?;
            wallet::chain_event_envelope::Event::ChainReorged(wallet::ChainReorged {
                reverted: Some(wallet::ChainRangeReverted {
                    chain_epoch: Some(chain_epoch_message(previous_epoch)),
                    start_height: reverted_range.start.value(),
                    end_height: reverted_range.end.value(),
                }),
                committed: Some(chain_epoch_committed_message(
                    event.resulting_epoch,
                    event.committed_range,
                )),
            })
        }
    };
    Ok(wallet::ChainEventEnvelope {
        cursor: event.cursor.as_bytes().to_vec(),
        event_sequence: event.cursor.event_sequence(),
        chain_view: Some(chain_view_message(event.resulting_epoch)),
        event: Some(event_body),
    })
}

fn chain_epoch_committed_message(
    chain_epoch: ChainEpoch,
    range: zinder_core::BlockHeightRange,
) -> wallet::ChainEpochCommitted {
    wallet::ChainEpochCommitted {
        chain_epoch: Some(chain_epoch_message(chain_epoch)),
        start_height: range.start.value(),
        end_height: range.end.value(),
    }
}

fn writer_status_response(
    network: Network,
    chain_epoch: ChainEpoch,
    readiness_report: &ReadinessReport,
) -> WriterStatusResponse {
    let upstream_tip = upstream_tip_from_readiness(readiness_report);
    let gap_blocks = upstream_tip
        .as_ref()
        .and_then(|upstream_tip| upstream_tip.committed_height)
        .map(|upstream_height| {
            upstream_height.saturating_sub(chain_epoch.visible_tip_height.value())
        });
    let mut chain_view = chain_view_message(chain_epoch);
    chain_view.upstream_tip = upstream_tip;
    let upstream_not_ready =
        if let ReadinessCause::UpstreamNotReady(detail) = &readiness_report.cause {
            Some(ops::UpstreamNotReadyDetail::from(detail))
        } else {
            None
        };
    WriterStatusResponse {
        chain_view: Some(chain_view),
        network_name: encode_zinder_native_chain_name(network).to_owned(),
        phase: readiness_report
            .phase
            .map_or(WriterPhase::Unspecified, WriterPhase::from)
            .into(),
        gap_blocks,
        upstream_not_ready,
    }
}

fn upstream_tip_from_readiness(readiness_report: &ReadinessReport) -> Option<wallet::UpstreamTip> {
    if let ReadinessCause::UpstreamNotReady(detail) = &readiness_report.cause {
        (detail.upstream_committed_height.is_some() || detail.upstream_estimated_height.is_some())
            .then_some(wallet::UpstreamTip {
                committed_height: detail.upstream_committed_height,
                estimated_height: detail.upstream_estimated_height,
            })
    } else {
        readiness_report
            .target_height
            .map(|committed_height| wallet::UpstreamTip {
                committed_height: Some(committed_height),
                estimated_height: None,
            })
    }
}

fn chain_value_pools_response(
    chain_epoch: ChainEpoch,
    value_pools: ChainValuePools,
) -> wallet::ChainValuePoolsAtTipResponse {
    let source_tip = value_pools.source_tip;
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
        source_tip: Some(block_tip_message(source_tip.height, source_tip.hash)),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Only source errors with public gRPC semantics need distinct status codes."
)]
fn status_from_source_error(error: &SourceError) -> Status {
    match error {
        SourceError::NodeUnavailable { reason } => Status::unavailable(reason.clone()),
        SourceError::NodeCapabilityMissing { capability } => {
            Status::failed_precondition(format!("upstream node is missing {capability}"))
        }
        SourceError::SourceProtocolMismatch { reason } => Status::data_loss(*reason),
        _ => Status::internal("upstream source operation failed"),
    }
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
        wallet::address_lookup::Selector::Address(_) => Err(Status::unimplemented(
            "version-1 ingest control accepts only the script_hash address selector",
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
    Ok(TransparentOutPoint::new(
        transaction_id_from_rpc_hex(&message.transaction_id)?,
        message.output_index,
    ))
}

#[cfg(test)]
mod tests {
    use std::{fs, str::FromStr as _, sync::Arc, time::Duration};

    use tokio::net::TcpListener;
    use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
    use tokio_util::sync::CancellationToken;
    use tonic::{Code, Request, transport::Server};
    use zinder_core::{
        ArtifactSchemaVersion, BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId,
        ChainTipMetadata, MempoolEntry, RawTransactionBytes, TransactionId, UnixTimestampMillis,
    };
    use zinder_proto::{
        capabilities::INGEST_CONTROL_CHAIN_EVENTS_V1,
        v1::{
            ingest::{
                CanonicalWriterStatusRequest, ServerInfoRequest, WriterStatusRequest,
                canonical_control_client::CanonicalControlClient,
                ingest_control_client::IngestControlClient,
            },
            wallet::{
                ChainEventEnvelope, ChainEventStreamFamily, ChainEventsRequest,
                MempoolEventEnvelope, MempoolEventStreamFamily, MempoolEventsRequest,
                MempoolSnapshotRequest, chain_event_envelope, mempool_event_envelope,
            },
        },
    };
    use zinder_runtime::{BearerToken, Readiness, ReadinessState};
    use zinder_source::{NodeAuth, NodeSource, ZebraJsonRpcSource};
    use zinder_store::{
        CanonicalEventCursor, CanonicalEventKind, EventStreamStartPosition, MempoolEvent,
        RocksDbResourceBudget, event_stream_start_message,
    };

    use crate::{
        CanonicalCheckpointStagingRoot, CanonicalControlGrpcAdapter, LiveMempoolOwner,
        writer::control::{
            canonical_control_channel, handle_canonical_control_command,
            test_support::published_fixture_store,
        },
    };

    use super::{CanonicalIngestControlGrpcAdapter, canonical_event_message};

    #[test]
    fn tip_reorg_wire_event_names_the_exact_previous_epoch()
    -> Result<(), Box<dyn std::error::Error>> {
        let previous_epoch = fixture_epoch(7, 100, 0x71);
        let resulting_epoch = fixture_epoch(8, 101, 0x81);
        let event = crate::writer::control::CanonicalIngestEvent {
            cursor: CanonicalEventCursor::at(23)?,
            kind: CanonicalEventKind::Reorged,
            resulting_epoch,
            previous_epoch: Some(previous_epoch),
            reverted_range: Some(BlockHeightRange::inclusive(
                BlockHeight::new(99),
                BlockHeight::new(100),
            )),
            committed_range: BlockHeightRange::inclusive(
                BlockHeight::new(99),
                BlockHeight::new(101),
            ),
        };

        let envelope = canonical_event_message(event)?;
        let Some(chain_event_envelope::Event::ChainReorged(reorg)) = envelope.event else {
            return Err("expected a reorg envelope".into());
        };
        assert_eq!(
            reorg
                .reverted
                .and_then(|reverted| reverted.chain_epoch)
                .map(|epoch| epoch.chain_epoch_id),
            Some(previous_epoch.id.value()),
            "the reverted range must retain the epoch that was displaced"
        );
        assert_eq!(
            reorg
                .committed
                .and_then(|committed| committed.chain_epoch)
                .map(|epoch| epoch.chain_epoch_id),
            Some(resulting_epoch.id.value())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_listener_authenticates_both_control_surfaces_and_serves_history()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fixture = shared_control_fixture().await?;
        assert_control_authentication(&mut fixture.canonical_client, &mut fixture.ingest_client)
            .await?;
        let chain_events = assert_chain_events_contract(&mut fixture.ingest_client).await?;
        let mempool_events = assert_mempool_contract(&mut fixture.ingest_client).await?;

        drop(mempool_events);
        drop(chain_events);
        fixture.shutdown().await
    }

    struct SharedControlFixture {
        temporary: tempfile::TempDir,
        canonical_client: CanonicalControlClient<tonic::transport::Channel>,
        ingest_client: IngestControlClient<tonic::transport::Channel>,
        cancel: CancellationToken,
        server_task: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
        command_task: tokio::task::JoinHandle<()>,
    }

    impl SharedControlFixture {
        async fn shutdown(self) -> Result<(), Box<dyn std::error::Error>> {
            drop(self.ingest_client);
            drop(self.canonical_client);
            self.cancel.cancel();
            self.server_task.await??;
            self.command_task.abort();
            let _ = self.command_task.await;
            drop(self.temporary);
            Ok(())
        }
    }

    async fn shared_control_fixture() -> Result<SharedControlFixture, Box<dyn std::error::Error>> {
        let temporary = tempfile::TempDir::new()?;
        let checkpoint_staging_root = temporary.path().join("checkpoint-staging");
        fs::create_dir(&checkpoint_staging_root)?;
        let mut store = published_fixture_store(&temporary.path().join("canonical"))?;
        let (canonical, mut commands) = canonical_control_channel();
        let command_task = tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                handle_canonical_control_command(&mut store, command);
            }
        });

        let owner = LiveMempoolOwner::default();
        let chain_epoch = canonical.chain_epoch().await?.chain_epoch;
        owner
            .apply_event(
                &canonical,
                MempoolEvent::Added {
                    entry: fixture_mempool_entry(chain_epoch),
                },
                UnixTimestampMillis::new(1_750_000_000_100),
            )
            .await?;
        owner.complete_hydration(&canonical).await?;

        let bearer_token = BearerToken::from_str("fixture-control-token")?;
        let node_source: Arc<dyn NodeSource> = Arc::new(ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashTestnet,
            "http://127.0.0.1:1",
            NodeAuth::None,
            Duration::from_secs(1),
        )?);
        let readiness = Readiness::new(ReadinessState::ready(Some(1)));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let listen_addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let canonical_adapter = CanonicalControlGrpcAdapter::new(
            canonical.clone(),
            CanonicalCheckpointStagingRoot::new(checkpoint_staging_root),
            RocksDbResourceBudget::for_local_tests(),
        )
        .with_bearer_token(Some(bearer_token.clone()));
        let ingest_adapter = CanonicalIngestControlGrpcAdapter::new(
            zinder_core::Network::ZcashTestnet,
            canonical,
            owner,
            node_source,
            readiness,
        )
        .with_bearer_token(Some(bearer_token));
        let server_task = tokio::spawn(async move {
            Server::builder()
                .add_service(canonical_adapter.into_server())
                .add_service(ingest_adapter.into_server())
                .serve_with_incoming_shutdown(
                    TcpListenerStream::new(listener),
                    server_cancel.cancelled_owned(),
                )
                .await
        });

        let endpoint = format!("http://{listen_addr}");
        let canonical_client = CanonicalControlClient::connect(endpoint.clone()).await?;
        let ingest_client = IngestControlClient::connect(endpoint).await?;
        Ok(SharedControlFixture {
            temporary,
            canonical_client,
            ingest_client,
            cancel,
            server_task,
            command_task,
        })
    }

    async fn assert_control_authentication(
        canonical_client: &mut CanonicalControlClient<tonic::transport::Channel>,
        ingest_client: &mut IngestControlClient<tonic::transport::Channel>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            canonical_client
                .writer_status(CanonicalWriterStatusRequest {})
                .await
                .err()
                .map(|status| status.code()),
            Some(Code::Unauthenticated)
        );
        assert_eq!(
            ingest_client
                .writer_status(WriterStatusRequest {})
                .await
                .err()
                .map(|status| status.code()),
            Some(Code::Unauthenticated)
        );

        let canonical_status = canonical_client
            .writer_status(authenticated(CanonicalWriterStatusRequest {}))
            .await?
            .into_inner();
        assert_eq!(
            canonical_status.fence.map(|fence| fence.event_sequence),
            Some(1)
        );
        let ingest_status = ingest_client
            .writer_status(authenticated(WriterStatusRequest {}))
            .await?
            .into_inner();
        assert_eq!(ingest_status.network_name, "zcash-testnet");
        Ok(())
    }

    async fn assert_chain_events_contract(
        ingest_client: &mut IngestControlClient<tonic::transport::Channel>,
    ) -> Result<tonic::Streaming<ChainEventEnvelope>, Box<dyn std::error::Error>> {
        let server_info = ingest_client
            .server_info(authenticated(ServerInfoRequest {}))
            .await?
            .into_inner()
            .server_info
            .ok_or("ingest server info was absent")?;
        assert!(
            !server_info
                .capabilities
                .iter()
                .any(|capability| capability == INGEST_CONTROL_CHAIN_EVENTS_V1),
            "the whole-RPC ChainEvents capability must remain absent while SAFE and address filters are unsupported"
        );

        let mut chain_events = ingest_client
            .chain_events(authenticated(ChainEventsRequest {
                start: Some(event_stream_start_message(
                    &EventStreamStartPosition::EarliestRetained,
                )),
                family: ChainEventStreamFamily::Tip as i32,
                address_filter: Vec::new(),
            }))
            .await?
            .into_inner();
        let first_chain_event = tokio::time::timeout(Duration::from_secs(1), chain_events.next())
            .await?
            .ok_or("tip event stream ended before the fixture event")??;
        assert_eq!(first_chain_event.event_sequence, 1);
        assert!(matches!(
            first_chain_event.event,
            Some(chain_event_envelope::Event::ChainCommitted(_))
        ));

        let safe_status = ingest_client
            .chain_events(authenticated(ChainEventsRequest {
                start: Some(event_stream_start_message(
                    &EventStreamStartPosition::EarliestRetained,
                )),
                family: ChainEventStreamFamily::Safe as i32,
                address_filter: Vec::new(),
            }))
            .await
            .err()
            .ok_or("SAFE chain events unexpectedly succeeded")?;
        assert_eq!(safe_status.code(), Code::Unimplemented);
        let filter_status = ingest_client
            .chain_events(authenticated(ChainEventsRequest {
                start: Some(event_stream_start_message(
                    &EventStreamStartPosition::EarliestRetained,
                )),
                family: ChainEventStreamFamily::Tip as i32,
                address_filter: vec!["t1unsupported".to_owned()],
            }))
            .await
            .err()
            .ok_or("address-filtered chain events unexpectedly succeeded")?;
        assert_eq!(filter_status.code(), Code::Unimplemented);
        Ok(chain_events)
    }

    async fn assert_mempool_contract(
        ingest_client: &mut IngestControlClient<tonic::transport::Channel>,
    ) -> Result<tonic::Streaming<MempoolEventEnvelope>, Box<dyn std::error::Error>> {
        let snapshot = ingest_client
            .mempool_snapshot(authenticated(MempoolSnapshotRequest {
                max_entries: 10,
                from_cursor: Vec::new(),
            }))
            .await?
            .into_inner();
        assert_eq!(snapshot.entries.len(), 1);
        let mut mempool_events = ingest_client
            .mempool_events(authenticated(MempoolEventsRequest {
                start: Some(event_stream_start_message(
                    &EventStreamStartPosition::EarliestRetained,
                )),
                family: MempoolEventStreamFamily::Mempool as i32,
            }))
            .await?
            .into_inner();
        let first_mempool_event =
            tokio::time::timeout(Duration::from_secs(1), mempool_events.next())
                .await?
                .ok_or("mempool event stream ended before the fixture event")??;
        assert!(matches!(
            first_mempool_event.event,
            Some(mempool_event_envelope::Event::Added(_))
        ));
        Ok(mempool_events)
    }

    fn authenticated<Message>(message: Message) -> Request<Message> {
        let mut request = Request::new(message);
        request.metadata_mut().insert(
            "authorization",
            tonic::metadata::MetadataValue::from_static("Bearer fixture-control-token"),
        );
        request
    }

    fn fixture_mempool_entry(chain_epoch: zinder_core::ChainEpoch) -> MempoolEntry {
        MempoolEntry {
            transaction_id: TransactionId::from_bytes([0xA1; 32]),
            auth_digest: None,
            raw_transaction_bytes: RawTransactionBytes::new(vec![0xA1; 8]),
            compact_transaction_bytes: vec![0xA1; 4],
            first_seen_unix_millis: UnixTimestampMillis::new(1_750_000_000_100),
            first_seen_chain_epoch: chain_epoch,
            transparent_outputs: Vec::new(),
            transparent_spends: Vec::new(),
        }
    }

    fn fixture_epoch(id: u64, height: u32, hash_byte: u8) -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(id),
            network: zinder_core::Network::ZcashTestnet,
            visible_tip_height: BlockHeight::new(height),
            visible_tip_hash: BlockHash::from_bytes([hash_byte; 32]),
            settled_tip_height: BlockHeight::new(height.saturating_sub(1)),
            settled_tip_hash: BlockHash::from_bytes([hash_byte.saturating_sub(1); 32]),
            artifact_schema_version: ArtifactSchemaVersion::new(4),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_750_000_000_000),
        }
    }
}
