//! Background tasks that keep wallet-query readiness in sync with canonical
//! storage.
//!
//! Without these tasks, `zinder-query` and `zinder-compat-lightwalletd` would
//! report the visible chain tip they observed at startup forever, causing
//! `/readyz` and `/metrics` to drift behind ingest. Production readers use the
//! secondary catchup task; the primary-store refresh task remains for
//! in-process tests and development composition.

use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint};
use zinder_core::{BlockHeight, ChainEpochId, Network};
use zinder_proto::v1::ingest::{WriterStatusRequest, ingest_control_client::IngestControlClient};
use zinder_runtime::{Readiness, ReadinessCause, ReadinessState};
use zinder_store::{PrimaryChainStore, SecondaryCatchupOutcome, SecondaryChainStore, StoreError};

/// Default interval between readiness refresh polls.
///
/// One second matches `tip_follow.poll_interval_ms` in `zinder-ingest`, so
/// query/compat readiness lags ingest by at most one tick.
pub const DEFAULT_READINESS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const WRITER_STATUS_METHOD: &str = "zinder.v1.ingest.IngestControl/WriterStatus";

/// Maximum interval between writer-status RPCs while the chain is idle.
///
/// When secondary catchup reports no advance and the previous writer-status
/// snapshot already matches the reader's visible epoch, the next writer-status
/// RPC is deferred until this interval elapses. Heartbeat refreshes the
/// liveness gauges and catches writer outages even when nothing has changed.
const WRITER_STATUS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Private writer-status upstream used by secondary readers to compute replica lag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterStatusConfig {
    /// HTTP/2 endpoint for `zinder-ingest`'s private ingest control-plane gRPC server.
    pub endpoint: String,
    /// Expected writer network. A response for any other network is rejected.
    pub network: Network,
}

/// Runtime options for the secondary catchup task.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecondaryCatchupOptions {
    /// Delay between secondary `RocksDB` catchup attempts.
    pub interval: Duration,
    /// Maximum acceptable lag between writer and secondary chain-epoch ids.
    pub lag_threshold_chain_epochs: u64,
    /// Optional private ingest writer-status upstream used to compute replica lag.
    pub writer_status: Option<WriterStatusConfig>,
}

/// Spawns a task that refreshes `readiness` from `store` every `interval`.
///
/// The task exits when `cancel` fires. A successful read with a visible epoch
/// updates readiness to [`ReadinessState::ready`] with the new tip height; a
/// successful read with no visible epoch leaves readiness untouched (preserving
/// the startup snapshot); a failed read marks the service not-ready with
/// [`ReadinessCause::StorageUnavailable`].
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub fn spawn_readiness_refresh(
    store: PrimaryChainStore,
    readiness: Readiness,
    interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(interval) => refresh_visible_height(&store, &readiness),
            }
        }
    })
}

