//! Chain-event and mempool-event retention tasks owned by the ingest writer.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::UnixTimestampMillis;
use zinder_runtime::{Readiness, ReadinessCause, ReadinessState};
use zinder_store::{
    ChainEventRetentionReport, MempoolEventRetentionConfig, MempoolEventRetentionReport,
    PrimaryChainStore, StoreError,
};

use crate::ingest_loop::{HistoricalWorkGate, wait_until_historical_work_or_cancelled};

const TRANSPARENT_RETENTION_BACKLOG_YIELD: Duration = Duration::from_millis(250);
const TRANSPARENT_RETENTION_CAUGHT_UP_INTERVAL: Duration = Duration::from_secs(30);
const TRANSPARENT_RETENTION_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Runtime configuration for chain-event retention pruning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainEventRetentionConfig {
    /// Retention window for chain-event rows. `None` means unbounded retention.
    pub retention_window: Option<Duration>,
    /// Interval between retention checks.
    pub check_interval: Duration,
    /// Warning window before retention expiry.
    pub cursor_at_risk_warning: Duration,
}

/// Runtime configuration for mempool-event retention pruning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MempoolEventRetentionWorkerConfig {
    /// Retention windows applied per event variant.
    pub retention: MempoolEventRetentionConfig,
    /// Interval between retention checks.
    pub check_interval: Duration,
    /// Warning window before retention expiry.
    pub cursor_at_risk_warning: Duration,
}

/// Spawns bounded transparent-projection retention maintenance.
///
/// The historical-work gate keeps this task idle until canonical ingest and
/// derive replay are both caught up. Each blocking store pass has its own
/// height and outpoint limits, then yields before draining more backlog so a
/// newly-arrived canonical block can close the gate between passes.
#[must_use = "await the handle during shutdown"]
pub fn spawn_transparent_retention_task(
    store: PrimaryChainStore,
    historical_work_gate: HistoricalWorkGate,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if wait_until_historical_work_or_cancelled(&historical_work_gate, &cancel).await {
                return;
            }

            let store_for_pass = store.clone();
            let pass = tokio::task::spawn_blocking(move || {
                store_for_pass.sweep_transparent_retention_once()
            });
            let delay = match pass.await {
                Ok(Ok(outcome)) if outcome.backlog_heights() > 0 => {
                    TRANSPARENT_RETENTION_BACKLOG_YIELD
                }
                Ok(Ok(_)) => TRANSPARENT_RETENTION_CAUGHT_UP_INTERVAL,
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "transparent_retention_sweep_failed",
                        error = %error,
                        "transparent retention maintenance pass failed; retrying"
                    );
                    TRANSPARENT_RETENTION_RETRY_INTERVAL
                }
                Err(error) => {
                    tracing::warn!(
                        target: "zinder::ingest",
                        event = "transparent_retention_task_failed",
                        error = %error,
                        "transparent retention maintenance task stopped; retrying"
                    );
                    TRANSPARENT_RETENTION_RETRY_INTERVAL
                }
            };

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    })
}

/// Spawns the ingest-owned chain-event retention task.
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub fn spawn_chain_event_retention_task(
    store: PrimaryChainStore,
    readiness: Readiness,
    config: ChainEventRetentionConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(config.check_interval) => {
                    if let Err(error) = run_chain_event_retention_once(&store, &readiness, config) {
                        tracing::warn!(
                            target: "zinder::ingest",
                            event = "chain_event_retention_failed",
                            error = %error,
                            "chain-event retention pass failed"
                        );
                    }
                }
            }
        }
    })
}

/// Spawns the ingest-owned mempool-event retention task.
#[must_use = "drop the handle to detach the task or await it for symmetric shutdown"]
pub fn spawn_mempool_event_retention_task(
    store: PrimaryChainStore,
    readiness: Readiness,
    config: MempoolEventRetentionWorkerConfig,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(config.check_interval) => {
                    if let Err(error) = run_mempool_event_retention_once(&store, &readiness, config) {
                        tracing::warn!(
                            target: "zinder::ingest",
                            event = "mempool_event_retention_failed",
                            error = %error,
                            "mempool-event retention pass failed"
                        );
                    }
                }
            }
        }
    })
}

