//! Private ingest control-plane adapter for writer status and chain events.

use std::{pin::Pin, time::Instant};

use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};
use zinder_core::{ChainEpoch, Network};
use zinder_proto::v1::{
    ingest::{
        WriterStatusRequest, WriterStatusResponse,
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    wallet,
};
use zinder_store::{
    ChainEventEncodeError, ChainEventHistoryRequest, ChainEventStreamFamily, PrimaryChainStore,
    StreamCursorTokenV1, chain_event_envelope_message, run_chain_event_stream,
    status_from_store_error,
};

type IngestControlStream<Message> = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;
type ChainEventsStream = IngestControlStream<wallet::ChainEventEnvelope>;

/// Tonic adapter for the private ingest control service.
#[derive(Clone)]
pub struct IngestControlGrpcAdapter {
    network: Network,
    store: PrimaryChainStore,
}

impl IngestControlGrpcAdapter {
    /// Creates an ingest-control adapter over the primary store handle.
    #[must_use]
    pub fn new(network: Network, store: PrimaryChainStore) -> Self {
        Self { network, store }
    }

    /// Converts this adapter into a generated tonic server.
    #[must_use]
    pub fn into_server(self) -> IngestControlServer<Self> {
        IngestControlServer::new(self)
    }
}

#[tonic::async_trait]
impl IngestControl for IngestControlGrpcAdapter {
    type ChainEventsStream = ChainEventsStream;

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
                Ok(Response::new(WriterStatusResponse {
                    network_name: self.network.name().to_owned(),
                    latest_writer_chain_epoch_id: chain_epoch.map(|epoch| epoch.id.value()),
                    latest_writer_tip_height: chain_epoch.map(|epoch| epoch.tip_height.value()),
                    latest_writer_finalized_height: chain_epoch
                        .map(|epoch| epoch.finalized_height.value()),
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
        let from_cursor = cursor_from_request(request.from_cursor);
        let family = chain_event_stream_family_from_request(request.family)?;
        let store = self.store.clone();
        let (event_sender, event_receiver) = mpsc::channel(16);
        tokio::spawn(run_chain_event_stream(
            from_cursor,
            move |cursor| read_chain_event_page(store.clone(), cursor, family),
            event_sender,
        ));

        Ok(Response::new(Box::pin(ReceiverStream::new(event_receiver))))
    }
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

fn cursor_from_request(cursor_bytes: Vec<u8>) -> Option<StreamCursorTokenV1> {
    if cursor_bytes.is_empty() {
        None
    } else {
        Some(StreamCursorTokenV1::from_bytes(cursor_bytes))
    }
}

fn chain_event_stream_family_from_request(family: i32) -> Result<ChainEventStreamFamily, Status> {
    match wallet::ChainEventStreamFamily::try_from(family) {
        Ok(wallet::ChainEventStreamFamily::Tip) => Ok(ChainEventStreamFamily::Tip),
        Ok(wallet::ChainEventStreamFamily::Finalized) => Ok(ChainEventStreamFamily::Finalized),
        Err(_) => Err(Status::invalid_argument(
            "chain-event stream family is unknown",
        )),
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
        "network" => chain_epoch.network.name()
    )
    .set(1.0);
    metrics::gauge!(
        "zinder_ingest_writer_chain_epoch_id",
        "network" => chain_epoch.network.name()
    )
    .set(u64_to_f64(chain_epoch.id.value()));
    metrics::gauge!(
        "zinder_ingest_writer_tip_height",
        "network" => chain_epoch.network.name()
    )
    .set(u32_to_f64(chain_epoch.tip_height.value()));
    metrics::gauge!(
        "zinder_ingest_writer_finalized_height",
        "network" => chain_epoch.network.name()
    )
    .set(u32_to_f64(chain_epoch.finalized_height.value()));
}

fn record_empty_writer_progress(network: Network) {
    metrics::gauge!(
        "zinder_ingest_writer_has_chain_epoch",
        "network" => network.name()
    )
    .set(0.0);
    metrics::gauge!(
        "zinder_ingest_writer_chain_epoch_id",
        "network" => network.name()
    )
    .set(0.0);
    metrics::gauge!(
        "zinder_ingest_writer_tip_height",
        "network" => network.name()
    )
    .set(0.0);
    metrics::gauge!(
        "zinder_ingest_writer_finalized_height",
        "network" => network.name()
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
