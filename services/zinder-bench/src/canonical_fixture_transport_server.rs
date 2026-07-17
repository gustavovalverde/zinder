//! Captured-fixture JSON-RPC and Zebra indexer gRPC transport server.
//!
//! This diagnostic server presents identical immutable block bytes through
//! both source protocols. It never opens Zebra state and cannot certify Zebra
//! database-read cost; it isolates client transport, serialization, admission,
//! parsing, and canonical construction under a controlled response delay.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::Stream;
use jsonrpsee::{
    RpcModule,
    server::{
        ServerBuilder, ServerConfig, ServerHandle,
        middleware::rpc::{
            Batch, Notification, Request as RpcRequest, RpcServiceBuilder, RpcServiceT,
        },
    },
    types::ErrorObjectOwned,
};
use parking_lot::Mutex;
use prost::Message;
use serde::Serialize;
use serde_json::json;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};
use zinder_core::{BlockHeight, BlockId, wire::encode_rpc_block_hash_hex};
use zinder_proto::external::zebra_indexer_rpc::{
    BlockAndHash, BlockHashAndHeight, BlockRequest, Empty, MempoolChangeMessage,
    NonFinalizedStateChangeRequest,
    indexer_server::{Indexer, IndexerServer},
};
use zinder_source::{NodeSource, SourceBlock};

use crate::{
    BenchError,
    fixture::{FixtureManifest, FixtureNodeSource},
};

type ChainTipStream =
    Pin<Box<dyn Stream<Item = Result<BlockHashAndHeight, Status>> + Send + 'static>>;
type BlockStream = Pin<Box<dyn Stream<Item = Result<BlockAndHash, Status>> + Send + 'static>>;
type MempoolStream =
    Pin<Box<dyn Stream<Item = Result<MempoolChangeMessage, Status>> + Send + 'static>>;

/// Local addresses and delay for one captured transport server run.
#[derive(Clone, Debug)]
pub struct CanonicalFixtureTransportServerConfig {
    /// Directory containing the immutable fixture manifest and segments.
    pub fixture_directory: std::path::PathBuf,
    /// JSON-RPC listener used by the current batched source.
    pub json_rpc_listen_addr: SocketAddr,
    /// Zebra indexer gRPC listener used by unary `GetBlock`.
    pub indexer_grpc_listen_addr: SocketAddr,
    /// Fixed delay applied once per JSON request or batch and once per gRPC unary call.
    pub response_delay: Duration,
    /// Maximum JSON-RPC and gRPC response message size.
    pub max_response_bytes: u32,
}

/// Server-side transport measurements emitted after graceful shutdown.
#[derive(Clone, Debug, Serialize)]
pub struct CanonicalFixtureTransportServerReport {
    /// Stable diagnostic identity.
    pub contract_identity: &'static str,
    /// Immutable fixture range.
    pub fixture_from_height: u32,
    /// Immutable fixture range.
    pub fixture_to_height: u32,
    /// Fixed response delay used for the run.
    pub response_delay_millis: u64,
    /// JSON-RPC best-tip method calls.
    pub json_tip_request_count: u64,
    /// Successful JSON-RPC `getblock` batch members.
    pub json_successful_get_block_count: u64,
    /// Raw block bytes represented by successful JSON `getblock` results.
    pub json_raw_block_application_bytes: u64,
    /// Hex characters represented by successful JSON `getblock` results.
    pub json_hex_application_payload_bytes: u64,
    /// Direct JSON requests admitted by the server middleware.
    pub json_single_request_attempt_count: u64,
    /// JSON batches admitted by the server middleware.
    pub json_batch_request_attempt_count: u64,
    /// JSON request and batch timing and concurrency.
    pub json_transport_requests: TransportRequestMeasurements,
    /// Successful unary indexer `GetBlock` responses.
    pub grpc_successful_get_block_count: u64,
    /// Raw block bytes represented by successful gRPC responses.
    pub grpc_raw_block_application_bytes: u64,
    /// Encoded protobuf message bytes, excluding the five-byte gRPC prefix.
    pub grpc_protobuf_application_bytes: u64,
    /// Unary gRPC request timing and concurrency.
    pub grpc_transport_requests: TransportRequestMeasurements,
}

