//! Shared chain-event stream runner for gRPC adapters.

use std::{collections::VecDeque, future::Future, time::Duration};

use tokio::sync::mpsc;
use tonic::Status;
use zinder_proto::v1::wallet;

use crate::StreamCursorTokenV1;

const CHAIN_EVENT_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Repeatedly reads chain-event pages and sends them to a gRPC stream sink.
///
/// A page may lead with a server-injected synthetic `ChainReorged` envelope
/// (reconnect-reorg recovery); the runner forwards it like any other envelope
/// and advances the cursor from the envelope it just sent, so the next page
/// always makes forward progress.
pub async fn run_chain_event_stream<ReadPage, ReadPageFuture>(
    mut cursor: Option<StreamCursorTokenV1>,
    mut read_page: ReadPage,
    event_sender: mpsc::Sender<Result<wallet::ChainEventEnvelope, Status>>,
) where
    ReadPage: FnMut(Option<StreamCursorTokenV1>) -> ReadPageFuture + Send + 'static,
    ReadPageFuture: Future<Output = Result<Vec<wallet::ChainEventEnvelope>, Status>> + Send,
{
    let mut queued_events: VecDeque<wallet::ChainEventEnvelope> = VecDeque::new();

    loop {
        if let Some(event_envelope) = queued_events.pop_front() {
            cursor = Some(StreamCursorTokenV1::from_bytes(
                event_envelope.cursor.clone(),
            ));
            if event_sender.send(Ok(event_envelope)).await.is_err() {
                return;
            }
            continue;
        }

        match read_page(cursor.clone()).await {
            Ok(event_envelopes) if event_envelopes.is_empty() => {
                // Same pattern as `stream_mempool_events`: race the polling
                // sleep against receiver closure so the task exits cleanly
                // on server shutdown when no further events are produced.
                tokio::select! {
                    () = tokio::time::sleep(CHAIN_EVENT_IDLE_POLL_INTERVAL) => {}
                    () = event_sender.closed() => return,
                }
            }
            Ok(event_envelopes) => {
                queued_events = event_envelopes.into();
            }
            Err(error) => {
                let _send_result = event_sender.send(Err(error)).await;
                return;
            }
        }
    }
}
