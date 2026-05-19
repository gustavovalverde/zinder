//! Production wirings of [`MempoolSurface`] and [`TipChangeWatcher`] over the
//! private `IngestControl` gRPC.
//!
//! The compatibility binary is colocated with `zinder-query` and the writer's
//! `IngestControl` endpoint. The wirings here connect on demand so a transient
//! writer outage does not require restarting the compat process.
//!
//! ## Connection lifecycle and fan-out
//!
//! Each [`IngestControlMempoolSurface`] instance dials a single HTTP/2 channel
//! to the writer the first time a method is invoked, caches it in an
//! [`Arc<OnceCell<_>>`], and reuses it for every subsequent
//! [`IngestControlMempoolSurface::mempool_snapshot_page`] /
//! [`IngestControlMempoolSurface::mempool_events`] call. Tonic's `Channel`
//! handles transparent HTTP/2 reconnect, so a transient writer outage does
//! not require restarting the compat process.
//!
//! With N concurrent lightwalletd `GetMempoolStream` clients there are N
//! concurrent `IngestControl` `MempoolEvents` subscriptions multiplexed over
//! the cached channel. Operators capping the public lightwalletd surface
//! (Caddy, nginx) bound N at the proxy edge; Zinder does not enforce a
//! process-wide cap.
//! [`spawn_ingest_control_tip_change_publisher`] runs a separate session-per-
//! reconnect loop for `ChainEvents`, with a 500 ms backoff between attempts
//! so a writer restart does not drive a tight reconnect loop.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OnceCell, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use zinder_proto::v1::{
    ingest::ingest_control_client::IngestControlClient,
    wallet::{self, MempoolEventStreamFamily},
};
use zinder_runtime::{AuthenticatedChannel, BearerToken, connect_zinder_grpc};
use zinder_store::{
    StreamCursorTokenV1, mempool_entry_from_message, mempool_event_envelope_from_message,
};

use super::surface::{
    MempoolEventEnvelopeStream, MempoolSnapshotPage, MempoolSurface, MempoolSurfaceError,
    TipChangeWatcher, TipChangeWatcherError,
};

type AuthenticatedIngestControlClient = IngestControlClient<AuthenticatedChannel>;

/// Reconnect delay after a `chain_events` session ends without an explicit shutdown.
///
/// Short enough to recover quickly from a writer restart, long enough that a
/// tight reconnect loop does not hammer the writer when it actively rejects
/// connections.
const TIP_PUBLISHER_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Mempool read surface backed by an `IngestControl` gRPC endpoint.
#[derive(Clone, Debug)]
pub struct IngestControlMempoolSurface {
    endpoint: String,
    bearer_token: Option<BearerToken>,
    channel: Arc<OnceCell<AuthenticatedChannel>>,
}

impl IngestControlMempoolSurface {
    /// Creates a mempool surface that dials the writer's `IngestControl`
    /// endpoint on first use and caches the channel for the surface's
    /// lifetime.
    #[must_use]
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            bearer_token: None,
            channel: Arc::new(OnceCell::new()),
        }
    }

    /// Wires a shared-secret bearer token onto every outbound request. The
    /// token is attached as an `authorization: Bearer <token>` metadata
    /// header by the underlying gRPC interceptor.
    #[must_use]
    pub fn with_bearer_token(mut self, bearer_token: BearerToken) -> Self {
        self.bearer_token = Some(bearer_token);
        self
    }

    /// Returns an `IngestControl` client backed by the surface's cached
    /// HTTP/2 channel.
    ///
    /// The first call dials the writer; subsequent calls reuse the cached
    /// [`AuthenticatedChannel`] (cheap clone, transparent HTTP/2 reconnect).
    async fn ingest_control_client(
        &self,
    ) -> Result<AuthenticatedIngestControlClient, MempoolSurfaceError> {
        let endpoint = self.endpoint.clone();
        let bearer_token = self.bearer_token.clone();
        let channel = self
            .channel
            .get_or_try_init(|| async move {
                connect_zinder_grpc(&endpoint, bearer_token.as_ref())
                    .await
                    .map_err(|error| MempoolSurfaceError::Unavailable {
                        reason: error.to_string(),
                    })
            })
            .await?;
        Ok(IngestControlClient::new(channel.clone()))
    }
}