/// Spawns a task that catches a `RocksDB` secondary up with its primary and refreshes readiness.
///
/// When `options.writer_status` is `Some`, readiness uses the private ingest
/// writer-status RPC to compare the writer's latest committed chain epoch with
/// this secondary's replayed visible epoch. When it is `None`, the task only
/// refreshes readiness from local secondary state; tests and in-process
/// development profiles use that mode.
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub fn spawn_secondary_catchup(
    store: SecondaryChainStore,
    readiness: Readiness,
    options: SecondaryCatchupOptions,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut writer_status_upstream = options.writer_status.map(WriterStatusUpstream::new);
        loop {
            catch_up_secondary(
                &store,
                &readiness,
                options.lag_threshold_chain_epochs,
                writer_status_upstream.as_mut(),
            )
            .await;

            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(options.interval) => {},
            }
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WriterStatusSnapshot {
    chain_epoch_id: Option<ChainEpochId>,
    tip_height: Option<BlockHeight>,
    finalized_height: Option<BlockHeight>,
}

struct WriterStatusUpstream {
    endpoint: String,
    network: Network,
    client: Option<IngestControlClient<Channel>>,
    last_snapshot: Option<WriterStatusSnapshot>,
    last_fetched_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriterStatusRead {
    Disabled,
    Snapshot(WriterStatusSnapshot),
    Unavailable,
}

impl WriterStatusUpstream {
    fn new(config: WriterStatusConfig) -> Self {
        Self {
            endpoint: config.endpoint,
            network: config.network,
            client: None,
            last_snapshot: None,
            last_fetched_at: None,
        }
    }

    async fn fetch_snapshot(&mut self) -> Result<WriterStatusSnapshot, WriterStatusFetchError> {
        if self.client.is_none() {
            let endpoint = Endpoint::from_shared(self.endpoint.clone()).map_err(|source| {
                WriterStatusFetchError::InvalidEndpoint {
                    endpoint: self.endpoint.clone(),
                    source,
                }
            })?;
            let channel =
                endpoint
                    .connect()
                    .await
                    .map_err(|source| WriterStatusFetchError::Connect {
                        endpoint: self.endpoint.clone(),
                        source,
                    })?;
            self.client = Some(IngestControlClient::new(channel));
        }

        let Some(client) = self.client.as_mut() else {
            return Err(WriterStatusFetchError::ClientMissing);
        };
        let response = match client.writer_status(WriterStatusRequest {}).await {
            Ok(response) => response.into_inner(),
            Err(source) => {
                self.client = None;
                return Err(WriterStatusFetchError::Rpc {
                    endpoint: self.endpoint.clone(),
                    method: WRITER_STATUS_METHOD,
                    source,
                });
            }
        };
        if response.network_name != self.network.name() {
            self.client = None;
            return Err(WriterStatusFetchError::NetworkMismatch {
                endpoint: self.endpoint.clone(),
                method: WRITER_STATUS_METHOD,
                expected: self.network.name(),
                actual: response.network_name,
            });
        }

        let snapshot = WriterStatusSnapshot {
            chain_epoch_id: response.latest_writer_chain_epoch_id.map(ChainEpochId::new),
            tip_height: response.latest_writer_tip_height.map(BlockHeight::new),
            finalized_height: response
                .latest_writer_finalized_height
                .map(BlockHeight::new),
        };
        self.last_snapshot = Some(snapshot);
        self.last_fetched_at = Some(Instant::now());
        Ok(snapshot)
    }

    const fn last_snapshot(&self) -> Option<WriterStatusSnapshot> {
        self.last_snapshot
    }

    /// Returns whether the previous writer-status snapshot still matches
    /// `reader_chain_epoch_id` AND was fetched within the heartbeat window.
    ///
    /// When this returns `true`, the catchup loop can skip a writer-status
    /// RPC because no new information would change the readiness state.
    fn snapshot_matches_within_heartbeat(
        &self,
        reader_chain_epoch_id: Option<ChainEpochId>,
    ) -> bool {
        let Some(last_fetched_at) = self.last_fetched_at else {
            return false;
        };
        if last_fetched_at.elapsed() >= WRITER_STATUS_HEARTBEAT_INTERVAL {
            return false;
        }
        let Some(last_snapshot) = self.last_snapshot else {
            return false;
        };
        last_snapshot.chain_epoch_id == reader_chain_epoch_id
    }
}

#[derive(Debug, Error)]
enum WriterStatusFetchError {
    #[error("invalid writer-status endpoint {endpoint}: {source}")]
    InvalidEndpoint {
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },
    #[error("could not connect to writer-status endpoint {endpoint}: {source}")]
    Connect {
        endpoint: String,
        #[source]
        source: tonic::transport::Error,
    },
    #[error("writer-status client was not initialized")]
    ClientMissing,
    #[error("writer-status RPC {method} at {endpoint} failed: {source}")]
    Rpc {
        endpoint: String,
        method: &'static str,
        #[source]
        source: tonic::Status,
    },
    #[error(
        "writer-status RPC {method} at {endpoint} returned network mismatch: expected {expected}, got {actual}"
    )]
    NetworkMismatch {
        endpoint: String,
        method: &'static str,
        expected: &'static str,
        actual: String,
    },
}

