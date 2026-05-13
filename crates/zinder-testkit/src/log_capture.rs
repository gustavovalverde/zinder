//! In-process [`tracing`] event capture for assertion-based tests.
//!
//! [`LogCapture`] installs a process-local [`tracing_subscriber::Layer`] that
//! buffers every emitted event into a `Mutex<Vec<CapturedEvent>>`. Tests can
//! then assert on emitted phases, retention events, or capability probes
//! without comparing against unstructured `stderr` text.
//!
//! Designed for unit and integration tests that exercise startup or
//! background-task code paths where the tracing event is the only
//! externally-observable signal.
//!
//! # Usage
//!
//! ```ignore
//! use zinder_testkit::log_capture::LogCapture;
//!
//! #[tokio::test]
//! async fn check_schema_phase_emits_entry_and_exit_events() {
//!     let capture = LogCapture::install_for_target("zinder::startup");
//!     run_startup().await;
//!     let events = capture.events();
//!     assert!(
//!         events.iter().any(|event| event.field("phase").is_some_and(|value| value == "check_schema")),
//!         "no check_schema event was captured",
//!     );
//! }
//! ```
//!
//! Each [`LogCapture`] sets a process-local default subscriber for the
//! current thread via [`tracing::subscriber::set_default`]. The returned
//! drop guard restores the previous subscriber, so concurrent tests must
//! hold their own [`LogCapture`] for the duration of the captured region.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

/// One captured tracing event.
#[derive(Clone, Debug)]
pub struct CapturedEvent {
    /// `target=` field of the originating event (e.g. `"zinder::startup"`).
    pub target: String,
    /// Severity of the event.
    pub level: Level,
    /// Captured field values keyed by field name. The `message` field carries
    /// the formatted log message.
    pub fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    /// Returns the value of `field_name` when present, otherwise `None`.
    #[must_use]
    pub fn field(&self, field_name: &str) -> Option<&str> {
        self.fields.get(field_name).map(String::as_str)
    }

    /// Returns the formatted `message` field, or an empty string when no
    /// message was emitted.
    #[must_use]
    pub fn message(&self) -> &str {
        self.fields.get("message").map_or("", String::as_str)
    }
}

/// Captures tracing events for an in-process test region.
///
/// The capture is scoped to the current thread; drop the value to restore
/// the previously-installed subscriber.
#[must_use = "drop the LogCapture only when the capture region ends"]
pub struct LogCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    target_filter: Option<String>,
    _subscriber_guard: DefaultGuard,
}

impl LogCapture {
    /// Installs a capture-only subscriber for the current thread that buffers
    /// every event regardless of its `target`.
    pub fn install() -> Self {
        Self::install_inner(None)
    }

    /// Installs a capture-only subscriber for the current thread that retains
    /// only events whose `target` matches `target` exactly.
    pub fn install_for_target(target: impl Into<String>) -> Self {
        Self::install_inner(Some(target.into()))
    }

    fn install_inner(target_filter: Option<String>) -> Self {
        let events: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            events: Arc::clone(&events),
            target_filter: target_filter.clone(),
        };
        let subscriber = Registry::default().with(layer);
        let guard = tracing::subscriber::set_default(subscriber);
        Self {
            events,
            target_filter,
            _subscriber_guard: guard,
        }
    }

    /// Returns a clone of every captured event in arrival order.
    #[must_use]
    pub fn events(&self) -> Vec<CapturedEvent> {
        match self.events.lock() {
            Ok(events) => events.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns `true` when at least one captured event matches `predicate`.
    #[must_use]
    pub fn any(&self, predicate: impl Fn(&CapturedEvent) -> bool) -> bool {
        self.events().iter().any(predicate)
    }

    /// Returns the active target filter, when one was supplied.
    #[must_use]
    pub fn target_filter(&self) -> Option<&str> {
        self.target_filter.as_deref()
    }
}

struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    target_filter: Option<String>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata: &Metadata<'_> = event.metadata();
        if let Some(target_filter) = self.target_filter.as_deref()
            && metadata.target() != target_filter
        {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            target: metadata.target().to_owned(),
            level: *metadata.level(),
            fields: visitor.fields,
        };
        let mut events_guard = match self.events.lock() {
            Ok(events) => events,
            Err(poisoned) => poisoned.into_inner(),
        };
        events_guard.push(captured);
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, captured_value: &dyn fmt::Debug) {
        let mut buffer = String::new();
        let _ = write!(buffer, "{captured_value:?}");
        self.fields.insert(field.name().to_owned(), buffer);
    }

    fn record_str(&mut self, field: &Field, captured_value: &str) {
        self.fields
            .insert(field.name().to_owned(), captured_value.to_owned());
    }

    fn record_i64(&mut self, field: &Field, captured_value: i64) {
        self.fields
            .insert(field.name().to_owned(), captured_value.to_string());
    }

    fn record_u64(&mut self, field: &Field, captured_value: u64) {
        self.fields
            .insert(field.name().to_owned(), captured_value.to_string());
    }

    fn record_bool(&mut self, field: &Field, captured_value: bool) {
        self.fields
            .insert(field.name().to_owned(), captured_value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::{info, warn};

    #[test]
    fn capture_records_message_target_and_level() {
        let capture = LogCapture::install();
        info!(target: "zinder::test", code = 7, "hello world");
        let events = capture.events();
        assert_eq!(events.len(), 1);
        let captured = &events[0];
        assert_eq!(captured.target, "zinder::test");
        assert_eq!(captured.level, Level::INFO);
        assert_eq!(captured.message(), "hello world");
        assert_eq!(captured.field("code"), Some("7"));
    }

    #[test]
    fn capture_target_filter_drops_unmatched_targets() {
        let capture = LogCapture::install_for_target("zinder::startup");
        info!(target: "zinder::startup", phase = "load_config", "entry");
        info!(target: "zinder::other", "should be ignored");
        let events = capture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].target, "zinder::startup");
    }

    #[test]
    fn capture_records_warn_level() {
        let capture = LogCapture::install();
        warn!(target: "zinder::test", "the sky is falling");
        let events = capture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::WARN);
    }

    #[test]
    fn capture_any_matches_a_field() {
        let capture = LogCapture::install();
        info!(target: "zinder::startup", phase = "open_storage", phase_state = "entry", "begin");
        info!(target: "zinder::startup", phase = "open_storage", phase_state = "exit", outcome = "ok", elapsed_ms = 12_u64, "done");
        assert!(capture.any(|event| {
            event.target == "zinder::startup"
                && event.field("phase") == Some("open_storage")
                && event.field("phase_state") == Some("exit")
        }));
    }
}