/// Timing and concurrency for one server-side transport request unit.
#[derive(Clone, Debug, Serialize)]
pub struct TransportRequestMeasurements {
    /// Requests or batches admitted by the server.
    pub attempt_count: u64,
    /// Requests or batches that returned a transport response.
    pub completion_count: u64,
    /// Requests or batches dropped before returning a response.
    pub cancelled_count: u64,
    /// First-admission to last-completion interval.
    pub interval_seconds: f64,
    /// Sum of all individual request service durations.
    pub cumulative_active_seconds: f64,
    /// Time-weighted mean active requests over the interval.
    pub effective_concurrency: f64,
    /// Maximum simultaneously active requests.
    pub max_active_requests: u64,
    /// Mean server-side request latency.
    pub average_latency_seconds: f64,
    /// Nearest-rank p95 server-side request latency.
    pub p95_latency_seconds: f64,
}

#[derive(Clone)]
struct FixtureTransportState {
    source: FixtureNodeSource,
    tip: BlockId,
    response_delay: Duration,
    stats: Arc<TransportServerStats>,
}

#[derive(Default)]
struct TransportServerStats {
    json_tip_requests: AtomicU64,
    json_successful_get_blocks: AtomicU64,
    json_raw_block_bytes: AtomicU64,
    json_hex_payload_bytes: AtomicU64,
    json_single_request_attempts: Arc<AtomicU64>,
    json_batch_request_attempts: Arc<AtomicU64>,
    json_transport_requests: Arc<TransportRequestStats>,
    grpc_successful_get_blocks: AtomicU64,
    grpc_raw_block_bytes: AtomicU64,
    grpc_protobuf_message_bytes: AtomicU64,
    grpc_transport_requests: Arc<TransportRequestStats>,
}

#[derive(Default)]
struct TransportRequestStats {
    attempts: AtomicU64,
    completions: AtomicU64,
    cancellations: AtomicU64,
    active: AtomicU64,
    max_active: AtomicU64,
    first_started_at: Mutex<Option<Instant>>,
    last_completed_at: Mutex<Option<Instant>>,
    cumulative_active_time: Mutex<Duration>,
    latencies_seconds: Mutex<Vec<f64>>,
}

struct RunningTransportServers {
    json_rpc_handle: ServerHandle,
    indexer_grpc_handle: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

#[derive(Clone)]
struct FixtureResponseDelay<S> {
    service: S,
    delay: Duration,
    stats: Arc<TransportRequestStats>,
    single_request_attempts: Arc<AtomicU64>,
    batch_request_attempts: Arc<AtomicU64>,
}

impl<S> RpcServiceT for FixtureResponseDelay<S>
where
    S: RpcServiceT + Clone + Send + Sync + 'static,
{
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;
    type BatchResponse = S::BatchResponse;

    fn call<'a>(
        &self,
        request: RpcRequest<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let service = self.service.clone();
        let delay = self.delay;
        let stats = Arc::clone(&self.stats);
        let single_request_attempts = Arc::clone(&self.single_request_attempts);
        async move {
            single_request_attempts.fetch_add(1, Ordering::Relaxed);
            let active_request = TransportRequestGuard::start(stats);
            delay_response(delay).await;
            let response = service.call(request).await;
            active_request.complete();
            response
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let service = self.service.clone();
        let delay = self.delay;
        let stats = Arc::clone(&self.stats);
        let batch_request_attempts = Arc::clone(&self.batch_request_attempts);
        async move {
            batch_request_attempts.fetch_add(1, Ordering::Relaxed);
            let active_request = TransportRequestGuard::start(stats);
            delay_response(delay).await;
            let response = service.batch(batch).await;
            active_request.complete();
            response
        }
    }

    fn notification<'a>(
        &self,
        notification: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.service.notification(notification)
    }
}

