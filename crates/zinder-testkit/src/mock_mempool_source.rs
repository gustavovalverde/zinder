//! [`MempoolSource`] fake for ingest and source layer tests.
//!
//! `MockMempoolSource` records how many times a consumer opens its event
//! stream and forwards scripted events into the most recently opened
//! stream. The mock exposes a control handle ([`MockMempoolSourceControl`])
//! that tests use to push [`MempoolSourceEvent`] values, push
//! [`SourceError`] errors, and close the stream.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use zinder_core::{BlockHash, BlockHeight, MempoolEvictionReason, TransactionId};
use zinder_source::{
    MempoolSource, MempoolSourceBackend, MempoolSourceCapabilities, MempoolSourceEntry,
    MempoolSourceEvent, MempoolSourceEventStream, SourceError,
};

/// A [`MempoolSource`] fake driven by a [`MockMempoolSourceControl`].
#[derive(Clone, Debug)]
pub struct MockMempoolSource {
    capabilities: MempoolSourceCapabilities,
    inner: Arc<MockMempoolSourceInner>,
}

#[derive(Debug)]
struct MockMempoolSourceInner {
    pending_sender: Mutex<Option<mpsc::UnboundedSender<Result<MempoolSourceEvent, SourceError>>>>,
    open_count: AtomicU32,
}

/// Control handle used by tests to push events into the most recently
/// opened [`MockMempoolSource`] stream.
#[derive(Clone, Debug)]
pub struct MockMempoolSourceControl {
    inner: Arc<MockMempoolSourceInner>,
}

/// Error returned when the test tries to push an event before the system
/// under test has opened a stream, or after the stream has been closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("mock mempool source has no open event stream")]
pub struct MockMempoolSourceClosed;

impl MockMempoolSource {
    /// Creates a streaming-backend mock, returning the control handle.
    #[must_use]
    pub fn streaming() -> (Self, MockMempoolSourceControl) {
        Self::with_capabilities(MempoolSourceCapabilities::streaming())
    }

    /// Creates a polling-backend mock, returning the control handle.
    #[must_use]
    pub fn polling() -> (Self, MockMempoolSourceControl) {
        Self::with_capabilities(MempoolSourceCapabilities::polling())
    }

    fn with_capabilities(
        capabilities: MempoolSourceCapabilities,
    ) -> (Self, MockMempoolSourceControl) {
        let inner = Arc::new(MockMempoolSourceInner {
            pending_sender: Mutex::new(None),
            open_count: AtomicU32::new(0),
        });
        let mock = Self {
            capabilities,
            inner: Arc::clone(&inner),
        };
        let control = MockMempoolSourceControl { inner };
        (mock, control)
    }
}

impl MockMempoolSourceControl {
    /// Pushes a hydrated `Added` event into the open stream.
    pub fn push_added(&self, entry: MempoolSourceEntry) -> Result<(), MockMempoolSourceClosed> {
        self.push_result(Ok(MempoolSourceEvent::Added(entry)))
    }

    /// Pushes an `Invalidated` event into the open stream.
    pub fn push_invalidated(
        &self,
        transaction_id: TransactionId,
        reason: MempoolEvictionReason,
    ) -> Result<(), MockMempoolSourceClosed> {
        self.push_result(Ok(MempoolSourceEvent::Invalidated {
            transaction_id,
            reason,
        }))
    }

    /// Pushes a `Mined` event into the open stream.
    pub fn push_mined(
        &self,
        transaction_id: TransactionId,
        mined_height: BlockHeight,
        block_hash: BlockHash,
    ) -> Result<(), MockMempoolSourceClosed> {
        self.push_result(Ok(MempoolSourceEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        }))
    }

    /// Pushes a [`SourceError`] item into the open stream.
    pub fn push_error(&self, error: SourceError) -> Result<(), MockMempoolSourceClosed> {
        self.push_result(Err(error))
    }

    /// Closes the currently open stream by dropping the sender.
    pub fn close_stream(&self) {
        *self.inner.pending_sender.lock() = None;
    }

    /// Returns the number of times the system under test has opened a
    /// fresh event stream.
    #[must_use]
    pub fn open_count(&self) -> u32 {
        self.inner.open_count.load(Ordering::SeqCst)
    }

    fn push_result(
        &self,
        outcome: Result<MempoolSourceEvent, SourceError>,
    ) -> Result<(), MockMempoolSourceClosed> {
        let active_sender = self
            .inner
            .pending_sender
            .lock()
            .as_ref()
            .cloned()
            .ok_or(MockMempoolSourceClosed)?;
        active_sender
            .send(outcome)
            .map_err(|_| MockMempoolSourceClosed)
    }
}

#[async_trait]
impl MempoolSource for MockMempoolSource {
    fn capabilities(&self) -> MempoolSourceCapabilities {
        self.capabilities
    }

    async fn events(&self) -> Result<MempoolSourceEventStream, SourceError> {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        *self.inner.pending_sender.lock() = Some(event_sender);
        self.inner.open_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(UnboundedReceiverStream::new(event_receiver)))
    }
}

impl MockMempoolSource {
    /// Returns the configured backend kind.
    #[must_use]
    pub const fn backend(&self) -> MempoolSourceBackend {
        self.capabilities.backend
    }
}

#[cfg(test)]
mod tests {
    use super::{MockMempoolSource, MockMempoolSourceClosed};
    use std::error::Error;
    use tokio_stream::StreamExt;
    use zinder_core::{
        AuthDigest, BlockHash, BlockHeight, MempoolEvictionReason, RawTransactionBytes,
        TransactionId, UnixTimestampMillis,
    };
    use zinder_source::{
        MempoolSource, MempoolSourceBackend, MempoolSourceEntry, MempoolSourceEvent,
    };

    fn sample_entry(transaction_id_byte: u8) -> MempoolSourceEntry {
        MempoolSourceEntry {
            transaction_id: TransactionId::from_bytes([transaction_id_byte; 32]),
            auth_digest: Some(AuthDigest::from_bytes([transaction_id_byte; 32])),
            raw_transaction_bytes: RawTransactionBytes::new(vec![transaction_id_byte; 16]),
            observed_at_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_mock_yields_pushed_events() -> Result<(), Box<dyn Error>> {
        let (mock, control) = MockMempoolSource::streaming();
        assert_eq!(mock.backend(), MempoolSourceBackend::Streaming);
        let mut stream = mock.events().await?;

        control.push_added(sample_entry(0x10))?;
        control.push_mined(
            TransactionId::from_bytes([0x11; 32]),
            BlockHeight::new(101),
            BlockHash::from_bytes([0x11; 32]),
        )?;
        control.close_stream();

        let first_event = stream.next().await.ok_or("expected first event")??;
        assert!(matches!(first_event, MempoolSourceEvent::Added(_)));

        let second_event = stream.next().await.ok_or("expected second event")??;
        assert!(matches!(second_event, MempoolSourceEvent::Mined { .. }));

        let after_close = stream.next().await;
        assert!(after_close.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pushing_before_open_returns_closed() {
        let (_mock, control) = MockMempoolSource::streaming();
        let outcome = control.push_invalidated(
            TransactionId::from_bytes([0x20; 32]),
            MempoolEvictionReason::Conflict,
        );
        assert_eq!(outcome, Err(MockMempoolSourceClosed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_count_increments_per_events_call() -> Result<(), Box<dyn Error>> {
        let (mock, control) = MockMempoolSource::polling();
        let _first = mock.events().await?;
        let _second = mock.events().await?;
        assert_eq!(control.open_count(), 2);
        Ok(())
    }
}