#[derive(Debug, Error)]
enum ChainEventRetentionError {
    #[error("system time is before unix epoch")]
    SystemTimeBeforeUnixEpoch,
    #[error("current unix timestamp does not fit in milliseconds")]
    TimestampTooLarge,
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn run_chain_event_retention_once(
    store: &PrimaryChainStore,
    readiness: &Readiness,
    config: ChainEventRetentionConfig,
) -> Result<(), ChainEventRetentionError> {
    let now = current_unix_millis()?;
    let report = match config.retention_window {
        Some(retention_window) => {
            let cutoff_created_at = retention_cutoff(now, retention_window);
            store.prune_chain_events_before(cutoff_created_at)?
        }
        None => store.chain_event_retention_report()?,
    };
    record_oldest_retained_age(now, report);
    update_retention_readiness(store, readiness, config, now, report)?;

    Ok(())
}

fn update_retention_readiness(
    store: &PrimaryChainStore,
    readiness: &Readiness,
    config: ChainEventRetentionConfig,
    now: UnixTimestampMillis,
    report: ChainEventRetentionReport,
) -> Result<(), StoreError> {
    let current_height = store
        .current_chain_epoch()?
        .map(|chain_epoch| chain_epoch.visible_tip_height.value());
    let is_at_risk = is_cursor_at_risk(config, now, report);
    let current_cause = readiness.report().cause;

    if is_at_risk
        && matches!(
            current_cause,
            ReadinessCause::Ready | ReadinessCause::CursorAtRisk { .. }
        )
    {
        let Some(retention_window) = config.retention_window else {
            return Ok(());
        };
        let oldest_retained_age_hours = report
            .oldest_retained_created_at
            .map_or(0, |created_at| age_hours(now, created_at));
        readiness.set(ReadinessState::cursor_at_risk(
            oldest_retained_age_hours,
            duration_hours(retention_window),
            current_height,
        ));
    } else if !is_at_risk && matches!(current_cause, ReadinessCause::CursorAtRisk { .. }) {
        readiness.set(ReadinessState::ready(current_height));
    }

    Ok(())
}

fn is_cursor_at_risk(
    config: ChainEventRetentionConfig,
    now: UnixTimestampMillis,
    report: ChainEventRetentionReport,
) -> bool {
    let Some(retention_window) = config.retention_window else {
        return false;
    };
    let Some(oldest_retained_created_at) = report.oldest_retained_created_at else {
        return false;
    };
    let warning_threshold = retention_window.saturating_sub(config.cursor_at_risk_warning);
    age_duration(now, oldest_retained_created_at) > warning_threshold
}

fn retention_cutoff(now: UnixTimestampMillis, retention_window: Duration) -> UnixTimestampMillis {
    let retention_millis = u64::try_from(retention_window.as_millis()).unwrap_or(u64::MAX);
    UnixTimestampMillis::new(now.value().saturating_sub(retention_millis))
}

fn record_oldest_retained_age(now: UnixTimestampMillis, report: ChainEventRetentionReport) {
    let oldest_age_seconds = report
        .oldest_retained_created_at
        .map_or(0, |created_at| age_duration(now, created_at).as_secs());
    metrics::gauge!("zinder_chain_event_retention_oldest_age_seconds")
        .set(u64_to_f64(oldest_age_seconds));
}

fn age_duration(now: UnixTimestampMillis, created_at: UnixTimestampMillis) -> Duration {
    Duration::from_millis(now.value().saturating_sub(created_at.value()))
}

fn age_hours(now: UnixTimestampMillis, created_at: UnixTimestampMillis) -> u64 {
    age_duration(now, created_at).as_secs() / 3_600
}

fn duration_hours(duration: Duration) -> u64 {
    duration.as_secs() / 3_600
}

fn current_unix_millis() -> Result<UnixTimestampMillis, ChainEventRetentionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ChainEventRetentionError::SystemTimeBeforeUnixEpoch)?;
    let millis = u64::try_from(duration.as_millis())
        .map_err(|_| ChainEventRetentionError::TimestampTooLarge)?;

    Ok(UnixTimestampMillis::new(millis))
}

