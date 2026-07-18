//! Shared `[retention]` config section.
//!
//! Owns the chain-event and mempool-event retention windows enforced by
//! the writer (`zinder-ingest`)
//! through `ServerInfo`. Before this section existed, the writer and
//! readers each carried their own copy of the same windows; operators
//! were responsible for keeping them in sync. Both planes now read one
//! `[retention]` section so the writer's enforcement and the reader's
//! advertisement cannot drift.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ConfigError;

const DEFAULT_CHAIN_EVENT_RETENTION_HOURS: u64 = 168;
const DEFAULT_CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS: u64 = 60_000;
const DEFAULT_CURSOR_AT_RISK_WARNING_HOURS: u64 = 24;
const DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES: u64 = 60;
const DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS: u64 = 24;
const DEFAULT_MEMPOOL_EVENT_RETENTION_CHECK_INTERVAL_MS: u64 = 30_000;
// Fires at 20% of the shortest mempool retention window
// (60 minutes × 20% = 12 minutes against DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES).
const DEFAULT_MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES: u64 = 12;

/// Raw `[retention]` config section.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionSection {
    /// Chain-event retention window in hours. `0` disables eviction.
    pub chain_event_retention_hours: Option<u64>,
    /// Chain-event retention sweep cadence in milliseconds. Must be > 0.
    pub chain_event_retention_check_interval_ms: Option<u64>,
    /// Cursor-at-risk warning lead time in hours.
    pub cursor_at_risk_warning_hours: Option<u64>,
    /// Mined-mempool retention window in minutes. `0` disables retention.
    pub mempool_mined_retention_minutes: Option<u64>,
    /// Invalidated-mempool retention window in hours. `0` disables retention.
    pub mempool_invalidated_retention_hours: Option<u64>,
    /// Mempool-event retention sweep cadence in milliseconds. Must be > 0.
    pub mempool_event_retention_check_interval_ms: Option<u64>,
    /// Mempool cursor-at-risk warning lead time in minutes.
    pub mempool_cursor_at_risk_warning_minutes: Option<u64>,
}

/// Fully resolved retention configuration with all defaults applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedRetention {
    /// Chain-event retention window in hours.
    pub chain_event_retention_hours: u64,
    /// Chain-event retention sweep cadence in milliseconds.
    pub chain_event_retention_check_interval_ms: u64,
    /// Cursor-at-risk warning lead time in hours.
    pub cursor_at_risk_warning_hours: u64,
    /// Mined-mempool retention window in minutes.
    pub mempool_mined_retention_minutes: u64,
    /// Invalidated-mempool retention window in hours.
    pub mempool_invalidated_retention_hours: u64,
    /// Mempool-event retention sweep cadence in milliseconds.
    pub mempool_event_retention_check_interval_ms: u64,
    /// Mempool cursor-at-risk warning lead time in minutes.
    pub mempool_cursor_at_risk_warning_minutes: u64,
}

impl ResolvedRetention {
    /// Chain-event retention window as a [`Duration`], or `None` when
    /// eviction is disabled (configured value is zero).
    #[must_use]
    pub fn chain_event_window(&self) -> Option<Duration> {
        (self.chain_event_retention_hours > 0)
            .then(|| Duration::from_hours(self.chain_event_retention_hours))
    }

    /// Chain-event retention sweep cadence.
    #[must_use]
    pub fn chain_event_check_interval(&self) -> Duration {
        Duration::from_millis(self.chain_event_retention_check_interval_ms)
    }

    /// Cursor-at-risk warning lead time for chain events.
    #[must_use]
    pub fn cursor_at_risk_warning(&self) -> Duration {
        Duration::from_hours(self.cursor_at_risk_warning_hours)
    }

    /// Mined-mempool retention window, or `None` when retention is
    /// disabled.
    #[must_use]
    pub fn mempool_mined_window(&self) -> Option<Duration> {
        (self.mempool_mined_retention_minutes > 0)
            .then(|| Duration::from_mins(self.mempool_mined_retention_minutes))
    }

