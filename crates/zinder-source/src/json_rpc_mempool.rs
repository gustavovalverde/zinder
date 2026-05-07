//! JSON-RPC mempool polling source.
//!
//! This adapter implements [`MempoolSource`] by diffing successive
//! `getrawmempool` snapshots from a Zebra-compatible node. New txids are
//! hydrated through `getrawtransaction(verbose=0)`. Disappeared txids are
//! classified through `getrawtransaction(verbose=1)`: a mined response
//! produces [`MempoolSourceEvent::Mined`]; a `not found` response produces
//! [`MempoolSourceEvent::Invalidated`] with reason
//! [`zinder_core::MempoolEvictionReason::Unknown`].
//!
//! The polling backend is the fallback path used when the upstream Zebra
//! deployment does not expose the streaming indexer port. The streaming
//! [`crate::ZebraIndexerMempoolSource`] is preferred.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::TryStreamExt;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::ReceiverStream;
use zinder_core::{MempoolEvictionReason, TransactionId, UnixTimestampMillis};

use crate::{
    MempoolHydrationFailureReason, MempoolSource, MempoolSourceCapabilities, MempoolSourceEntry,
    MempoolSourceEvent, MempoolSourceEventStream, SourceError, UpstreamTransactionLookup,
    ZebraJsonRpcSource,
};

const JSON_RPC_POLLING_BACKEND_LABEL: &str = "json_rpc_polling";

fn increment_polling_hydration_failure(reason: MempoolHydrationFailureReason) {
    metrics::counter!(
        "zinder_mempool_hydration_failures_total",
        "backend" => JSON_RPC_POLLING_BACKEND_LABEL,
        "reason" => reason.as_label()
    )
    .increment(1);
}

/// Default cadence between mempool polls.
///
/// Five seconds matches the trade-off between hydration cost and freshness
/// observed in the M3 mempool spec evidence: polling at 100 ms (Zaino's
/// default) is fine on a small mempool but consumes JSON-RPC budget that
/// is also needed by chain ingestion. Operators on small deployments can
/// shorten the interval; operators on busy nodes should keep it at the
/// default until streaming ingestion is available.
pub const DEFAULT_MEMPOOL_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum number of concurrent `getrawtransaction` round-trips during a single poll.
///
/// Bounds upstream node load while still amortizing RPC RTT across burst
/// arrivals; with the default value, a 100-tx burst hydrates in roughly
/// 100/16 ≈ 7 RPC round-trips instead of 100.
const MEMPOOL_POLL_HYDRATION_CONCURRENCY: usize = 16;

/// Runtime options for [`JsonRpcMempoolSource`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonRpcMempoolSourceOptions {
    /// Cadence between successive `getrawmempool` snapshots.
    pub poll_interval: Duration,
    /// Channel buffer size for the emitted source event stream.
    pub event_channel_capacity: usize,
}

impl Default for JsonRpcMempoolSourceOptions {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_MEMPOOL_POLL_INTERVAL,
            event_channel_capacity: 64,
        }
    }
}

/// Mempool source that polls a Zebra-compatible JSON-RPC endpoint.
#[derive(Clone)]
pub struct JsonRpcMempoolSource {
    json_rpc: ZebraJsonRpcSource,
    options: JsonRpcMempoolSourceOptions,
}

impl JsonRpcMempoolSource {
    /// Creates a polling mempool source backed by `json_rpc`.
    #[must_use]
    pub fn new(json_rpc: ZebraJsonRpcSource) -> Self {
        Self::with_options(json_rpc, JsonRpcMempoolSourceOptions::default())
    }

    /// Creates a polling mempool source with explicit options.
    #[must_use]
    pub const fn with_options(
        json_rpc: ZebraJsonRpcSource,
        options: JsonRpcMempoolSourceOptions,
    ) -> Self {
        Self { json_rpc, options }
    }
}

#[async_trait]
impl MempoolSource for JsonRpcMempoolSource {
    fn capabilities(&self) -> MempoolSourceCapabilities {
        MempoolSourceCapabilities::polling()
    }

    async fn events(&self) -> Result<MempoolSourceEventStream, SourceError> {
        let (event_sender, event_receiver) = mpsc::channel(self.options.event_channel_capacity);
        let json_rpc = self.json_rpc.clone();
        let poll_interval = self.options.poll_interval;
        let known_transaction_ids: Arc<Mutex<HashSet<TransactionId>>> =
            Arc::new(Mutex::new(HashSet::new()));

        tokio::spawn(async move {
            run_polling_loop(json_rpc, poll_interval, known_transaction_ids, event_sender).await;
        });

        Ok(Box::pin(ReceiverStream::new(event_receiver)))
    }
}