fn refresh_visible_height(store: &PrimaryChainStore, readiness: &Readiness) {
    match store.current_chain_epoch() {
        Ok(Some(chain_epoch)) => {
            readiness.set(ReadinessState::ready(Some(chain_epoch.tip_height.value())));
        }
        Ok(None) => {
            // Store has no visible epoch yet; preserve the startup snapshot
            // so readiness does not flap to "ready, height=None" once a real
            // height has already been observed.
        }
        Err(error) => {
            tracing::warn!(
                target: "zinder::query",
                event = "readiness_refresh_failed",
                error = %error,
                "readiness refresh failed; marking storage_unavailable"
            );
            readiness.set(ReadinessState::not_ready(
                ReadinessCause::StorageUnavailable,
            ));
        }
    }
}

async fn catch_up_secondary(
    store: &SecondaryChainStore,
    readiness: &Readiness,
    lag_threshold_chain_epochs: u64,
    writer_status_upstream: Option<&mut WriterStatusUpstream>,
) {
    let started_at = Instant::now();
    let catchup_outcome = catch_up_secondary_store(store);
    record_secondary_catchup_outcome(started_at, &catchup_outcome);
    match catchup_outcome {
        Ok((catchup, current_chain_epoch)) => {
            let current_chain_epoch_id = current_chain_epoch.map(|epoch| epoch.id);
            if catchup.before == catchup.after
                && writer_status_upstream.as_deref().is_some_and(|upstream| {
                    upstream.snapshot_matches_within_heartbeat(current_chain_epoch_id)
                })
            {
                // Nothing advanced and the previous writer-status snapshot
                // still matches; skip the writer-status RPC and gauge updates.
                // Heartbeat will refresh both within
                // `WRITER_STATUS_HEARTBEAT_INTERVAL`.
                return;
            }

            record_secondary_progress(current_chain_epoch);
            let current_height = current_chain_epoch.map(|epoch| epoch.tip_height.value());
            let writer_status_read = writer_status_read(writer_status_upstream).await;
            update_secondary_readiness(
                readiness,
                current_chain_epoch_id,
                current_height,
                lag_threshold_chain_epochs,
                writer_status_read,
            );
        }
        Err(error) => {
            tracing::warn!(
                target: "zinder::query",
                event = "secondary_catchup_failed",
                error = %error,
                "secondary catchup failed; marking storage_unavailable"
            );
            readiness.set(ReadinessState::not_ready(
                ReadinessCause::StorageUnavailable,
            ));
        }
    }
}

fn catch_up_secondary_store(
    store: &SecondaryChainStore,
) -> Result<(SecondaryCatchupOutcome, Option<zinder_core::ChainEpoch>), zinder_store::StoreError> {
    let catchup = store.try_catch_up()?;
    let current_chain_epoch = store.current_chain_epoch()?;
    Ok((catchup, current_chain_epoch))
}

async fn writer_status_read(
    writer_status_upstream: Option<&mut WriterStatusUpstream>,
) -> WriterStatusRead {
    let Some(upstream) = writer_status_upstream else {
        return WriterStatusRead::Disabled;
    };

    let started_at = Instant::now();
    let fetch_outcome = upstream.fetch_snapshot().await;
    record_writer_status_fetch_outcome(started_at, &fetch_outcome);
    match fetch_outcome {
        Ok(snapshot) => {
            record_writer_status_snapshot(snapshot);
            WriterStatusRead::Snapshot(snapshot)
        }
        Err(error) => {
            tracing::warn!(
                target: "zinder::query",
                event = "writer_status_fetch_failed",
                endpoint = %error.endpoint(),
                method = %error.method(),
                error = %error,
                "writer-status fetch failed"
            );
            upstream
                .last_snapshot()
                .map_or(WriterStatusRead::Unavailable, WriterStatusRead::Snapshot)
        }
    }
}

