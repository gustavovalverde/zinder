//! Streaming Zebra indexer mempool source.
//!
//! Consumes Zebra's gRPC `Indexer.MempoolChange` stream (available when
//! Zebra is built with `--features indexer`) and hydrates `ADDED`
//! notifications through the `ZebraJsonRpcSource::fetch_raw_transaction_bytes`
//! call. Streaming is the preferred backend because it surfaces typed
//! `Invalidated` and `Mined` change types that the polling backend can
//! only synthesize from secondary lookups.
//!
//! # Reconnect contract
//!
//! Zebra's broadcast channel terminates the stream with `UNAVAILABLE` on
//! `RecvError::Lagged`; the source treats this as a transient error and
//! the consumer must reconnect and resnapshot the upstream mempool. The
//! adapter does not buffer events across reconnects.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::transport::{Channel, Endpoint};
use zinder_core::{AuthDigest, MempoolEvictionReason, TransactionId, UnixTimestampMillis};
use zinder_proto::external::zebra_indexer_rpc::{
    Empty, MempoolChangeMessage, indexer_client::IndexerClient, mempool_change_message::ChangeType,
};

use crate::{
    MempoolHydrationFailureReason, MempoolSource, MempoolSourceCapabilities, MempoolSourceEntry,
    MempoolSourceEvent, MempoolSourceEventStream, SourceError, UpstreamTransactionLookup,
    ZebraJsonRpcSource,
};

const ZEBRA_INDEXER_STREAMING_BACKEND_LABEL: &str = "zebra_indexer_streaming";

fn increment_streaming_hydration_failure(reason: MempoolHydrationFailureReason) {
    metrics::counter!(
        "zinder_mempool_hydration_failures_total",
        "backend" => ZEBRA_INDEXER_STREAMING_BACKEND_LABEL,
        "reason" => reason.as_label()
    )
    .increment(1);
}

/// Endpoint for a Zebra indexer-feature gRPC port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZebraIndexerSourceTarget {
    /// HTTP/2 endpoint URL of the Zebra indexer port (e.g.
    /// `http://127.0.0.1:8154`).
    pub endpoint_url: String,
}

impl ZebraIndexerSourceTarget {
    /// Creates an indexer source target from an endpoint URL.
    pub fn new(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
        }
    }
}

/// Runtime options for [`ZebraIndexerMempoolSource`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZebraIndexerMempoolSourceOptions {
    /// Connect timeout for the indexer endpoint.
    pub connect_timeout: Duration,
    /// Request timeout for the streaming RPC call.
    pub request_timeout: Duration,
    /// Channel buffer size for the emitted source event stream.
    pub event_channel_capacity: usize,
}

impl Default for ZebraIndexerMempoolSourceOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_mins(1),
            event_channel_capacity: 256,
        }
    }
}

/// Streaming mempool source backed by Zebra's gRPC indexer port.
#[derive(Clone)]
pub struct ZebraIndexerMempoolSource {
    target: ZebraIndexerSourceTarget,
    hydration_json_rpc: ZebraJsonRpcSource,
    options: ZebraIndexerMempoolSourceOptions,
}

impl ZebraIndexerMempoolSource {
    /// Creates a streaming mempool source.
    ///
    /// `target` points at the Zebra indexer gRPC endpoint;
    /// `hydration_json_rpc` is the JSON-RPC source used to fetch raw
    /// transaction bytes for `ADDED` change notifications. Both must
    /// point at the same Zebra deployment to avoid mempool-state
    /// inconsistency.
    #[must_use]
    pub fn new(target: ZebraIndexerSourceTarget, hydration_json_rpc: ZebraJsonRpcSource) -> Self {
        Self::with_options(
            target,
            hydration_json_rpc,
            ZebraIndexerMempoolSourceOptions::default(),
        )
    }

    /// Creates a streaming mempool source with explicit options.
    #[must_use]
    pub const fn with_options(
        target: ZebraIndexerSourceTarget,
        hydration_json_rpc: ZebraJsonRpcSource,
        options: ZebraIndexerMempoolSourceOptions,
    ) -> Self {
        Self {
            target,
            hydration_json_rpc,
            options,
        }
    }

