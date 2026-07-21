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
//! the consumer must reconnect. `mempool_change` only reports transitions
//! from the moment it opens, so every `events()` call also resnapshots the
//! upstream mempool via `getrawmempool` and emits a synthetic `Added` for
//! each already-present transaction; otherwise a transaction that entered
//! the mempool before the (re)connect would never be observed. The
//! resnapshot runs concurrently with, not before, draining the wire
//! stream. Wire observations are buffered until the resnapshot has been
//! forwarded, then the source emits
//! [`MempoolSourceEvent::InitialSnapshotComplete`]. The marker is a control
//! event, not a mempool lifecycle transition: it is the consumer's proof that
//! an initially empty index is complete enough to expose. Duplicate `Added`
//! events the two passes both produce for the same txid are a safe no-op
//! (`MempoolIndex::apply_added`).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::TryStreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::transport::Channel;
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

/// Bounds concurrent `getrawtransaction` round trips during a resnapshot.
///
/// Applies while resnapshotting the mempool on `events()` (re)connect.
/// Mirrors the polling backend's concurrency budget
/// (`json_rpc_mempool::MEMPOOL_POLL_HYDRATION_CONCURRENCY`).
const MEMPOOL_RESNAPSHOT_HYDRATION_CONCURRENCY: usize = 16;

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
        let channel = crate::transport::connect_zebra_indexer_channel(
            &self.target,
            crate::transport::ZebraIndexerChannelOptions {
                connect_timeout: self.options.connect_timeout,
                request_timeout: self.options.request_timeout,
            },
        )
        .await
        .map_err(|error| SourceError::MempoolStreamUnavailable {
            reason: error.to_string(),
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
        let (wire_sender, mut wire_receiver) = mpsc::channel(self.options.event_channel_capacity);
        let hydration_json_rpc = self.hydration_json_rpc.clone();

        tokio::spawn(async move {
            loop {
                match wire_stream.message().await {
                    Ok(Some(wire_message)) => {
                        if matches!(
                            forward_wire_message(&hydration_json_rpc, wire_message, &wire_sender)
                                .await,
                            ForwardOutcome::ChannelClosed
                        ) {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(stream_status) => {
                        let _ = wire_sender
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

        let snapshot_hydration_json_rpc = self.hydration_json_rpc.clone();
        tokio::spawn(async move {
            if let Err(source_error) =
                resnapshot_current_mempool(snapshot_hydration_json_rpc, &event_sender).await
            {
                let _ = forward_error(source_error, &event_sender).await;
                return;
            }

            // `mempool_change` was already open before the snapshot began.
            // Flush its bounded prefix before publishing the completion
            // marker so observations that raced the snapshot are applied
            // while the consumer still keeps the index hidden.
            while let Ok(wire_event) = wire_receiver.try_recv() {
                if matches!(
                    forward_result(wire_event, &event_sender).await,
                    ForwardOutcome::ChannelClosed
                ) {
                    return;
                }
            }
            if matches!(
                forward_event(MempoolSourceEvent::InitialSnapshotComplete, &event_sender).await,
                ForwardOutcome::ChannelClosed
            ) {
                return;
            }

            while let Some(wire_event) = wire_receiver.recv().await {
                if matches!(
                    forward_result(wire_event, &event_sender).await,
                    ForwardOutcome::ChannelClosed
                ) {
                    return;
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(event_receiver)))
    }
}

/// Emits a synthetic `Added` event for every transaction already sitting in
/// the upstream mempool at (re)connect time.
///
/// `mempool_change` only reports transitions from the moment it opens, so
/// without this pass a subscriber that (re)connects mid-stream would never
/// observe transactions that arrived before that point. Runs concurrently
/// with the wire-stream drain rather than before it, so its
/// `getrawtransaction` round trips never delay live delta delivery.
async fn resnapshot_current_mempool(
    hydration_json_rpc: ZebraJsonRpcSource,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<(), SourceError> {
    let observed_at = UnixTimestampMillis::now();
    let transaction_ids = hydration_json_rpc
        .fetch_raw_mempool_transaction_ids()
        .await?;

    futures_util::stream::iter(transaction_ids.into_iter().map(Ok::<_, SourceError>))
        .try_for_each_concurrent(MEMPOOL_RESNAPSHOT_HYDRATION_CONCURRENCY, |transaction_id| {
            let hydration_json_rpc = &hydration_json_rpc;
            async move {
                let source_event =
                    build_added_event(hydration_json_rpc, transaction_id, None, observed_at)
                        .await?;
                if matches!(
                    forward_event(source_event, event_sender).await,
                    ForwardOutcome::ChannelClosed
                ) {
                    return Err(SourceError::MempoolStreamUnavailable {
                        reason: "mempool snapshot receiver closed".to_owned(),
                    });
                }
                Ok(())
            }
        })
        .await
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
    let auth_digest = match decode_auth_digest(&wire_message.auth_digest) {
        Ok(auth_digest) => auth_digest,
        Err(decode_error) => return forward_error(decode_error, event_sender).await,
    };

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

async fn forward_result(
    source_result: Result<MempoolSourceEvent, SourceError>,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> ForwardOutcome {
    match source_result {
        Ok(source_event) => forward_event(source_event, event_sender).await,
        Err(source_error) => forward_error(source_error, event_sender).await,
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

/// Decodes a `MempoolChangeMessage.tx_hash` into a [`TransactionId`].
///
/// Zebra's indexer fills the field with `bytes_in_display_order`, so the
/// wire bytes carry RPC byte order and must be reversed into internal
/// order; a verbatim read yields a byte-reversed txid that fails every
/// follow-up `getrawtransaction` hydration lookup.
fn decode_transaction_id(wire_bytes: &[u8]) -> Result<TransactionId, SourceError> {
    zinder_core::wire::decode_rpc_transaction_id_bytes(wire_bytes).map_err(|_| {
        SourceError::InvalidTransactionIdLength {
            byte_count: wire_bytes.len(),
        }
    })
}

/// Decodes a `MempoolChangeMessage.auth_digest` into an [`AuthDigest`].
///
/// Same `bytes_in_display_order` wire contract as the txid field. Empty
/// bytes represent an omitted digest for pre-v5 transactions. Any
/// non-empty malformed value is a protocol error rather than absent metadata.
fn decode_auth_digest(wire_bytes: &[u8]) -> Result<Option<AuthDigest>, SourceError> {
    if wire_bytes.is_empty() {
        return Ok(None);
    }
    zinder_core::wire::decode_rpc_auth_digest_bytes(wire_bytes)
        .map(Some)
        .map_err(|_| SourceError::SourceProtocolMismatch {
            reason: "Zebra indexer mempool auth_digest was not exactly 32 bytes",
        })
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;

    // Zebra's indexer fills `MempoolChangeMessage.tx_hash` and
    // `auth_digest` with `bytes_in_display_order` (RPC byte order); the
    // decoders must reverse into internal order or every hydration lookup
    // built from the txid targets a byte-reversed transaction.
    #[test]
    fn decode_transaction_id_reverses_display_order_wire_bytes() -> Result<(), SourceError> {
        let mut wire_bytes = [0u8; 32];
        for (index, slot) in wire_bytes.iter_mut().enumerate() {
            *slot = u8::try_from(index).unwrap_or_default();
        }
        let transaction_id = decode_transaction_id(&wire_bytes)?;
        let mut internal_bytes = wire_bytes;
        internal_bytes.reverse();
        assert_eq!(transaction_id.as_bytes(), internal_bytes);
        Ok(())
    }

    #[test]
    fn decode_transaction_id_matches_getrawmempool_display_hex_for_same_transaction()
    -> eyre::Result<()> {
        // The same txid arrives display-order-hex from `getrawmempool` and
        // display-order-bytes from the indexer stream; both must decode to
        // one internal-order id or stream hydration diverges from the
        // resnapshot path.
        let display_hex = "c3ca0ce69e0661792cbc65812eb351d0f5ba7238fdec2bb5dca3fc8ab7559436";
        let wire_bytes = hex::decode(display_hex)?;
        let from_stream = decode_transaction_id(&wire_bytes)?;
        let from_rpc = zinder_core::wire::decode_rpc_transaction_id_hex(display_hex)?;
        assert_eq!(from_stream, from_rpc);
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
        assert!(matches!(auth_digest, Ok(None)));
    }

    #[test]
    fn decode_auth_digest_reverses_display_order_wire_bytes() -> Result<(), &'static str> {
        let mut wire_bytes = [0u8; 32];
        for (index, slot) in wire_bytes.iter_mut().enumerate() {
            *slot = u8::try_from(index).unwrap_or_default();
        }
        let auth_digest = decode_auth_digest(&wire_bytes)
            .map_err(|_| "32 byte payload decodes")?
            .ok_or("32 byte payload is present")?;
        let mut internal_bytes = wire_bytes;
        internal_bytes.reverse();
        assert_eq!(auth_digest.as_bytes(), internal_bytes);
        Ok(())
    }

    #[test]
    fn decode_auth_digest_rejects_non_empty_invalid_length() {
        let wire_bytes = [0x55u8; 16];
        let auth_digest = decode_auth_digest(&wire_bytes);
        assert!(matches!(
            auth_digest,
            Err(SourceError::SourceProtocolMismatch { .. })
        ));
    }

    // Regression coverage for the resnapshot-on-connect gap: `mempool_change`
    // only reports transitions from the moment it opens, so without a
    // resnapshot pass a transaction already in the mempool at (re)connect
    // time was never observed (see the module doc's `# Reconnect contract`).
    #[tokio::test]
    async fn resnapshot_current_mempool_emits_added_for_preexisting_transaction() -> eyre::Result<()>
    {
        // All-same-byte txid sidesteps needing to know `getrawmempool`'s
        // display-order byte reversal: reversing `[0x11; 32]` is a no-op.
        let transaction_id = TransactionId::from_bytes([0x11; 32]);
        let txid_hex = "11".repeat(32);
        let server = zinder_testkit::JsonRpcTestServer::start([
            zinder_testkit::method("getrawmempool").reply(zinder_testkit::RpcReply::result(
                serde_json::json!([txid_hex]),
            )),
            zinder_testkit::method("getrawtransaction").reply(zinder_testkit::RpcReply::result(
                serde_json::json!("deadbeef"),
            )),
        ])?;
        let hydration_json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;

        let (event_sender, mut event_receiver) = mpsc::channel(8);
        resnapshot_current_mempool(hydration_json_rpc, &event_sender).await?;
        drop(event_sender);

        let event = event_receiver
            .recv()
            .await
            .ok_or_else(|| eyre::eyre!("expected a resnapshot event"))??;
        let MempoolSourceEvent::Added(entry) = event else {
            return Err(eyre::eyre!("expected an Added event, got {event:?}"));
        };
        assert_eq!(entry.transaction_id, transaction_id);
        assert_eq!(
            entry.raw_transaction_bytes.as_slice(),
            [0xde, 0xad, 0xbe, 0xef]
        );
        assert!(
            event_receiver.recv().await.is_none(),
            "resnapshot must not emit more than one event per mempool entry"
        );
        Ok(())
    }
}