fn update_secondary_readiness(
    readiness: &Readiness,
    current_chain_epoch_id: Option<ChainEpochId>,
    current_height: Option<u32>,
    lag_threshold_chain_epochs: u64,
    writer_status_read: WriterStatusRead,
) {
    let writer_status_snapshot = match writer_status_read {
        WriterStatusRead::Disabled => {
            record_secondary_replica_lag(0);
            readiness.set(ReadinessState::ready(current_height));
            return;
        }
        WriterStatusRead::Unavailable => {
            readiness.set(ReadinessState::not_ready(
                ReadinessCause::WriterStatusUnavailable,
            ));
            return;
        }
        WriterStatusRead::Snapshot(snapshot) => snapshot,
    };
    let Some(latest_writer_chain_epoch_id) = writer_status_snapshot.chain_epoch_id else {
        record_secondary_replica_lag(0);
        readiness.set(ReadinessState::ready(current_height));
        return;
    };

    let reader_chain_epoch_id = current_chain_epoch_id.map_or(0, ChainEpochId::value);
    let lag_chain_epochs = latest_writer_chain_epoch_id
        .value()
        .saturating_sub(reader_chain_epoch_id);
    record_secondary_replica_lag(lag_chain_epochs);

    if lag_chain_epochs > lag_threshold_chain_epochs {
        readiness.set(ReadinessState::replica_lagging(
            lag_chain_epochs,
            current_height,
        ));
        return;
    }

    if current_chain_epoch_id.is_some() {
        readiness.set(ReadinessState::ready(current_height));
    } else {
        readiness.set(ReadinessState::syncing(
            None,
            current_height,
            writer_status_snapshot.tip_height.map(BlockHeight::value),
        ));
    }
}

fn record_secondary_catchup_outcome(
    started_at: Instant,
    catchup_outcome: &Result<
        (SecondaryCatchupOutcome, Option<zinder_core::ChainEpoch>),
        StoreError,
    >,
) {
    metrics::histogram!(
        "zinder_query_secondary_catchup_duration_seconds",
        "status" => outcome_status(catchup_outcome),
        "error_class" => store_error_class(catchup_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_query_secondary_catchup_total",
        "status" => outcome_status(catchup_outcome),
        "error_class" => store_error_class(catchup_outcome.as_ref().err())
    )
    .increment(1);
}

fn record_secondary_progress(current_chain_epoch: Option<zinder_core::ChainEpoch>) {
    let (has_visible_epoch, chain_epoch_id, tip_height) =
        current_chain_epoch.map_or((0.0, 0.0, 0.0), |chain_epoch| {
            (
                1.0,
                u64_to_f64(chain_epoch.id.value()),
                u32_to_f64(chain_epoch.tip_height.value()),
            )
        });

    metrics::gauge!("zinder_query_secondary_has_visible_epoch").set(has_visible_epoch);
    metrics::gauge!("zinder_query_secondary_chain_epoch_id").set(chain_epoch_id);
    metrics::gauge!("zinder_query_secondary_tip_height").set(tip_height);
}

fn record_secondary_replica_lag(lag_chain_epochs: u64) {
    metrics::gauge!("zinder_query_secondary_replica_lag_chain_epochs")
        .set(u64_to_f64(lag_chain_epochs));
}

fn record_writer_status_fetch_outcome(
    started_at: Instant,
    fetch_outcome: &Result<WriterStatusSnapshot, WriterStatusFetchError>,
) {
    metrics::histogram!(
        "zinder_query_writer_status_request_duration_seconds",
        "status" => outcome_status(fetch_outcome),
        "error_class" => writer_status_error_class(fetch_outcome.as_ref().err())
    )
    .record(started_at.elapsed());
    metrics::counter!(
        "zinder_query_writer_status_request_total",
        "status" => outcome_status(fetch_outcome),
        "error_class" => writer_status_error_class(fetch_outcome.as_ref().err())
    )
    .increment(1);
    metrics::gauge!("zinder_query_writer_status_available").set(if fetch_outcome.is_ok() {
        1.0
    } else {
        0.0
    });
}

fn record_writer_status_snapshot(snapshot: WriterStatusSnapshot) {
    let (has_chain_epoch, chain_epoch_id, tip_height, finalized_height) = snapshot
        .chain_epoch_id
        .map_or((0.0, 0.0, 0.0, 0.0), |chain_epoch_id| {
            (
                1.0,
                u64_to_f64(chain_epoch_id.value()),
                snapshot
                    .tip_height
                    .map_or(0.0, |height| u32_to_f64(height.value())),
                snapshot
                    .finalized_height
                    .map_or(0.0, |height| u32_to_f64(height.value())),
            )
        });

    metrics::gauge!("zinder_query_writer_status_has_chain_epoch").set(has_chain_epoch);
    metrics::gauge!("zinder_query_writer_status_chain_epoch_id").set(chain_epoch_id);
    metrics::gauge!("zinder_query_writer_status_tip_height").set(tip_height);
    metrics::gauge!("zinder_query_writer_status_finalized_height").set(finalized_height);
}

const fn outcome_status<T, E>(outcome: &Result<T, E>) -> &'static str {
    if outcome.is_ok() { "ok" } else { "error" }
}