async fn run_polling_loop(
    json_rpc: ZebraJsonRpcSource,
    poll_interval: Duration,
    known_transaction_ids: Arc<Mutex<HashSet<TransactionId>>>,
    event_sender: mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) {
    loop {
        let observed_at = UnixTimestampMillis::now();
        match poll_once(
            &json_rpc,
            &known_transaction_ids,
            observed_at,
            &event_sender,
        )
        .await
        {
            Ok(()) => {}
            Err(send_failed) if send_failed.is_send_failure() => return,
            Err(send_failed) => {
                let send_outcome = event_sender
                    .send(Err(send_failed.into_source_error()))
                    .await;
                if send_outcome.is_err() {
                    return;
                }
            }
        }
        sleep(poll_interval).await;
    }
}

/// Outcome of a single poll iteration that escaped the local handlers.
enum PollFailure {
    Source(SourceError),
    ReceiverGone,
}

impl PollFailure {
    fn is_send_failure(&self) -> bool {
        matches!(self, Self::ReceiverGone)
    }

    fn into_source_error(self) -> SourceError {
        match self {
            Self::Source(error) => error,
            Self::ReceiverGone => SourceError::NodeUnavailable {
                is_retryable: true,
                reason: "mempool source consumer dropped the event stream".to_owned(),
            },
        }
    }
}