#[async_trait]
impl MempoolSurface for IngestControlMempoolSurface {
    async fn mempool_snapshot_page(
        &self,
        max_entries: u32,
        from_cursor: Option<Vec<u8>>,
    ) -> Result<MempoolSnapshotPage, MempoolSurfaceError> {
        let mut client = self.ingest_control_client().await?;
        let response = client
            .mempool_snapshot(wallet::MempoolSnapshotRequest {
                max_entries,
                from_cursor: from_cursor.unwrap_or_default(),
            })
            .await
            .map_err(|status| MempoolSurfaceError::Unavailable {
                reason: format!("ingest-control mempool_snapshot failed: {status}"),
            })?
            .into_inner();
        let entries = response
            .entries
            .into_iter()
            .map(|entry| {
                mempool_entry_from_message(entry).map_err(|error| {
                    MempoolSurfaceError::Unavailable {
                        reason: error.to_string(),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if response.next_cursor.is_empty() {
            None
        } else {
            Some(response.next_cursor)
        };
        Ok(MempoolSnapshotPage {
            snapshot_sequence: response.snapshot_sequence,
            entries,
            next_cursor,
        })
    }

    async fn mempool_events(
        &self,
        from_cursor: Option<StreamCursorTokenV1>,
    ) -> Result<MempoolEventEnvelopeStream, MempoolSurfaceError> {
        let mut client = self.ingest_control_client().await?;
        let cursor_bytes = from_cursor
            .as_ref()
            .map_or_else(Vec::new, |cursor| cursor.as_bytes().to_vec());
        let response = client
            .mempool_events(wallet::MempoolEventsRequest {
                from_cursor: cursor_bytes,
                family: MempoolEventStreamFamily::Mempool as i32,
            })
            .await
            .map_err(|status| MempoolSurfaceError::Unavailable {
                reason: format!("ingest-control mempool_events failed: {status}"),
            })?;

        let (output_sender, output_receiver) = mpsc::channel(16);
        let mut response_stream = response.into_inner();
        tokio::spawn(async move {
            loop {
                match tokio_stream::StreamExt::next(&mut response_stream).await {
                    Some(Ok(message)) => {
                        match mempool_event_envelope_from_message(message).map_err(|error| {
                            MempoolSurfaceError::Unavailable {
                                reason: error.to_string(),
                            }
                        }) {
                            Ok(envelope) => {
                                if output_sender.send(Ok(envelope)).await.is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                let _ = output_sender.send(Err(error)).await;
                                return;
                            }
                        }
                    }
                    Some(Err(status)) => {
                        let _ = output_sender
                            .send(Err(MempoolSurfaceError::Unavailable {
                                reason: format!("ingest-control mempool_events errored: {status}"),
                            }))
                            .await;
                        return;
                    }
                    None => return,
                }
            }
        });
        Ok(Box::pin(ReceiverStream::new(output_receiver)))
    }
}

/// Resolves [`TipChangeWatcher::await_tip_change`] from a
/// [`tokio::sync::watch::Receiver`] published by a chain-events consumer.
#[derive(Clone, Debug)]
pub struct WatchTipChangeWatcher {
    receiver: watch::Receiver<u64>,
}

impl WatchTipChangeWatcher {
    /// Creates a watcher over the given watch receiver.
    #[must_use]
    pub const fn new(receiver: watch::Receiver<u64>) -> Self {
        Self { receiver }
    }
}

#[async_trait]
impl TipChangeWatcher for WatchTipChangeWatcher {
    async fn await_tip_change(&self) -> Result<(), TipChangeWatcherError> {
        let mut receiver = self.receiver.clone();
        // Marks any unseen value as seen so the next `changed()` waits for
        // the strictly-after-now publish.
        receiver.mark_unchanged();
        receiver
            .changed()
            .await
            .map_err(|_| TipChangeWatcherError::SignalClosed)
    }
}

/// Spawns a task that consumes `IngestControl.chain_events` and publishes
/// committed event sequences to a `watch::Sender<u64>`.
///
/// Returns a watcher view over the same channel. Drop the
/// [`tokio::task::JoinHandle`] to detach, or await it for symmetric
/// shutdown.
#[must_use = "drop the handle to detach the publisher or await it for symmetric shutdown"]
pub fn spawn_ingest_control_tip_change_publisher(
    endpoint: String,
    bearer_token: Option<BearerToken>,
    cancel: CancellationToken,
) -> (Arc<dyn TipChangeWatcher>, tokio::task::JoinHandle<()>) {
    let (tip_sender, tip_receiver) = watch::channel(0_u64);
    let watcher = Arc::new(WatchTipChangeWatcher::new(tip_receiver));
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = run_ingest_control_tip_change_session(endpoint.clone(), bearer_token.as_ref(), &tip_sender) => {
                    tokio::time::sleep(TIP_PUBLISHER_RECONNECT_BACKOFF).await;
                }
            }
        }
    });
    (watcher, handle)
}

async fn run_ingest_control_tip_change_session(
    endpoint: String,
    bearer_token: Option<&BearerToken>,
    tip_sender: &watch::Sender<u64>,
) {
    let mut client = match connect_authenticated_ingest_control(&endpoint, bearer_token).await {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(
                target: "zinder::compat_lightwalletd",
                event = "tip_change_publisher_connect_failed",
                error = %error,
                "tip-change publisher could not connect to ingest-control; will retry"
            );
            return;
        }
    };
    let response_outcome = client
        .chain_events(Request::new(wallet::ChainEventsRequest {
            from_cursor: Vec::new(),
            family: wallet::ChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }))
        .await;
    let response = match response_outcome {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(
                target: "zinder::compat_lightwalletd",
                event = "tip_change_publisher_subscribe_failed",
                error = %error,
                "tip-change publisher chain_events subscribe failed; will retry"
            );
            return;
        }
    };
    let mut response_stream = response.into_inner();
    while let Some(message_outcome) = tokio_stream::StreamExt::next(&mut response_stream).await {
        match message_outcome {
            Ok(envelope) => {
                let _ = tip_sender.send(envelope.event_sequence);
            }
            Err(error) => {
                tracing::debug!(
                    target: "zinder::compat_lightwalletd",
                    event = "tip_change_publisher_stream_error",
                    error = %error,
                    "tip-change publisher chain_events stream errored; will reconnect"
                );
                return;
            }
        }
    }
}

async fn connect_authenticated_ingest_control(
    endpoint: &str,
    bearer_token: Option<&BearerToken>,
) -> Result<AuthenticatedIngestControlClient, MempoolSurfaceError> {
    let channel = connect_zinder_grpc(endpoint, bearer_token)
        .await
        .map_err(|error| MempoolSurfaceError::Unavailable {
            reason: error.to_string(),
        })?;
    Ok(IngestControlClient::new(channel))
}