fn store_error_class(error: Option<&StoreError>) -> &'static str {
    match error {
        None => "none",
        Some(StoreError::StorageUnavailable { .. }) => "storage_unavailable",
        Some(StoreError::EntropyUnavailable { .. }) => "entropy_unavailable",
        Some(StoreError::ChainEpochMissing { .. }) => "chain_epoch_missing",
        Some(StoreError::NoVisibleChainEpoch) => "no_visible_chain_epoch",
        Some(StoreError::ChainEpochConflict { .. }) => "chain_epoch_conflict",
        Some(StoreError::ChainEpochNetworkMismatch { .. }) => "chain_epoch_network_mismatch",
        Some(StoreError::SchemaMismatch { .. }) => "schema_mismatch",
        Some(StoreError::SchemaTooNew { .. }) => "schema_too_new",
        Some(StoreError::PrimaryAlreadyOpen { .. }) => "primary_already_open",
        Some(StoreError::SecondaryCatchupFailed { .. }) => "secondary_catchup_failed",
        Some(StoreError::CheckpointUnavailable { .. }) => "checkpoint_unavailable",
        Some(StoreError::ReorgWindowExceeded { .. }) => "reorg_window_exceeded",
        Some(StoreError::EventCursorExpired { .. }) => "event_cursor_expired",
        Some(StoreError::EventCursorInvalid { .. }) => "event_cursor_invalid",
        Some(StoreError::ChainEventSequenceOverflow) => "chain_event_sequence_overflow",
        Some(StoreError::ChainEpochSequenceOverflow) => "chain_epoch_sequence_overflow",
        Some(StoreError::InvalidChainEpochArtifacts { .. }) => "invalid_chain_epoch_artifacts",
        Some(StoreError::ArtifactPayloadTooLarge { .. }) => "artifact_payload_too_large",
        Some(StoreError::InvalidChainStoreOptions { .. }) => "invalid_chain_store_options",
        Some(StoreError::ArtifactMissing { .. }) => "artifact_missing",
        Some(StoreError::ArtifactCorrupt { .. }) => "artifact_corrupt",
        Some(StoreError::Unsupported { .. }) => "unsupported",
        Some(_) => "store",
    }
}

fn writer_status_error_class(error: Option<&WriterStatusFetchError>) -> &'static str {
    match error {
        None => "none",
        Some(WriterStatusFetchError::InvalidEndpoint { .. }) => "invalid_endpoint",
        Some(WriterStatusFetchError::Connect { .. }) => "connect",
        Some(WriterStatusFetchError::ClientMissing) => "client_missing",
        Some(WriterStatusFetchError::Rpc { .. }) => "rpc",
        Some(WriterStatusFetchError::NetworkMismatch { .. }) => "network_mismatch",
    }
}

impl WriterStatusFetchError {
    fn endpoint(&self) -> &str {
        match self {
            Self::InvalidEndpoint { endpoint, .. }
            | Self::Connect { endpoint, .. }
            | Self::Rpc { endpoint, .. }
            | Self::NetworkMismatch { endpoint, .. } => endpoint,
            Self::ClientMissing => "unknown",
        }
    }

    const fn method(&self) -> &'static str {
        match self {
            Self::Rpc { method, .. } | Self::NetworkMismatch { method, .. } => method,
            Self::InvalidEndpoint { .. } | Self::Connect { .. } | Self::ClientMissing => {
                WRITER_STATUS_METHOD
            }
        }
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
