//! Streaming chain-tip notifications from Zebra's gRPC indexer.
//!
//! Subscribes to Zebra's `Indexer.ChainTipChange` stream (available when
//! Zebra is built with `--features indexer`) and forwards each notification
//! as a typed [`ChainTipNotification`]. The subscriber is consumed by the
//! ingest tip-follow loop as a push-based wake-up signal that replaces the
//! `poll_interval_ms` cadence with near-zero-latency reactions to new
//! tip blocks.
//!
//! The adapter does *not* hydrate the block bytes from the stream. Block
//! fetching stays on the existing JSON-RPC path because the per-block
//! workflow still needs `z_gettreestate`, which is JSON-RPC-only. The win
//! is in the wake-up signal: every 2 s of idle sleep in the polling path
//! is replaced with an immediate notification on tip change.
//!
//! # Reconnect contract
//!
//! Zebra terminates the stream with `UNAVAILABLE` when the broadcast
//! channel lags or the node restarts. The consumer re-subscribes and keeps
//! JSON-RPC polling active while the stream is down. The adapter does not
//! buffer notifications across reconnects; the tip-follow loop's
//! poll-interval safety net ensures any missed tips are caught up via JSON-RPC.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::transport::{Channel, Endpoint};
use zinder_core::{BlockHeight, BlockId};
use zinder_proto::external::zebra_indexer_rpc::{
    BlockHashAndHeight, Empty, indexer_client::IndexerClient,
};

use crate::{SourceError, ZebraIndexerSourceTarget, decode_display_block_hash};

/// A chain-tip change observed via Zebra's gRPC indexer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainTipNotification {
    /// Tip identity emitted by Zebra at the time of notification.
    pub tip_id: BlockId,
}

/// Stream of chain-tip notifications.
///
/// The stream may yield transient errors (network blips, broadcast lag).
/// Consumers should treat each error as a signal to fall back to the
/// polling path on the next iteration; the tip-follow loop's existing
/// poll-interval safety net covers that case.
pub type ChainTipNotificationStream = BoxStream<'static, Result<ChainTipNotification, SourceError>>;

/// Source that opens Zebra chain-tip notification streams.
///
/// Consumers use this as a wake-up source, not as canonical chain data. A
/// subscription can end at any time; callers keep JSON-RPC polling as the
/// authoritative catch-up path and re-subscribe when the stream ends.
#[async_trait]
pub trait ChainTipNotificationSource: Send + Sync + 'static {
    /// Opens a fresh chain-tip notification stream.
    async fn subscribe(&self) -> Result<ChainTipNotificationStream, SourceError>;
}

/// Runtime options for [`ZebraIndexerChainTipSource`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZebraIndexerChainTipSourceOptions {
    /// Connect timeout for the indexer endpoint.
    pub connect_timeout: Duration,
    /// Request timeout applied to the streaming RPC call.
    pub request_timeout: Duration,
    /// Channel buffer size for the emitted notification stream.
    pub notification_channel_capacity: usize,
}

impl Default for ZebraIndexerChainTipSourceOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_hours(1),
            notification_channel_capacity: 64,
        }
    }
}

/// Streaming chain-tip source backed by Zebra's gRPC indexer port.
#[derive(Clone, Debug)]
pub struct ZebraIndexerChainTipSource {
    target: ZebraIndexerSourceTarget,
    options: ZebraIndexerChainTipSourceOptions,
}

impl ZebraIndexerChainTipSource {
    /// Creates a chain-tip source with default options.
    #[must_use]
    pub fn new(target: ZebraIndexerSourceTarget) -> Self {
        Self::with_options(target, ZebraIndexerChainTipSourceOptions::default())
    }

    /// Creates a chain-tip source with explicit options.
    #[must_use]
    pub const fn with_options(
        target: ZebraIndexerSourceTarget,
        options: ZebraIndexerChainTipSourceOptions,
    ) -> Self {
        Self { target, options }
    }

    /// Subscribes to Zebra's `ChainTipChange` stream.
    ///
    /// Returns a future-bounded stream of typed [`ChainTipNotification`]
    /// values. On any transport-level failure the stream ends and the
    /// caller is expected to re-subscribe.
    pub async fn subscribe(&self) -> Result<ChainTipNotificationStream, SourceError> {
        let mut indexer_client = self.connect().await?;
        let response = indexer_client
            .chain_tip_change(Request::new(Empty {}))
            .await
            .map_err(|status| SourceError::ChainTipStreamUnavailable {
                reason: format!("indexer chain_tip_change call failed: {status}"),
            })?;
        let mut wire_stream = response.into_inner();

        let (sender, receiver) = mpsc::channel(self.options.notification_channel_capacity);

        tokio::spawn(async move {
            loop {
                match wire_stream.message().await {
                    Ok(Some(wire_message)) => {
                        let outcome = decode_chain_tip_message(&wire_message);
                        if sender.send(outcome).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(status) => {
                        let _ = sender
                            .send(Err(SourceError::ChainTipStreamUnavailable {
                                reason: format!("indexer chain_tip_change stream ended: {status}"),
                            }))
                            .await;
                        return;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(receiver)))
    }

    async fn connect(&self) -> Result<IndexerClient<Channel>, SourceError> {
        let endpoint =
            Endpoint::from_shared(self.target.endpoint_url.clone()).map_err(|error| {
                SourceError::ChainTipStreamUnavailable {
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
                .map_err(|error| SourceError::ChainTipStreamUnavailable {
                    reason: format!("indexer endpoint connect failed: {error}"),
                })?;
        Ok(IndexerClient::new(channel))
    }
}

#[async_trait]
impl ChainTipNotificationSource for ZebraIndexerChainTipSource {
    async fn subscribe(&self) -> Result<ChainTipNotificationStream, SourceError> {
        Self::subscribe(self).await
    }
}

fn decode_chain_tip_message(
    wire_message: &BlockHashAndHeight,
) -> Result<ChainTipNotification, SourceError> {
    if wire_message.hash.len() != 32 {
        return Err(SourceError::SourceProtocolMismatch {
            reason: "Zebra chain_tip_change wire reported a block hash that was not 32 bytes",
        });
    }
    // `BlockHashAndHeight.hash` arrives in display order per the proto
    // contract, which matches what JSON-RPC consumers feed into
    // `decode_display_block_hash` after hex-encoding. Reuse the same
    // canonical decode path so internal byte ordering stays single-sourced.
    let display_hash_hex = hex::encode(&wire_message.hash);
    let hash = decode_display_block_hash(&display_hash_hex)?;
    Ok(ChainTipNotification {
        tip_id: BlockId {
            height: BlockHeight::new(wire_message.height),
            hash,
        },
    })
}
