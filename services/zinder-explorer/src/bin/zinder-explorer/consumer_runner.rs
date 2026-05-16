//! Background task that drives the `BlockSummary` derive consumer.
//!
//! The runner opens a `WalletQueryClient`, reads any persisted cursor for
//! [`zinder_explorer::BLOCK_SUMMARY_CONSUMER_NAME`], subscribes to
//! `WalletQuery.ChainEvents` from that cursor, and feeds the resulting
//! stream into [`zinder_explorer::run_chain_events_subscriber`]. The wallet
//! plane replays historic events strictly after the supplied cursor, so the
//! same loop covers cold-start backfill and live updates.
//!
//! On a transient stream failure the runner sleeps for
//! [`RECONNECT_BACKOFF`] and reconnects with the latest persisted cursor;
//! cancellation through the binary's shared token unblocks both the sleep
//! and the subscriber loop.

use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use zinder_explorer::{
    BLOCK_SUMMARY_CONSUMER_NAME, BlockSummaryConsumer, DeriveError, DeriveStore,
    run_chain_events_subscriber,
};
use zinder_proto::v1::wallet::{
    ChainEventStreamFamily as WireChainEventStreamFamily, ChainEventsRequest,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::{AuthenticatedChannel, connect_authenticated_channel};

/// Backoff used between reconnect attempts after a transient stream failure.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

pub(crate) async fn run_block_summary_consumer(
    store: DeriveStore,
    wallet_endpoint: String,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match dispatch_once(&store, &wallet_endpoint, &cancel).await {
            Ok(()) => break,
            Err(error) => {
                tracing::warn!(
                    target: "zinder::explorer",
                    event = "block_summary_consumer_reconnect",
                    error = %error,
                    "BlockSummary consumer stream ended; reconnecting after backoff"
                );
                tokio::select! {
                    () = sleep(RECONNECT_BACKOFF) => {}
                    () = cancel.cancelled() => break,
                }
            }
        }
    }
    tracing::info!(
        target: "zinder::explorer",
        event = "block_summary_consumer_stopped",
        "BlockSummary consumer stopped"
    );
}

async fn dispatch_once(
    store: &DeriveStore,
    wallet_endpoint: &str,
    cancel: &CancellationToken,
) -> Result<(), DispatchError> {
    let channel = connect_authenticated_channel(wallet_endpoint, None)
        .await
        .map_err(|error| DispatchError::Connect(error.to_string()))?;
    let mut wallet_client = WalletQueryClient::new(channel);
    let cursor_bytes = store
        .get_cursor(BLOCK_SUMMARY_CONSUMER_NAME)
        .map_err(|error| DispatchError::Other(error.to_string()))?
        .unwrap_or_default();
    let stream = wallet_client
        .chain_events(Request::new(ChainEventsRequest {
            from_cursor: cursor_bytes,
            family: WireChainEventStreamFamily::Tip as i32,
            address_filter: Vec::new(),
        }))
        .await
        .map_err(|status| DispatchError::Connect(status.to_string()))?
        .into_inner();

    let mut consumer =
        BlockSummaryConsumer::new(wallet_client_for_consumer(wallet_endpoint).await?);
    tokio::select! {
        outcome = run_chain_events_subscriber(&mut consumer, store, stream) => {
            outcome.map(|_| ()).map_err(|error: DeriveError| DispatchError::Subscriber(error.to_string()))
        }
        () = cancel.cancelled() => Ok(()),
    }
}

async fn wallet_client_for_consumer(
    wallet_endpoint: &str,
) -> Result<WalletQueryClient<AuthenticatedChannel>, DispatchError> {
    let channel = connect_authenticated_channel(wallet_endpoint, None)
        .await
        .map_err(|error| DispatchError::Connect(error.to_string()))?;
    Ok(WalletQueryClient::new(channel))
}

#[derive(Debug, thiserror::Error)]
enum DispatchError {
    #[error("BlockSummary consumer connect failed: {0}")]
    Connect(String),
    #[error("BlockSummary consumer subscriber failed: {0}")]
    Subscriber(String),
    #[error("BlockSummary consumer setup failed: {0}")]
    Other(String),
}