    /// Invalidated-mempool retention window, or `None` when retention is
    /// disabled.
    #[must_use]
    pub fn mempool_invalidated_window(&self) -> Option<Duration> {
        (self.mempool_invalidated_retention_hours > 0)
            .then(|| Duration::from_hours(self.mempool_invalidated_retention_hours))
    }

    /// Mempool-event retention sweep cadence.
    #[must_use]
    pub fn mempool_check_interval(&self) -> Duration {
        Duration::from_millis(self.mempool_event_retention_check_interval_ms)
    }

    /// Cursor-at-risk warning lead time for mempool events.
    #[must_use]
    pub fn mempool_cursor_at_risk_warning(&self) -> Duration {
        Duration::from_mins(self.mempool_cursor_at_risk_warning_minutes)
    }
}

/// Redacted TOML projection of the `[retention]` section.
#[derive(Debug, Serialize)]
pub struct RetentionToml {
    /// Chain-event retention window in hours.
    pub chain_event_retention_hours: u64,
    /// Chain-event retention sweep cadence in milliseconds.
    pub chain_event_retention_check_interval_ms: u64,
    /// Cursor-at-risk warning lead time in hours.
    pub cursor_at_risk_warning_hours: u64,
    /// Mined-mempool retention window in minutes.
    pub mempool_mined_retention_minutes: u64,
    /// Invalidated-mempool retention window in hours.
    pub mempool_invalidated_retention_hours: u64,
    /// Mempool-event retention sweep cadence in milliseconds.
    pub mempool_event_retention_check_interval_ms: u64,
    /// Mempool cursor-at-risk warning lead time in minutes.
    pub mempool_cursor_at_risk_warning_minutes: u64,
}

impl RetentionToml {
    /// Builds a [`RetentionToml`] from a [`ResolvedRetention`].
    #[must_use]
    pub const fn from_resolved(retention: ResolvedRetention) -> Self {
        Self {
            chain_event_retention_hours: retention.chain_event_retention_hours,
            chain_event_retention_check_interval_ms: retention
                .chain_event_retention_check_interval_ms,
            cursor_at_risk_warning_hours: retention.cursor_at_risk_warning_hours,
            mempool_mined_retention_minutes: retention.mempool_mined_retention_minutes,
            mempool_invalidated_retention_hours: retention.mempool_invalidated_retention_hours,
            mempool_event_retention_check_interval_ms: retention
                .mempool_event_retention_check_interval_ms,
            mempool_cursor_at_risk_warning_minutes: retention
                .mempool_cursor_at_risk_warning_minutes,
        }
    }
}