fn run_mempool_event_retention_once(
    store: &PrimaryChainStore,
    readiness: &Readiness,
    config: MempoolEventRetentionWorkerConfig,
) -> Result<(), ChainEventRetentionError> {
    let now = current_unix_millis()?;
    let report = if config.retention.is_unbounded() {
        store.mempool_event_retention_report()?
    } else {
        store.prune_mempool_events_before(now, config.retention)?
    };
    record_mempool_oldest_retained_age(now, report);
    update_mempool_retention_readiness(store, readiness, config, now, report)?;

    Ok(())
}

fn update_mempool_retention_readiness(
    store: &PrimaryChainStore,
    readiness: &Readiness,
    config: MempoolEventRetentionWorkerConfig,
    now: UnixTimestampMillis,
    report: MempoolEventRetentionReport,
) -> Result<(), StoreError> {
    let current_height = store
        .current_chain_epoch()?
        .map(|chain_epoch| chain_epoch.visible_tip_height.value());
    let is_at_risk = is_mempool_cursor_at_risk(config, now, report);
    let current_cause = readiness.report().cause;

    if is_at_risk
        && matches!(
            current_cause,
            ReadinessCause::Ready | ReadinessCause::MempoolCursorAtRisk { .. }
        )
    {
        let Some(retention_window) = shortest_mempool_retention_window(config.retention) else {
            return Ok(());
        };
        let oldest_retained_age_minutes = report
            .oldest_retained_observed_at
            .map_or(0, |observed_at| age_minutes(now, observed_at));
        readiness.set(ReadinessState::mempool_cursor_at_risk(
            oldest_retained_age_minutes,
            duration_minutes(retention_window),
            current_height,
        ));
    } else if !is_at_risk && matches!(current_cause, ReadinessCause::MempoolCursorAtRisk { .. }) {
        readiness.set(ReadinessState::ready(current_height));
    }

    Ok(())
}

fn is_mempool_cursor_at_risk(
    config: MempoolEventRetentionWorkerConfig,
    now: UnixTimestampMillis,
    report: MempoolEventRetentionReport,
) -> bool {
    let Some(retention_window) = shortest_mempool_retention_window(config.retention) else {
        return false;
    };
    let Some(oldest_retained_observed_at) = report.oldest_retained_observed_at else {
        return false;
    };
    let warning_threshold = retention_window.saturating_sub(config.cursor_at_risk_warning);
    age_duration(now, oldest_retained_observed_at) > warning_threshold
}

fn shortest_mempool_retention_window(retention: MempoolEventRetentionConfig) -> Option<Duration> {
    let candidates = [
        retention.added_retention,
        retention.mined_retention,
        retention.invalidated_retention,
    ];
    candidates
        .into_iter()
        .flatten()
        .min_by_key(std::time::Duration::as_secs)
}

fn record_mempool_oldest_retained_age(
    now: UnixTimestampMillis,
    report: MempoolEventRetentionReport,
) {
    let oldest_age_seconds = report
        .oldest_retained_observed_at
        .map_or(0, |observed_at| age_duration(now, observed_at).as_secs());
    metrics::gauge!("zinder_mempool_event_retention_oldest_age_seconds")
        .set(u64_to_f64(oldest_age_seconds));
}

fn age_minutes(now: UnixTimestampMillis, observed_at: UnixTimestampMillis) -> u64 {
    age_duration(now, observed_at).as_secs() / 60
}

fn duration_minutes(duration: Duration) -> u64 {
    duration.as_secs() / 60
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; chain-event retention values are diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}