    async fn connect(&self) -> Result<IndexerClient<Channel>, SourceError> {
        let endpoint =
            Endpoint::from_shared(self.target.endpoint_url.clone()).map_err(|error| {
                SourceError::MempoolStreamUnavailable {
                    reason: format!("invalid indexer endpoint: {error}"),
                }
            })?;
        let endpoint = endpoint
            .connect_timeout(self.options.connect_timeout)
            .timeout(self.options.request_timeout);
        let channel =
            endpoint
                .connect()
                .await
                .map_err(|error| SourceError::MempoolStreamUnavailable {
                    reason: format!("indexer endpoint connect failed: {error}"),
                })?;
        Ok(IndexerClient::new(channel))
    }
}

#[async_trait]
impl MempoolSource for ZebraIndexerMempoolSource {
    fn capabilities(&self) -> MempoolSourceCapabilities {
        MempoolSourceCapabilities::streaming()
    }

    async fn events(&self) -> Result<MempoolSourceEventStream, SourceError> {
        let mut indexer_client = self.connect().await?;
        let response = indexer_client
            .mempool_change(Request::new(Empty {}))
            .await
            .map_err(|status| SourceError::MempoolStreamUnavailable {
                reason: format!("indexer mempool_change call failed: {status}"),
            })?;
        let mut wire_stream = response.into_inner();

        let (event_sender, event_receiver) = mpsc::channel(self.options.event_channel_capacity);
        let hydration_json_rpc = self.hydration_json_rpc.clone();

        tokio::spawn(async move {
            loop {
                match wire_stream.message().await {
                    Ok(Some(wire_message)) => {
                        if matches!(
                            forward_wire_message(&hydration_json_rpc, wire_message, &event_sender)
                                .await,
                            ForwardOutcome::ChannelClosed
                        ) {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(stream_status) => {
                        let _ = event_sender
                            .send(Err(SourceError::MempoolStreamUnavailable {
                                reason: format!(
                                    "indexer mempool_change stream ended: {stream_status}"
                                ),
                            }))
                            .await;
                        return;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(event_receiver)))
    }
}

/// Whether the consumer is still listening to the source-event channel.
enum ForwardOutcome {
    Continue,
    ChannelClosed,
}

async fn forward_wire_message(
    hydration_json_rpc: &ZebraJsonRpcSource,
    wire_message: MempoolChangeMessage,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> ForwardOutcome {
    let observed_at = UnixTimestampMillis::now();
    let transaction_id = match decode_transaction_id(&wire_message.tx_hash) {
        Ok(transaction_id) => transaction_id,
        Err(decode_error) => return forward_error(decode_error, event_sender).await,
    };
    let auth_digest = decode_auth_digest(&wire_message.auth_digest);

    let outcome = match ChangeType::try_from(wire_message.change_type) {
        Ok(ChangeType::Added) => {
            build_added_event(hydration_json_rpc, transaction_id, auth_digest, observed_at).await
        }
        Ok(ChangeType::Invalidated) => Ok(MempoolSourceEvent::Invalidated {
            transaction_id,
            reason: MempoolEvictionReason::Unknown,
        }),
        Ok(ChangeType::Mined) => build_mined_event(hydration_json_rpc, transaction_id).await,
        Err(_) => Err(SourceError::SourceProtocolMismatch {
            reason: "Zebra indexer mempool_change wire reported unrecognized ChangeType",
        }),
    };

    match outcome {
        Ok(source_event) => forward_event(source_event, event_sender).await,
        Err(source_error) => forward_error(source_error, event_sender).await,
    }
}

async fn forward_event(
    source_event: MempoolSourceEvent,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> ForwardOutcome {
    if event_sender.send(Ok(source_event)).await.is_err() {
        ForwardOutcome::ChannelClosed
    } else {
        ForwardOutcome::Continue
    }
}

async fn forward_error(
    source_error: SourceError,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> ForwardOutcome {
    if event_sender.send(Err(source_error)).await.is_err() {
        ForwardOutcome::ChannelClosed
    } else {
        ForwardOutcome::Continue
    }
}

async fn build_mined_event(
    hydration_json_rpc: &ZebraJsonRpcSource,
    transaction_id: TransactionId,
) -> Result<MempoolSourceEvent, SourceError> {
    match hydration_json_rpc
        .fetch_upstream_transaction_lookup(transaction_id)
        .await?
    {
        UpstreamTransactionLookup::Mined {
            mined_height,
            block_hash,
        } => Ok(MempoolSourceEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        }),
        UpstreamTransactionLookup::InMempool | UpstreamTransactionLookup::NotFound => {
            // Race window: streaming reported MINED but the follow-up
            // lookup observed a different state (e.g. fork or restoration
            // to mempool). Surface as Invalidated{Unknown} so downstream
            // does not over-report Mined heights.
            Ok(MempoolSourceEvent::Invalidated {
                transaction_id,
                reason: MempoolEvictionReason::Unknown,
            })
        }
    }
}

async fn build_added_event(
    hydration_json_rpc: &ZebraJsonRpcSource,
    transaction_id: TransactionId,
    auth_digest: Option<AuthDigest>,
    observed_at: UnixTimestampMillis,
) -> Result<MempoolSourceEvent, SourceError> {
    match hydration_json_rpc
        .fetch_raw_transaction_bytes(transaction_id)
        .await
    {
        Ok(Some(raw_transaction_bytes)) => Ok(MempoolSourceEvent::Added(MempoolSourceEntry {
            transaction_id,
            auth_digest,
            raw_transaction_bytes,
            observed_at_unix_millis: observed_at,
        })),
        Ok(None) => {
            increment_streaming_hydration_failure(MempoolHydrationFailureReason::NotFound);
            Err(SourceError::MempoolHydrationFailed {
                transaction_id,
                reason: "transaction disappeared from upstream node before hydration completed"
                    .to_owned(),
            })
        }
        Err(hydration_error) => {
            increment_streaming_hydration_failure(MempoolHydrationFailureReason::RpcError);
            Err(hydration_error)
        }
    }
}

fn decode_transaction_id(wire_bytes: &[u8]) -> Result<TransactionId, SourceError> {
    let byte_count = wire_bytes.len();
    let id_bytes = <[u8; 32]>::try_from(wire_bytes)
        .map_err(|_| SourceError::InvalidTransactionIdLength { byte_count })?;
    Ok(TransactionId::from_bytes(id_bytes))
}

fn decode_auth_digest(wire_bytes: &[u8]) -> Option<AuthDigest> {
    if wire_bytes.is_empty() {
        return None;
    }
    <[u8; 32]>::try_from(wire_bytes)
        .ok()
        .map(AuthDigest::from_bytes)
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;

    #[test]
    fn decode_transaction_id_accepts_32_byte_payload() -> Result<(), SourceError> {
        let wire_bytes = [0x42u8; 32];
        let transaction_id = decode_transaction_id(&wire_bytes)?;
        assert_eq!(transaction_id.as_bytes(), wire_bytes);
        Ok(())
    }

    #[test]
    fn decode_transaction_id_rejects_short_payload() {
        let wire_bytes = [0x42u8; 16];
        let outcome = decode_transaction_id(&wire_bytes);
        assert!(matches!(
            outcome,
            Err(SourceError::InvalidTransactionIdLength { byte_count: 16 })
        ));
    }

    #[test]
    fn decode_auth_digest_returns_none_for_empty_bytes() {
        let auth_digest = decode_auth_digest(&[]);
        assert!(auth_digest.is_none());
    }

    #[test]
    fn decode_auth_digest_returns_some_for_32_byte_payload() -> Result<(), &'static str> {
        let wire_bytes = [0x55u8; 32];
        let auth_digest = decode_auth_digest(&wire_bytes).ok_or("32 byte payload decodes")?;
        assert_eq!(auth_digest.as_bytes(), wire_bytes);
        Ok(())
    }

    #[test]
    fn decode_auth_digest_returns_none_for_invalid_length() {
        let wire_bytes = [0x55u8; 16];
        let auth_digest = decode_auth_digest(&wire_bytes);
        assert!(auth_digest.is_none());
    }
}