/// Validates and resolves a [`RetentionSection`], applying per-field
/// defaults and cross-field invariants.
///
/// Returns [`ConfigError::Invalid`] when a sweep cadence is zero or when
/// a cursor-at-risk warning lead time exceeds the matching retention
/// window.
pub fn resolve_retention(section: RetentionSection) -> Result<ResolvedRetention, ConfigError> {
    let resolved = ResolvedRetention {
        chain_event_retention_hours: section
            .chain_event_retention_hours
            .unwrap_or(DEFAULT_CHAIN_EVENT_RETENTION_HOURS),
        chain_event_retention_check_interval_ms: section
            .chain_event_retention_check_interval_ms
            .unwrap_or(DEFAULT_CHAIN_EVENT_RETENTION_CHECK_INTERVAL_MS),
        cursor_at_risk_warning_hours: section
            .cursor_at_risk_warning_hours
            .unwrap_or(DEFAULT_CURSOR_AT_RISK_WARNING_HOURS),
        mempool_mined_retention_minutes: section
            .mempool_mined_retention_minutes
            .unwrap_or(DEFAULT_MEMPOOL_MINED_RETENTION_MINUTES),
        mempool_invalidated_retention_hours: section
            .mempool_invalidated_retention_hours
            .unwrap_or(DEFAULT_MEMPOOL_INVALIDATED_RETENTION_HOURS),
        mempool_event_retention_check_interval_ms: section
            .mempool_event_retention_check_interval_ms
            .unwrap_or(DEFAULT_MEMPOOL_EVENT_RETENTION_CHECK_INTERVAL_MS),
        mempool_cursor_at_risk_warning_minutes: section
            .mempool_cursor_at_risk_warning_minutes
            .unwrap_or(DEFAULT_MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES),
    };

    if resolved.chain_event_retention_check_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "retention.chain_event_retention_check_interval_ms must be greater than zero",
        ));
    }
    if resolved.mempool_event_retention_check_interval_ms == 0 {
        return Err(ConfigError::invalid(
            "retention.mempool_event_retention_check_interval_ms must be greater than zero",
        ));
    }
    if resolved.chain_event_retention_hours > 0
        && resolved.cursor_at_risk_warning_hours > resolved.chain_event_retention_hours
    {
        return Err(ConfigError::invalid(
            "retention.cursor_at_risk_warning_hours must be less than or equal to \
             retention.chain_event_retention_hours",
        ));
    }
    if let Some(shortest_minutes) = shortest_mempool_window_minutes(
        resolved.mempool_mined_retention_minutes,
        resolved.mempool_invalidated_retention_hours,
    ) && resolved.mempool_cursor_at_risk_warning_minutes > shortest_minutes
    {
        return Err(ConfigError::invalid(
            "retention.mempool_cursor_at_risk_warning_minutes must be less than or equal to \
             the shortest configured mempool retention window",
        ));
    }

    Ok(resolved)
}

fn shortest_mempool_window_minutes(mined_minutes: u64, invalidated_hours: u64) -> Option<u64> {
    let mined = (mined_minutes > 0).then_some(mined_minutes);
    let invalidated = (invalidated_hours > 0).then_some(invalidated_hours.saturating_mul(60));
    match (mined, invalidated) {
        (Some(mined), Some(invalidated)) => Some(mined.min(invalidated)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve_cleanly() -> Result<(), ConfigError> {
        let resolved = resolve_retention(RetentionSection::default())?;
        assert_eq!(
            resolved.chain_event_retention_hours,
            DEFAULT_CHAIN_EVENT_RETENTION_HOURS
        );
        assert_eq!(
            resolved.mempool_cursor_at_risk_warning_minutes,
            DEFAULT_MEMPOOL_CURSOR_AT_RISK_WARNING_MINUTES
        );
        Ok(())
    }

    #[test]
    fn zero_chain_event_check_interval_is_rejected() {
        let outcome = resolve_retention(RetentionSection {
            chain_event_retention_check_interval_ms: Some(0),
            ..RetentionSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn warning_exceeding_chain_event_window_is_rejected() {
        let outcome = resolve_retention(RetentionSection {
            chain_event_retention_hours: Some(10),
            cursor_at_risk_warning_hours: Some(20),
            ..RetentionSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn warning_exceeding_mempool_window_is_rejected() {
        let outcome = resolve_retention(RetentionSection {
            mempool_mined_retention_minutes: Some(10),
            mempool_cursor_at_risk_warning_minutes: Some(20),
            ..RetentionSection::default()
        });
        assert!(matches!(outcome, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn zero_retention_windows_disable_warning_check() -> Result<(), ConfigError> {
        let resolved = resolve_retention(RetentionSection {
            mempool_mined_retention_minutes: Some(0),
            mempool_invalidated_retention_hours: Some(0),
            mempool_cursor_at_risk_warning_minutes: Some(99_999),
            ..RetentionSection::default()
        })?;
        assert_eq!(resolved.mempool_cursor_at_risk_warning_minutes, 99_999);
        Ok(())
    }
}