/// Runs both fixture protocols until the process receives Ctrl-C.
pub async fn run_canonical_fixture_transport_server(
    config: CanonicalFixtureTransportServerConfig,
) -> Result<CanonicalFixtureTransportServerReport, BenchError> {
    if config.response_delay.as_millis() > u128::from(u64::MAX) {
        return Err(BenchError::invalid_argument(
            "fixture transport response delay exceeds u64 milliseconds",
        ));
    }
    let manifest = FixtureManifest::read(&config.fixture_directory)?;
    let source = FixtureNodeSource::open(&config.fixture_directory, &manifest)?;
    let state = FixtureTransportState {
        source,
        tip: manifest.tip_id()?,
        response_delay: config.response_delay,
        stats: Arc::new(TransportServerStats::default()),
    };
    let servers = start_transport_servers(&config, state.clone()).await?;
    tracing::info!(
        target: "zinder::bench",
        event = "canonical_fixture_transport_server_ready",
        json_rpc_listen_addr = %config.json_rpc_listen_addr,
        indexer_grpc_listen_addr = %config.indexer_grpc_listen_addr,
        response_delay_millis = config.response_delay.as_millis(),
        "captured canonical fixture transport server is ready"
    );
    tokio::signal::ctrl_c()
        .await
        .map_err(|source| BenchError::io("ctrl-c", source))?;
    servers
        .json_rpc_handle
        .stop()
        .map_err(|error| BenchError::invalid_argument(error.to_string()))?;
    servers.indexer_grpc_handle.abort();
    Ok(build_report(&manifest, &config, &state.stats))
}

