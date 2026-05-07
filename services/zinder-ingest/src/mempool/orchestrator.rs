//! Mempool orchestration: drives source events into the live index and the
//! canonical mempool-event store.
//!
//! Consumes a [`MempoolSource`] event stream and:
//!
//! - Hydrates [`MempoolSourceEntry`] values into [`MempoolEntry`] records
//!   stamped with the current visible [`ChainEpoch`].
//! - Applies the resulting [`MempoolEvent`] to the live [`MempoolIndex`]
//!   and persists it via [`PrimaryChainStore::append_mempool_event`].
//! - Emits readiness signals on hydration failures and source closures.
//!
//! The orchestrator owns no state beyond its handles; it is safe to drop
//! and re-create when the writer reconfigures the source.

use std::sync::Arc;

use tokio_stream::StreamExt;
use zinder_core::{ChainEpoch, UnixTimestampMillis};
use zinder_source::{
    MempoolHydrationFailureReason, MempoolSource, MempoolSourceEvent, SourceError,
};
use zinder_store::{MempoolEvent, PrimaryChainStore, StoreError};

use super::entry::{MempoolEntryBuildError, build_mempool_entry};
use super::index::{MempoolApplyOutcome, MempoolIndex};

/// Outcome metadata reported per applied source event.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MempoolOrchestratorEventOutcome {
    /// `source.events()` resolved successfully; the orchestrator is now
    /// draining the stream. Emitted exactly once per `run_mempool_orchestrator`
    /// invocation, before any per-event outcome.
    SourceStreamOpened,
    /// Event was applied to both the live index and the event log.
    Applied,
    /// Source observation was a no-op (duplicate Added or unknown txid).
    NoChange,
    /// Source delivered an error item; orchestrator continued listening.
    SourceErrorObserved,
    /// Hydrating an Added event failed; the txid was skipped.
    HydrationFailed,
}

/// Errors that terminate the orchestrator loop.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MempoolOrchestratorError {
    /// Opening the source event stream failed.
    #[error("mempool source event stream open failed")]
    SourceOpenFailed {
        /// Underlying source failure.
        #[source]
        source: SourceError,
    },
    /// Persisting an envelope to the canonical store failed.
    #[error("mempool event log append failed")]
    EventLogAppendFailed {
        /// Underlying store failure.
        #[source]
        source: StoreError,
    },
    /// The chain store returned an error while reading the visible chain
    /// epoch for hydration.
    #[error("mempool source observation could not stamp chain epoch")]
    ChainEpochUnavailable {
        /// Underlying store failure.
        #[source]
        source: StoreError,
    },
}

/// Drives a [`MempoolSource`] event stream into the live mempool surface.
///
/// Returns when the source stream closes naturally or when a fatal error
/// is observed. Per-event hydration failures and source-error items
/// produced by the stream are non-fatal: the orchestrator records them as
/// outcomes via the optional callback.
pub async fn run_mempool_orchestrator(
    source: Arc<dyn MempoolSource>,
    chain_store: PrimaryChainStore,
    mempool_index: MempoolIndex,
    mut on_event_outcome: impl FnMut(MempoolOrchestratorEventOutcome),
) -> Result<(), MempoolOrchestratorError> {
    let mut event_stream = source
        .events()
        .await
        .map_err(|source| MempoolOrchestratorError::SourceOpenFailed { source })?;
    on_event_outcome(MempoolOrchestratorEventOutcome::SourceStreamOpened);

    while let Some(event_result) = event_stream.next().await {
        let outcome = match event_result {
            Ok(source_event) => {
                let visible_chain_epoch = chain_store
                    .current_chain_epoch()
                    .map_err(|source| MempoolOrchestratorError::ChainEpochUnavailable { source })?;
                commit_source_event(
                    source_event,
                    visible_chain_epoch,
                    &mempool_index,
                    &chain_store,
                )?
            }
            Err(_source_error) => {
                metrics::counter!(
                    "zinder_mempool_source_errors_total",
                    "kind" => "stream_item"
                )
                .increment(1);
                MempoolOrchestratorEventOutcome::SourceErrorObserved
            }
        };
        on_event_outcome(outcome);
    }
    Ok(())
}

