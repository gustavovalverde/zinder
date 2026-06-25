//! Shared event-stream runner for resumable gRPC subscription adapters.
//!
//! One driver serves every cursor-bound event family (chain events, mempool
//! events). A family supplies a page-reader that returns the next page of wire
//! envelopes; the driver forwards them, advances the cursor from the envelope
//! it just sent, and idle-polls when the page is empty. The
//! [`EventEnvelope`] trait is the only family-specific seam the driver needs:
//! it extracts the resume cursor from a delivered envelope.

use std::{collections::VecDeque, future::Future, time::Duration};

use tokio::sync::mpsc;
use tonic::Status;
use zinder_proto::v1::wallet;

use crate::StreamCursorTokenV1;

const EVENT_STREAM_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Cursor-bearing wire envelope a stream family delivers to subscribers.
///
/// The driver is generic over this trait so one loop serves every family: it
/// reads the resume cursor from each delivered envelope and replays strictly
/// after it on the next page.
pub trait EventEnvelope {
    /// Returns the opaque resume-cursor bytes carried by this envelope.
    ///
    /// The driver reconstructs a [`StreamCursorTokenV1`] from these bytes and
    /// passes it to the next page read so delivery resumes strictly after the
    /// envelope just sent.
    fn cursor_bytes(&self) -> &[u8];
}

impl EventEnvelope for wallet::ChainEventEnvelope {
    fn cursor_bytes(&self) -> &[u8] {
        &self.cursor
    }
}

impl EventEnvelope for wallet::MempoolEventEnvelope {
    fn cursor_bytes(&self) -> &[u8] {
        &self.cursor
    }
}

/// Repeatedly reads event pages for one family and sends them to a gRPC sink.
///
/// A page may lead with a server-injected synthetic envelope (for chain
/// events, a `ChainReorged` reconnect-reorg recovery); the runner forwards it
/// like any other envelope and advances the cursor from the envelope it just
/// sent, so the next page always makes forward progress. An empty page idle-
/// polls until either the interval elapses or the receiver closes.
pub async fn run_event_stream<E, ReadPage, ReadPageFuture>(
    mut cursor: Option<StreamCursorTokenV1>,
    mut read_page: ReadPage,
    event_sender: mpsc::Sender<Result<E, Status>>,
) where
    E: EventEnvelope + Send + 'static,
    ReadPage: FnMut(Option<StreamCursorTokenV1>) -> ReadPageFuture + Send + 'static,
    ReadPageFuture: Future<Output = Result<Vec<E>, Status>> + Send,
{
    let mut queued_events: VecDeque<E> = VecDeque::new();

    loop {
        if let Some(event_envelope) = queued_events.pop_front() {
            cursor = Some(StreamCursorTokenV1::from_bytes(
                event_envelope.cursor_bytes().to_vec(),
            ));
            if event_sender.send(Ok(event_envelope)).await.is_err() {
                return;
            }
            continue;
        }

        match read_page(cursor.clone()).await {
            Ok(event_envelopes) if event_envelopes.is_empty() => {
                // Race the polling sleep against receiver closure so the task
                // exits cleanly on server shutdown when no further events are
                // produced.
                tokio::select! {
                    () = tokio::time::sleep(EVENT_STREAM_IDLE_POLL_INTERVAL) => {}
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