async fn poll_once(
    json_rpc: &ZebraJsonRpcSource,
    known_transaction_ids: &Arc<Mutex<HashSet<TransactionId>>>,
    observed_at: UnixTimestampMillis,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<(), PollFailure> {
    let observed_transaction_ids: HashSet<TransactionId> = json_rpc
        .fetch_raw_mempool_transaction_ids()
        .await
        .map_err(PollFailure::Source)?
        .into_iter()
        .collect();

    let (added_transaction_ids, removed_transaction_ids) =
        diff_known_state(known_transaction_ids, &observed_transaction_ids);

    futures_util::stream::iter(added_transaction_ids.into_iter().map(Ok::<_, PollFailure>))
        .try_for_each_concurrent(
            MEMPOOL_POLL_HYDRATION_CONCURRENCY,
            |transaction_id| async move {
                let observation =
                    emit_added_event(json_rpc, transaction_id, observed_at, event_sender).await?;
                if observation.should_advance_known_state() {
                    remember_added_transaction_id(known_transaction_ids, transaction_id);
                }
                Ok(())
            },
        )
        .await?;
    futures_util::stream::iter(
        removed_transaction_ids
            .into_iter()
            .map(Ok::<_, PollFailure>),
    )
    .try_for_each_concurrent(
        MEMPOOL_POLL_HYDRATION_CONCURRENCY,
        |transaction_id| async move {
            let observation =
                emit_disappearance_event(json_rpc, transaction_id, event_sender).await?;
            if observation.should_advance_known_state() {
                forget_removed_transaction_id(known_transaction_ids, transaction_id);
            }
            Ok(())
        },
    )
    .await?;
    Ok(())
}

fn diff_known_state(
    known_transaction_ids: &Arc<Mutex<HashSet<TransactionId>>>,
    observed_transaction_ids: &HashSet<TransactionId>,
) -> (Vec<TransactionId>, Vec<TransactionId>) {
    let known_state = known_transaction_ids.lock();
    let added: Vec<TransactionId> = observed_transaction_ids
        .difference(&known_state)
        .copied()
        .collect();
    let removed: Vec<TransactionId> = known_state
        .difference(observed_transaction_ids)
        .copied()
        .collect();
    drop(known_state);
    (added, removed)
}

fn remember_added_transaction_id(
    known_transaction_ids: &Arc<Mutex<HashSet<TransactionId>>>,
    transaction_id: TransactionId,
) {
    known_transaction_ids.lock().insert(transaction_id);
}

fn forget_removed_transaction_id(
    known_transaction_ids: &Arc<Mutex<HashSet<TransactionId>>>,
    transaction_id: TransactionId,
) {
    known_transaction_ids.lock().remove(&transaction_id);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationEmission {
    Emitted,
    PendingRetry,
}

impl ObservationEmission {
    const fn should_advance_known_state(self) -> bool {
        matches!(self, Self::Emitted)
    }
}

async fn emit_added_event(
    json_rpc: &ZebraJsonRpcSource,
    transaction_id: TransactionId,
    observed_at: UnixTimestampMillis,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<ObservationEmission, PollFailure> {
    let hydration_outcome = json_rpc.fetch_raw_transaction_bytes(transaction_id).await;
    match hydration_outcome {
        Ok(Some(raw_transaction_bytes)) => {
            let entry = MempoolSourceEntry {
                transaction_id,
                auth_digest: None,
                raw_transaction_bytes,
                observed_at_unix_millis: observed_at,
            };
            forward_event(MempoolSourceEvent::Added(entry), event_sender)
                .await
                .map(|()| ObservationEmission::Emitted)
        }
        Ok(None) => {
            increment_polling_hydration_failure(MempoolHydrationFailureReason::NotFound);
            Ok(ObservationEmission::PendingRetry)
        }
        Err(hydration_error) => {
            increment_polling_hydration_failure(MempoolHydrationFailureReason::RpcError);
            forward_error(hydration_error, event_sender)
                .await
                .map(|()| ObservationEmission::PendingRetry)
        }
    }
}

async fn emit_disappearance_event(
    json_rpc: &ZebraJsonRpcSource,
    transaction_id: TransactionId,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<ObservationEmission, PollFailure> {
    match json_rpc
        .fetch_upstream_transaction_lookup(transaction_id)
        .await
    {
        Ok(UpstreamTransactionLookup::Mined(mined_height)) => forward_event(
            MempoolSourceEvent::Mined {
                transaction_id,
                mined_height,
            },
            event_sender,
        )
        .await
        .map(|()| ObservationEmission::Emitted),
        Ok(UpstreamTransactionLookup::NotFound) => forward_event(
            MempoolSourceEvent::Invalidated {
                transaction_id,
                reason: MempoolEvictionReason::Unknown,
            },
            event_sender,
        )
        .await
        .map(|()| ObservationEmission::Emitted),
        Ok(UpstreamTransactionLookup::InMempool) => {
            // Source reports the txid is still in the mempool; the diff
            // observed it as removed. Treat as a transient race and let
            // the next poll observe the true state.
            Ok(ObservationEmission::PendingRetry)
        }
        Err(lookup_error) => forward_error(lookup_error, event_sender)
            .await
            .map(|()| ObservationEmission::PendingRetry),
    }
}

async fn forward_event(
    source_event: MempoolSourceEvent,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<(), PollFailure> {
    event_sender
        .send(Ok(source_event))
        .await
        .map_err(|_| PollFailure::ReceiverGone)
}

async fn forward_error(
    source_error: SourceError,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<(), PollFailure> {
    event_sender
        .send(Err(source_error))
        .await
        .map_err(|_| PollFailure::ReceiverGone)
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;

    fn transaction_id_with_byte(byte: u8) -> TransactionId {
        TransactionId::from_bytes([byte; 32])
    }

    #[test]
    fn diff_known_state_classifies_added_and_removed() {
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::from([
            transaction_id_with_byte(0x01),
            transaction_id_with_byte(0x02),
        ])));
        let observed = HashSet::from([
            transaction_id_with_byte(0x02),
            transaction_id_with_byte(0x03),
        ]);

        let (added, removed) = diff_known_state(&known_transaction_ids, &observed);

        assert_eq!(added, vec![transaction_id_with_byte(0x03)]);
        assert_eq!(removed, vec![transaction_id_with_byte(0x01)]);
    }

    #[test]
    fn added_observation_stays_pending_until_event_is_emitted() {
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let observed = HashSet::from([transaction_id_with_byte(0xAA)]);

        let (added, removed) = diff_known_state(&known_transaction_ids, &observed);

        assert_eq!(added, vec![transaction_id_with_byte(0xAA)]);
        assert!(removed.is_empty());
        assert!(known_transaction_ids.lock().is_empty());

        let (retry_added, retry_removed) = diff_known_state(&known_transaction_ids, &observed);
        assert_eq!(retry_added, vec![transaction_id_with_byte(0xAA)]);
        assert!(retry_removed.is_empty());

        remember_added_transaction_id(&known_transaction_ids, transaction_id_with_byte(0xAA));
        let (after_emit_added, after_emit_removed) =
            diff_known_state(&known_transaction_ids, &observed);
        assert!(after_emit_added.is_empty());
        assert!(after_emit_removed.is_empty());
    }

    #[test]
    fn removed_observation_stays_pending_until_event_is_emitted() {
        let known_transaction_ids =
            Arc::new(Mutex::new(HashSet::from([transaction_id_with_byte(0xBB)])));
        let observed = HashSet::new();

        let (added, removed) = diff_known_state(&known_transaction_ids, &observed);
        assert!(added.is_empty());
        assert_eq!(removed, vec![transaction_id_with_byte(0xBB)]);
        assert!(
            known_transaction_ids
                .lock()
                .contains(&transaction_id_with_byte(0xBB))
        );

        let (retry_added, retry_removed) = diff_known_state(&known_transaction_ids, &observed);
        assert!(retry_added.is_empty());
        assert_eq!(retry_removed, vec![transaction_id_with_byte(0xBB)]);

        forget_removed_transaction_id(&known_transaction_ids, transaction_id_with_byte(0xBB));
        let (after_emit_added, after_emit_removed) =
            diff_known_state(&known_transaction_ids, &observed);
        assert!(after_emit_added.is_empty());
        assert!(after_emit_removed.is_empty());
    }

    #[test]
    fn diff_known_state_yields_empty_diffs_when_unchanged() {
        let initial = HashSet::from([transaction_id_with_byte(0x40)]);
        let known_transaction_ids = Arc::new(Mutex::new(initial.clone()));

        let (added, removed) = diff_known_state(&known_transaction_ids, &initial);

        assert!(added.is_empty());
        assert!(removed.is_empty());
    }
}