fn commit_source_event(
    source_event: MempoolSourceEvent,
    visible_chain_epoch: Option<ChainEpoch>,
    mempool_index: &MempoolIndex,
    chain_store: &PrimaryChainStore,
) -> Result<MempoolOrchestratorEventOutcome, MempoolOrchestratorError> {
    let canonical_event = match canonical_event_from_source(source_event, visible_chain_epoch) {
        Ok(canonical_event) => canonical_event,
        Err(reason) => {
            record_hydration_failure(reason);
            return Ok(MempoolOrchestratorEventOutcome::HydrationFailed);
        }
    };

    // Per ADR-0010 §Implementation, every typed envelope must pass through
    // the durable event log before consumers can observe it through the
    // live index. If we mutated the index first and the append failed, a
    // reader could see an entry in the live mempool that no
    // `MempoolEvents` cursor will ever replay (or, for terminal events, see
    // the entry vanish without a `Mined`/`Invalidated` resolution).
    //
    // The orchestrator is single-tasked so the no-op predicate observes
    // the same index state that the subsequent apply will mutate; there is
    // no concurrent writer that can race the two reads.
    if would_be_noop(mempool_index, &canonical_event) {
        return Ok(MempoolOrchestratorEventOutcome::NoChange);
    }

    let _envelope = chain_store
        .append_mempool_event(canonical_event.clone(), UnixTimestampMillis::now())
        .map_err(|source| MempoolOrchestratorError::EventLogAppendFailed { source })?;
    let _apply_outcome = apply_to_index(mempool_index, canonical_event);
    record_mempool_size_gauges(mempool_index, chain_store);
    Ok(MempoolOrchestratorEventOutcome::Applied)
}

#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is #[non_exhaustive]; future variants are treated as no-ops until the orchestrator learns how to apply them."
)]
fn would_be_noop(mempool_index: &MempoolIndex, event: &MempoolEvent) -> bool {
    match event {
        MempoolEvent::Added { entry } => mempool_index.is_in_mempool(entry.transaction_id),
        MempoolEvent::Invalidated { transaction_id, .. }
        | MempoolEvent::Mined { transaction_id, .. } => {
            !mempool_index.is_in_mempool(*transaction_id)
        }
        _ => true,
    }
}

#[allow(
    unreachable_patterns,
    reason = "MempoolSourceEvent is #[non_exhaustive]; the orchestrator fails closed for future variants until it learns how to handle them."
)]
fn canonical_event_from_source(
    source_event: MempoolSourceEvent,
    visible_chain_epoch: Option<ChainEpoch>,
) -> Result<MempoolEvent, MempoolHydrationFailureReason> {
    match source_event {
        MempoolSourceEvent::Added(source_entry) => {
            let visible_chain_epoch =
                visible_chain_epoch.ok_or(MempoolHydrationFailureReason::NoVisibleChainEpoch)?;
            build_mempool_entry(source_entry, visible_chain_epoch)
                .map(|entry| MempoolEvent::Added { entry })
                .map_err(|build_error| match build_error {
                    MempoolEntryBuildError::TransactionParseFailed { .. } => {
                        MempoolHydrationFailureReason::TransactionParseFailed
                    }
                    MempoolEntryBuildError::CompactTransactionBuildFailed { .. } => {
                        MempoolHydrationFailureReason::CompactTransactionBuildFailed
                    }
                    MempoolEntryBuildError::TransparentOutputIndexOverflow => {
                        MempoolHydrationFailureReason::TransparentOutputIndexOverflow
                    }
                })
        }
        MempoolSourceEvent::Invalidated {
            transaction_id,
            reason,
        } => Ok(MempoolEvent::Invalidated {
            transaction_id,
            reason,
        }),
        MempoolSourceEvent::Mined {
            transaction_id,
            mined_height,
        } => Ok(MempoolEvent::Mined {
            transaction_id,
            mined_height,
        }),
        _ => Err(MempoolHydrationFailureReason::UnknownSourceEventVariant),
    }
}

#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is #[non_exhaustive]; future variants are observed as no-ops until we extend the index."
)]
fn apply_to_index(mempool_index: &MempoolIndex, event: MempoolEvent) -> MempoolApplyOutcome {
    match event {
        MempoolEvent::Added { entry } => mempool_index.apply_added(entry),
        MempoolEvent::Invalidated { transaction_id, .. } => {
            mempool_index.apply_invalidated(transaction_id)
        }
        MempoolEvent::Mined { transaction_id, .. } => mempool_index.apply_mined(transaction_id),
        _ => MempoolApplyOutcome::NoChange,
    }
}

fn record_hydration_failure(reason: MempoolHydrationFailureReason) {
    metrics::counter!(
        "zinder_mempool_hydration_failures_total",
        "reason" => reason.as_label()
    )
    .increment(1);
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges accept f64 samples; mempool counts are bounded diagnostic data"
)]
fn record_mempool_size_gauges(mempool_index: &MempoolIndex, chain_store: &PrimaryChainStore) {
    let entry_count = u32::try_from(mempool_index.entry_count()).unwrap_or(u32::MAX);
    metrics::gauge!("zinder_mempool_entries").set(f64::from(entry_count));
    if let Ok(retention_report) = chain_store.mempool_event_retention_report() {
        metrics::gauge!("zinder_mempool_events_retained")
            .set(retention_report.retained_event_count as f64);
    }
}
