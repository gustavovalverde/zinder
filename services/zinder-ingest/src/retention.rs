//! Chain-event retention task owned by the ingest writer.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zinder_core::UnixTimestampMillis;
use zinder_runtime::{Readiness, ReadinessCause, ReadinessState};
use zinder_store::{ChainEventRetentionReport, PrimaryChainStore, StoreError};

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
        .map(|chain_epoch| chain_epoch.tip_height.value());
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

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus gauges use f64 samples; chain-event retention values are diagnostic"
)]
fn u64_to_f64(sample: u64) -> f64 {
    sample as f64
}
