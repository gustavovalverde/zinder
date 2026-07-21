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
use futures_util::StreamExt as _;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::ReceiverStream;
use zinder_core::{MempoolEvictionReason, TransactionId, UnixTimestampMillis};

use crate::{
    MempoolHydrationFailureReason, MempoolSource, MempoolSourceCapabilities, MempoolSourceEntry,
    MempoolSourceEvent, MempoolSourceEventStream, NodeSource, SourceError,
    UpstreamTransactionLookup, ZebraJsonRpcSource,
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
/// Five seconds matches the trade-off between hydration cost and freshness.
/// Operators on small deployments can shorten the interval; operators on busy
/// nodes should keep it at the default until streaming ingestion is available.
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
    let mut certified_source_tip = None;
    loop {
        let observed_at = UnixTimestampMillis::now();
        match poll_once(
            &json_rpc,
            &known_transaction_ids,
            observed_at,
            &event_sender,
            certified_source_tip,
        )
        .await
        {
            Ok(PollCompletion::Complete { source_tip }) => {
                if certified_source_tip.is_none() {
                    certified_source_tip = Some(source_tip);
                }
            }
            Ok(PollCompletion::RetryNeeded) => {}
            Ok(PollCompletion::SourceTipChanged {
                certified_source_tip,
                observed_source_tip,
            }) => {
                let _send_outcome = event_sender
                    .send(Err(SourceError::MempoolStreamUnavailable {
                        reason: format!(
                            "mempool source tip changed from {certified_source_tip:?} to {observed_source_tip:?}"
                        ),
                    }))
                    .await;
                return;
            }
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

/// Whether a poll produced a complete, externally usable snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollCompletion {
    /// Every observed addition/removal was emitted and the known state now
    /// matches this poll's source snapshot.
    Complete {
        /// Stable upstream best-chain tip that fences this poll.
        source_tip: zinder_core::BlockId,
    },
    /// A hydration or lookup race needs another poll before the first snapshot
    /// can be declared complete.
    RetryNeeded,
    /// The upstream best chain differs from the tip that certified the
    /// currently exposed generation.
    SourceTipChanged {
        certified_source_tip: zinder_core::BlockId,
        observed_source_tip: zinder_core::BlockId,
    },
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
    certified_source_tip: Option<zinder_core::BlockId>,
) -> Result<PollCompletion, PollFailure> {
    let source_tip_before = json_rpc.tip_id().await.map_err(PollFailure::Source)?;
    if let Some(certified_source_tip) = certified_source_tip
        && certified_source_tip != source_tip_before
    {
        return Ok(PollCompletion::SourceTipChanged {
            certified_source_tip,
            observed_source_tip: source_tip_before,
        });
    }
    let observed_transaction_ids: HashSet<TransactionId> = json_rpc
        .fetch_raw_mempool_transaction_ids()
        .await
        .map_err(PollFailure::Source)?
        .into_iter()
        .collect();

    let (added_transaction_ids, removed_transaction_ids) =
        diff_known_state(known_transaction_ids, &observed_transaction_ids);

    let mut pending_retry = false;
    for (transaction_ids, transition_kind) in [
        (added_transaction_ids, PollTransitionKind::Added),
        (removed_transaction_ids, PollTransitionKind::Removed),
    ] {
        for transaction_batch in transaction_ids.chunks(MEMPOOL_POLL_HYDRATION_CONCURRENCY) {
            match poll_transition_batch(
                json_rpc,
                known_transaction_ids,
                transaction_batch,
                transition_kind,
                observed_at,
                source_tip_before,
                event_sender,
            )
            .await?
            {
                PollBatchCompletion::Complete => {}
                PollBatchCompletion::RetryNeeded => {
                    pending_retry = true;
                }
                PollBatchCompletion::SourceTipChanged {
                    observed_source_tip,
                } => {
                    return source_tip_change_outcome(
                        certified_source_tip,
                        source_tip_before,
                        observed_source_tip,
                    );
                }
            }
        }
    }
    certify_poll_completion(
        json_rpc,
        event_sender,
        certified_source_tip,
        source_tip_before,
        pending_retry,
    )
    .await
}

async fn certify_poll_completion(
    json_rpc: &ZebraJsonRpcSource,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
    certified_source_tip: Option<zinder_core::BlockId>,
    source_tip_before: zinder_core::BlockId,
    pending_retry: bool,
) -> Result<PollCompletion, PollFailure> {
    let initial_snapshot_permit = if certified_source_tip.is_none() && !pending_retry {
        Some(
            event_sender
                .reserve()
                .await
                .map_err(|_| PollFailure::ReceiverGone)?,
        )
    } else {
        None
    };
    let source_tip_after = json_rpc.tip_id().await.map_err(PollFailure::Source)?;
    if source_tip_before != source_tip_after {
        return source_tip_change_outcome(
            certified_source_tip,
            source_tip_before,
            source_tip_after,
        );
    }
    if pending_retry && certified_source_tip.is_none() {
        return Err(PollFailure::Source(SourceError::MempoolStreamUnavailable {
            reason: "initial mempool snapshot hydration was incomplete".to_owned(),
        }));
    }
    if let Some(initial_snapshot_permit) = initial_snapshot_permit {
        initial_snapshot_permit.send(Ok(MempoolSourceEvent::InitialSnapshotComplete {
            source_tip: source_tip_before,
        }));
    }
    Ok(if pending_retry {
        PollCompletion::RetryNeeded
    } else {
        PollCompletion::Complete {
            source_tip: source_tip_before,
        }
    })
}

fn source_tip_change_outcome(
    certified_source_tip: Option<zinder_core::BlockId>,
    source_tip_before: zinder_core::BlockId,
    observed_source_tip: zinder_core::BlockId,
) -> Result<PollCompletion, PollFailure> {
    certified_source_tip.map_or_else(
        || {
            Err(PollFailure::Source(SourceError::MempoolStreamUnavailable {
                reason: format!(
                    "source tip changed from {source_tip_before:?} to {observed_source_tip:?} while constructing the initial mempool snapshot"
                ),
            }))
        },
        |certified_source_tip| {
            Ok(PollCompletion::SourceTipChanged {
                certified_source_tip,
                observed_source_tip,
            })
        },
    )
}

#[derive(Clone, Copy)]
enum PollTransitionKind {
    Added,
    Removed,
}

enum PollBatchCompletion {
    Complete,
    RetryNeeded,
    SourceTipChanged {
        observed_source_tip: zinder_core::BlockId,
    },
}

#[allow(
    clippy::too_many_arguments,
    reason = "The batch boundary receives the complete source-tip and delivery contract explicitly."
)]
async fn poll_transition_batch(
    json_rpc: &ZebraJsonRpcSource,
    known_transaction_ids: &Arc<Mutex<HashSet<TransactionId>>>,
    transaction_ids: &[TransactionId],
    transition_kind: PollTransitionKind,
    observed_at: UnixTimestampMillis,
    expected_source_tip: zinder_core::BlockId,
    event_sender: &mpsc::Sender<Result<MempoolSourceEvent, SourceError>>,
) -> Result<PollBatchCompletion, PollFailure> {
    let observations = futures_util::stream::iter(transaction_ids.iter().copied())
        .map(|transaction_id| async move {
            match transition_kind {
                PollTransitionKind::Added => {
                    observe_added_event(json_rpc, transaction_id, observed_at).await
                }
                PollTransitionKind::Removed => {
                    observe_disappearance_event(json_rpc, transaction_id).await
                }
            }
        })
        .buffer_unordered(MEMPOOL_POLL_HYDRATION_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let observed_source_tip = json_rpc.tip_id().await.map_err(PollFailure::Source)?;
    if observed_source_tip != expected_source_tip {
        return Ok(PollBatchCompletion::SourceTipChanged {
            observed_source_tip,
        });
    }

    let mut pending_retry = false;
    for observation in observations {
        match observation {
            PollObservation::Transition {
                event,
                transaction_id,
            } => {
                forward_event(event, event_sender).await?;
                match transition_kind {
                    PollTransitionKind::Added => {
                        remember_added_transaction_id(known_transaction_ids, transaction_id);
                    }
                    PollTransitionKind::Removed => {
                        forget_removed_transaction_id(known_transaction_ids, transaction_id);
                    }
                }
            }
            PollObservation::Retry => pending_retry = true,
            PollObservation::RetryAfterError(source_error) => {
                forward_error(source_error, event_sender).await?;
                pending_retry = true;
            }
        }
    }
    Ok(if pending_retry {
        PollBatchCompletion::RetryNeeded
    } else {
        PollBatchCompletion::Complete
    })
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

enum PollObservation {
    Transition {
        event: MempoolSourceEvent,
        transaction_id: TransactionId,
    },
    Retry,
    RetryAfterError(SourceError),
}

async fn observe_added_event(
    json_rpc: &ZebraJsonRpcSource,
    transaction_id: TransactionId,
    observed_at: UnixTimestampMillis,
) -> PollObservation {
    let hydration_outcome = json_rpc.fetch_raw_transaction_bytes(transaction_id).await;
    match hydration_outcome {
        Ok(Some(raw_transaction_bytes)) => {
            let entry = MempoolSourceEntry {
                transaction_id,
                auth_digest: None,
                raw_transaction_bytes,
                observed_at_unix_millis: observed_at,
            };
            PollObservation::Transition {
                event: MempoolSourceEvent::Added(entry),
                transaction_id,
            }
        }
        Ok(None) => {
            increment_polling_hydration_failure(MempoolHydrationFailureReason::NotFound);
            PollObservation::Retry
        }
        Err(hydration_error) => {
            increment_polling_hydration_failure(MempoolHydrationFailureReason::RpcError);
            PollObservation::RetryAfterError(hydration_error)
        }
    }
}

async fn observe_disappearance_event(
    json_rpc: &ZebraJsonRpcSource,
    transaction_id: TransactionId,
) -> PollObservation {
    match json_rpc
        .fetch_upstream_transaction_lookup(transaction_id)
        .await
    {
        Ok(UpstreamTransactionLookup::Mined {
            mined_height,
            block_hash,
        }) => PollObservation::Transition {
            event: MempoolSourceEvent::Mined {
                transaction_id,
                mined_height,
                block_hash,
            },
            transaction_id,
        },
        Ok(UpstreamTransactionLookup::NotFound) => PollObservation::Transition {
            event: MempoolSourceEvent::Invalidated {
                transaction_id,
                reason: MempoolEvictionReason::Unknown,
            },
            transaction_id,
        },
        Ok(UpstreamTransactionLookup::InMempool) => {
            // Source reports the txid is still in the mempool; the diff
            // observed it as removed. Treat as a transient race and let
            // the next poll observe the true state.
            PollObservation::Retry
        }
        Err(lookup_error) => PollObservation::RetryAfterError(lookup_error),
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

    #[tokio::test]
    async fn polling_certifies_an_empty_initial_snapshot_at_one_stable_source_tip()
    -> eyre::Result<()> {
        let source_tip_response = serde_json::json!({
            "height": 7,
            "hash": vec![0x11; 32],
        });
        let server = zinder_testkit::JsonRpcTestServer::start([
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(source_tip_response.clone()),
            ),
            zinder_testkit::method("getrawmempool")
                .reply(zinder_testkit::RpcReply::result(serde_json::json!([]))),
            zinder_testkit::method("getbestblockheightandhash")
                .reply(zinder_testkit::RpcReply::result(source_tip_response)),
        ])?;
        let json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, mut event_receiver) = mpsc::channel(4);
        let poll_task = tokio::spawn(run_polling_loop(
            json_rpc,
            Duration::from_mins(1),
            known_transaction_ids,
            event_sender,
        ));

        let event = tokio::time::timeout(Duration::from_secs(1), event_receiver.recv())
            .await?
            .ok_or_else(|| eyre::eyre!("polling source closed before its completion marker"))??;
        assert_eq!(
            event,
            MempoolSourceEvent::InitialSnapshotComplete {
                source_tip: zinder_core::BlockId::new(
                    zinder_core::BlockHeight::new(7),
                    zinder_core::BlockHash::from_bytes([0x11; 32]),
                ),
            }
        );

        poll_task.abort();
        let _ = poll_task.await;
        Ok(())
    }

    #[tokio::test]
    async fn polling_rejects_an_initial_generation_with_pending_hydration() -> eyre::Result<()> {
        let transaction_id_hex = "A1".repeat(32);
        let source_tip_response = serde_json::json!({
            "height": 7,
            "hash": vec![0x11; 32],
        });
        let server = zinder_testkit::JsonRpcTestServer::start([
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(source_tip_response.clone()),
            ),
            zinder_testkit::method("getrawmempool").reply(zinder_testkit::RpcReply::result(
                serde_json::json!([transaction_id_hex]),
            )),
            zinder_testkit::method("getrawtransaction").reply(
                zinder_testkit::RpcReply::error_with_code(
                    -5,
                    "No such mempool or blockchain transaction",
                ),
            ),
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(source_tip_response.clone()),
            ),
            zinder_testkit::method("getbestblockheightandhash")
                .reply(zinder_testkit::RpcReply::result(source_tip_response)),
        ])?;
        let json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, mut event_receiver) = mpsc::channel(4);

        let outcome = poll_once(
            &json_rpc,
            &known_transaction_ids,
            UnixTimestampMillis::new(1_750_000_000_000),
            &event_sender,
            None,
        )
        .await;
        assert!(matches!(
            outcome,
            Err(PollFailure::Source(
                SourceError::MempoolStreamUnavailable { .. }
            ))
        ));
        assert!(
            event_receiver.try_recv().is_err(),
            "a pending hydration must not emit a source event or completion marker"
        );
        assert!(known_transaction_ids.lock().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn polling_rejects_an_initial_generation_when_its_source_tip_changes() -> eyre::Result<()>
    {
        let transaction_id_hex = "11".repeat(32);
        let server = zinder_testkit::JsonRpcTestServer::start([
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(serde_json::json!({
                    "height": 7,
                    "hash": vec![0x07; 32],
                })),
            ),
            zinder_testkit::method("getrawmempool").reply(zinder_testkit::RpcReply::result(
                serde_json::json!([transaction_id_hex]),
            )),
            zinder_testkit::method("getrawtransaction").reply(zinder_testkit::RpcReply::result(
                serde_json::json!("deadbeef"),
            )),
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(serde_json::json!({
                    "height": 8,
                    "hash": vec![0x08; 32],
                })),
            ),
        ])?;
        let json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, mut event_receiver) = mpsc::channel(4);

        let outcome = poll_once(
            &json_rpc,
            &known_transaction_ids,
            UnixTimestampMillis::new(1_750_000_000_000),
            &event_sender,
            None,
        )
        .await;

        assert!(matches!(
            outcome,
            Err(PollFailure::Source(
                SourceError::MempoolStreamUnavailable { .. }
            ))
        ));
        assert!(
            event_receiver.try_recv().is_err(),
            "an unstable source snapshot must publish no partial transitions"
        );
        assert!(
            known_transaction_ids.lock().is_empty(),
            "an unstable source snapshot must not advance the polling baseline"
        );
        Ok(())
    }

    #[tokio::test]
    async fn polling_stops_before_observing_a_new_source_tip_generation() -> eyre::Result<()> {
        let certified_source_tip = zinder_core::BlockId::new(
            zinder_core::BlockHeight::new(7),
            zinder_core::BlockHash::from_bytes([0x07; 32]),
        );
        let server = zinder_testkit::JsonRpcTestServer::start([zinder_testkit::method(
            "getbestblockheightandhash",
        )
        .reply(zinder_testkit::RpcReply::result(serde_json::json!({
            "height": 8,
            "hash": vec![0x08; 32],
        })))])?;
        let json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, mut event_receiver) = mpsc::channel(4);

        let completion = poll_once(
            &json_rpc,
            &known_transaction_ids,
            UnixTimestampMillis::new(1_750_000_000_000),
            &event_sender,
            Some(certified_source_tip),
        )
        .await
        .map_err(|_| eyre::eyre!("a new source tip must end the generation cleanly"))?;

        assert!(matches!(
            completion,
            PollCompletion::SourceTipChanged { .. }
        ));
        assert!(event_receiver.try_recv().is_err());
        assert!(server.requests_for("getrawmempool")?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn polling_hydration_obeys_event_channel_backpressure() -> eyre::Result<()> {
        const TRANSACTION_COUNT: usize = MEMPOOL_POLL_HYDRATION_CONCURRENCY * 4;
        let transaction_ids = (0..TRANSACTION_COUNT)
            .map(|index| format!("{:02x}", index.saturating_add(1)).repeat(32))
            .collect::<Vec<_>>();
        let source_tip_response = serde_json::json!({
            "height": 7,
            "hash": vec![0x07; 32],
        });
        let mut stubs = vec![
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(source_tip_response.clone()),
            ),
            zinder_testkit::method("getrawmempool").reply(zinder_testkit::RpcReply::result(
                serde_json::json!(transaction_ids),
            )),
        ];
        stubs.extend((0..TRANSACTION_COUNT).map(|_| {
            zinder_testkit::method("getrawtransaction").reply(zinder_testkit::RpcReply::result(
                serde_json::json!("deadbeef"),
            ))
        }));
        stubs.extend((0..5).map(|_| {
            zinder_testkit::method("getbestblockheightandhash").reply(
                zinder_testkit::RpcReply::result(source_tip_response.clone()),
            )
        }));
        let server = zinder_testkit::JsonRpcTestServer::start(stubs)?;
        let json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, _event_receiver) = mpsc::channel(1);
        let poll_task = tokio::spawn(async move {
            poll_once(
                &json_rpc,
                &known_transaction_ids,
                UnixTimestampMillis::new(1_750_000_000_000),
                &event_sender,
                None,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .requests_for("getrawtransaction")
                    .unwrap_or_default()
                    .len()
                    >= MEMPOOL_POLL_HYDRATION_CONCURRENCY
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(25)).await;
        let hydrated_request_count = server.requests_for("getrawtransaction")?.len();
        poll_task.abort();
        let _ = poll_task.await;

        assert!(
            hydrated_request_count < TRANSACTION_COUNT,
            "a blocked consumer must stop polling hydration before all raw transactions are retained"
        );
        Ok(())
    }

    #[tokio::test]
    async fn polling_rechecks_tip_after_waiting_for_snapshot_marker_capacity() -> eyre::Result<()> {
        let transaction_ids = ["11".repeat(32)];
        let source_tip_before = serde_json::json!({
            "height": 7,
            "hash": vec![0x07; 32],
        });
        let source_tip_after = serde_json::json!({
            "height": 8,
            "hash": vec![0x08; 32],
        });
        let server = zinder_testkit::JsonRpcTestServer::start([
            zinder_testkit::method("getbestblockheightandhash")
                .reply(zinder_testkit::RpcReply::result(source_tip_before.clone())),
            zinder_testkit::method("getrawmempool").reply(zinder_testkit::RpcReply::result(
                serde_json::json!(transaction_ids),
            )),
            zinder_testkit::method("getrawtransaction").reply(zinder_testkit::RpcReply::result(
                serde_json::json!("deadbeef"),
            )),
            zinder_testkit::method("getbestblockheightandhash")
                .reply(zinder_testkit::RpcReply::result(source_tip_before)),
            zinder_testkit::method("getbestblockheightandhash")
                .reply(zinder_testkit::RpcReply::result(source_tip_after)),
        ])?;
        let json_rpc = ZebraJsonRpcSource::new(
            zinder_core::Network::ZcashRegtest,
            server.url(),
            crate::NodeAuth::None,
            Duration::from_secs(5),
        )?;
        let known_transaction_ids = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, mut event_receiver) = mpsc::channel(1);
        let poll_task = tokio::spawn(async move {
            poll_once(
                &json_rpc,
                &known_transaction_ids,
                UnixTimestampMillis::new(1_750_000_000_000),
                &event_sender,
                None,
            )
            .await
        });

        assert!(matches!(
            event_receiver.recv().await,
            Some(Ok(MempoolSourceEvent::Added(_)))
        ));
        let outcome = poll_task.await?;

        assert!(matches!(
            outcome,
            Err(PollFailure::Source(
                SourceError::MempoolStreamUnavailable { .. }
            ))
        ));
        assert!(event_receiver.try_recv().is_err());
        Ok(())
    }
}