async fn start_transport_servers(
    config: &CanonicalFixtureTransportServerConfig,
    state: FixtureTransportState,
) -> Result<RunningTransportServers, BenchError> {
    let json_rpc_config = ServerConfig::builder()
        .max_response_body_size(config.max_response_bytes)
        .build();
    let response_delay = config.response_delay;
    let json_transport_requests = Arc::clone(&state.stats.json_transport_requests);
    let json_single_request_attempts = Arc::clone(&state.stats.json_single_request_attempts);
    let json_batch_request_attempts = Arc::clone(&state.stats.json_batch_request_attempts);
    let response_delay_middleware =
        RpcServiceBuilder::new().layer_fn(move |service| FixtureResponseDelay {
            service,
            delay: response_delay,
            stats: Arc::clone(&json_transport_requests),
            single_request_attempts: Arc::clone(&json_single_request_attempts),
            batch_request_attempts: Arc::clone(&json_batch_request_attempts),
        });
    let json_rpc_server = ServerBuilder::with_config(json_rpc_config)
        .set_rpc_middleware(response_delay_middleware)
        .build(config.json_rpc_listen_addr)
        .await
        .map_err(|error| {
            BenchError::invalid_argument(format!("JSON-RPC listener failed: {error}"))
        })?;
    let mut module = RpcModule::new(state.clone());
    module
        .register_async_method("getbestblockheightandhash", |_, state, _| async move {
            state
                .stats
                .json_tip_requests
                .fetch_add(1, Ordering::Relaxed);
            Ok::<_, ErrorObjectOwned>(json!({
                "height": state.tip.height.value(),
                "hash": state.tip.hash.as_bytes(),
            }))
        })
        .map_err(|error| BenchError::invalid_argument(error.to_string()))?;
    module
        .register_async_method("getblock", |params, state, _| async move {
            let (height_text, verbosity): (String, u8) = params.parse()?;
            if verbosity != 0 {
                return Err(rpc_error(
                    "fixture transport supports only getblock verbosity 0",
                ));
            }
            let height_value = height_text
                .parse::<u32>()
                .map_err(|_| rpc_error("getblock height must be a u32 string"))?;
            let block = state
                .source
                .fetch_block_at(BlockHeight::new(height_value))
                .await
                .map_err(|error| rpc_error(&error.to_string()))?;
            record_json_block(&state.stats, &block);
            Ok::<_, ErrorObjectOwned>(hex::encode(block.raw_block_bytes))
        })
        .map_err(|error| BenchError::invalid_argument(error.to_string()))?;
    let json_rpc_handle = json_rpc_server.start(module);

    let listener = tokio::net::TcpListener::bind(config.indexer_grpc_listen_addr)
        .await
        .map_err(|source| BenchError::io(config.indexer_grpc_listen_addr.to_string(), source))?;
    let grpc_service = FixtureIndexerService { state };
    let max_response_bytes = usize::try_from(config.max_response_bytes).unwrap_or(usize::MAX);
    let indexer_grpc_handle = tokio::spawn(
        Server::builder()
            .add_service(
                IndexerServer::new(grpc_service).max_encoding_message_size(max_response_bytes),
            )
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    Ok(RunningTransportServers {
        json_rpc_handle,
        indexer_grpc_handle,
    })
}

#[derive(Clone)]
struct FixtureIndexerService {
    state: FixtureTransportState,
}

#[tonic::async_trait]
impl Indexer for FixtureIndexerService {
    type ChainTipChangeStream = ChainTipStream;
    type NonFinalizedStateChangeStream = BlockStream;
    type MempoolChangeStream = MempoolStream;

    async fn chain_tip_change(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ChainTipChangeStream>, Status> {
        Err(Status::unimplemented(
            "fixture transport server exposes only GetBlock",
        ))
    }

    async fn non_finalized_state_change(
        &self,
        _request: Request<NonFinalizedStateChangeRequest>,
    ) -> Result<Response<Self::NonFinalizedStateChangeStream>, Status> {
        Err(Status::unimplemented(
            "fixture transport server exposes only GetBlock",
        ))
    }

    async fn mempool_change(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::MempoolChangeStream>, Status> {
        Err(Status::unimplemented(
            "fixture transport server exposes only GetBlock",
        ))
    }

    async fn get_block(
        &self,
        request: Request<BlockRequest>,
    ) -> Result<Response<BlockAndHash>, Status> {
        let active_request =
            TransportRequestGuard::start(Arc::clone(&self.state.stats.grpc_transport_requests));
        let outcome = self.get_block_inner(request.into_inner()).await;
        active_request.complete();
        outcome.map(Response::new)
    }
}

impl FixtureIndexerService {
    async fn get_block_inner(&self, request: BlockRequest) -> Result<BlockAndHash, Status> {
        let height_bytes: [u8; 4] = request.hash_or_height.try_into().map_err(|_| {
            Status::invalid_argument("fixture GetBlock requires a four-byte height")
        })?;
        let height = BlockHeight::new(u32::from_be_bytes(height_bytes));
        delay_response(self.state.response_delay).await;
        let block = self
            .state
            .source
            .fetch_block_at(height)
            .await
            .map_err(|error| Status::not_found(error.to_string()))?;
        let hash = hex::decode(encode_rpc_block_hash_hex(block.hash))
            .map_err(|error| Status::internal(error.to_string()))?;
        let response = BlockAndHash {
            hash,
            data: block.raw_block_bytes,
        };
        self.state
            .stats
            .grpc_successful_get_blocks
            .fetch_add(1, Ordering::Relaxed);
        self.state.stats.grpc_raw_block_bytes.fetch_add(
            u64::try_from(response.data.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.state.stats.grpc_protobuf_message_bytes.fetch_add(
            u64::try_from(response.encoded_len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Ok(response)
    }
}

async fn delay_response(delay: Duration) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

struct TransportRequestGuard {
    stats: Arc<TransportRequestStats>,
    started_at: Instant,
    completed: bool,
}

impl TransportRequestGuard {
    fn start(stats: Arc<TransportRequestStats>) -> Self {
        let started_at = Instant::now();
        stats.attempts.fetch_add(1, Ordering::Relaxed);
        let active = stats
            .active
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        stats.max_active.fetch_max(active, Ordering::Relaxed);
        let mut first_started_at = stats.first_started_at.lock();
        if first_started_at.is_none() {
            *first_started_at = Some(started_at);
        }
        drop(first_started_at);
        Self {
            stats,
            started_at,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.completed = true;
    }
}

impl Drop for TransportRequestGuard {
    fn drop(&mut self) {
        let completed_at = Instant::now();
        let elapsed = completed_at.saturating_duration_since(self.started_at);
        self.stats.active.fetch_sub(1, Ordering::Relaxed);
        if self.completed {
            self.stats.completions.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.cancellations.fetch_add(1, Ordering::Relaxed);
        }
        *self.stats.last_completed_at.lock() = Some(completed_at);
        *self.stats.cumulative_active_time.lock() += elapsed;
        self.stats
            .latencies_seconds
            .lock()
            .push(elapsed.as_secs_f64());
    }
}

fn record_json_block(stats: &TransportServerStats, block: &SourceBlock) {
    let raw_bytes = u64::try_from(block.raw_block_bytes.len()).unwrap_or(u64::MAX);
    stats
        .json_successful_get_blocks
        .fetch_add(1, Ordering::Relaxed);
    stats
        .json_raw_block_bytes
        .fetch_add(raw_bytes, Ordering::Relaxed);
    stats
        .json_hex_payload_bytes
        .fetch_add(raw_bytes.saturating_mul(2), Ordering::Relaxed);
}

fn rpc_error(message: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-8, message, None::<()>)
}

fn build_report(
    manifest: &FixtureManifest,
    config: &CanonicalFixtureTransportServerConfig,
    stats: &TransportServerStats,
) -> CanonicalFixtureTransportServerReport {
    CanonicalFixtureTransportServerReport {
        contract_identity: "canonical-fixture-transport-server-v1",
        fixture_from_height: manifest.from_height,
        fixture_to_height: manifest.to_height,
        response_delay_millis: u64::try_from(config.response_delay.as_millis()).unwrap_or(u64::MAX),
        json_tip_request_count: stats.json_tip_requests.load(Ordering::Relaxed),
        json_successful_get_block_count: stats.json_successful_get_blocks.load(Ordering::Relaxed),
        json_raw_block_application_bytes: stats.json_raw_block_bytes.load(Ordering::Relaxed),
        json_hex_application_payload_bytes: stats.json_hex_payload_bytes.load(Ordering::Relaxed),
        json_single_request_attempt_count: stats
            .json_single_request_attempts
            .load(Ordering::Relaxed),
        json_batch_request_attempt_count: stats.json_batch_request_attempts.load(Ordering::Relaxed),
        json_transport_requests: build_transport_request_measurements(
            &stats.json_transport_requests,
        ),
        grpc_successful_get_block_count: stats.grpc_successful_get_blocks.load(Ordering::Relaxed),
        grpc_raw_block_application_bytes: stats.grpc_raw_block_bytes.load(Ordering::Relaxed),
        grpc_protobuf_application_bytes: stats.grpc_protobuf_message_bytes.load(Ordering::Relaxed),
        grpc_transport_requests: build_transport_request_measurements(
            &stats.grpc_transport_requests,
        ),
    }
}

fn build_transport_request_measurements(
    stats: &TransportRequestStats,
) -> TransportRequestMeasurements {
    let mut latencies = stats.latencies_seconds.lock().clone();
    latencies.sort_by(f64::total_cmp);
    let cumulative_active_seconds = stats.cumulative_active_time.lock().as_secs_f64();
    let interval_seconds = stats
        .first_started_at
        .lock()
        .zip(*stats.last_completed_at.lock())
        .map_or(0.0, |(first, last)| {
            last.saturating_duration_since(first).as_secs_f64()
        });
    let average_latency = if latencies.is_empty() {
        0.0
    } else {
        latencies.iter().sum::<f64>() / usize_to_f64(latencies.len())
    };
    let effective_concurrency = if interval_seconds == 0.0 {
        0.0
    } else {
        cumulative_active_seconds / interval_seconds
    };
    TransportRequestMeasurements {
        attempt_count: stats.attempts.load(Ordering::Relaxed),
        completion_count: stats.completions.load(Ordering::Relaxed),
        cancelled_count: stats.cancellations.load(Ordering::Relaxed),
        interval_seconds,
        cumulative_active_seconds,
        effective_concurrency,
        max_active_requests: stats.max_active.load(Ordering::Relaxed),
        average_latency_seconds: average_latency,
        p95_latency_seconds: nearest_rank_p95(&latencies),
    }
}

fn nearest_rank_p95(sorted_samples: &[f64]) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let rank = sorted_samples.len().saturating_mul(95).saturating_add(99) / 100;
    sorted_samples
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or(0.0)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "latency sample counts are diagnostic and represented as f64 means"
)]
fn usize_to_f64(sample: usize) -> f64 {
    sample as f64
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        num::NonZeroU64,
        sync::{Arc, atomic::AtomicU64},
        time::Instant,
    };

    use jsonrpsee::{
        RpcModule,
        core::{client::ClientT, params::BatchRequestBuilder},
        http_client::HeaderMap,
        server::{ServerBuilder, middleware::rpc::RpcServiceBuilder},
    };
    use zinder_source::build_zebra_json_rpc_client;

    use super::FixtureResponseDelay;

    #[tokio::test]
    async fn json_rpc_batch_applies_injected_delay_once() -> Result<(), Box<dyn Error>> {
        let delay = std::time::Duration::from_millis(100);
        let stats = Arc::new(super::TransportRequestStats::default());
        let single_request_attempts = Arc::new(AtomicU64::new(0));
        let batch_request_attempts = Arc::new(AtomicU64::new(0));
        let observed_stats = Arc::clone(&stats);
        let observed_batch_request_attempts = Arc::clone(&batch_request_attempts);
        let middleware = RpcServiceBuilder::new().layer_fn(move |service| FixtureResponseDelay {
            service,
            delay,
            stats: Arc::clone(&stats),
            single_request_attempts: Arc::clone(&single_request_attempts),
            batch_request_attempts: Arc::clone(&batch_request_attempts),
        });
        let server = ServerBuilder::default()
            .set_rpc_middleware(middleware)
            .build("127.0.0.1:0")
            .await?;
        let address = server.local_addr()?;
        let mut module = RpcModule::new(());
        module.register_method("getblock", |_, (), _| "00")?;
        let handle = server.start(module);
        let client = build_zebra_json_rpc_client(
            &format!("http://{address}"),
            std::time::Duration::from_secs(1),
            NonZeroU64::new(1_048_576).ok_or("one MiB is nonzero")?,
            HeaderMap::new(),
        )?;
        let mut batch = BatchRequestBuilder::new();
        for _ in 0..4 {
            batch.insert("getblock", jsonrpsee::rpc_params![])?;
        }

        let started_at = Instant::now();
        let response = client.batch_request::<String>(batch).await?;
        let elapsed = started_at.elapsed();
        handle.stop()?;

        assert_eq!(response.num_successful_calls(), 4);
        assert!(elapsed >= delay, "elapsed {elapsed:?} was below {delay:?}");
        assert!(
            elapsed < delay.saturating_mul(3),
            "elapsed {elapsed:?} multiplied the delay across batch members"
        );
        let measurements = super::build_transport_request_measurements(&observed_stats);
        assert_eq!(
            observed_batch_request_attempts.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(measurements.attempt_count, 1);
        assert_eq!(measurements.completion_count, 1);
        assert_eq!(measurements.max_active_requests, 1);
        Ok(())
    }
}
