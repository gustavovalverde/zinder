//! Structural assertions on [`StartupPhase`] event emission.
//!
//! These tests pair the structured phase API with [`LogCapture`] so the
//! "entry plus outcome" contract every service binary depends on does not
//! silently regress. They live in `zinder-runtime`'s integration tier
//! (not under `tests/live/`) because they exercise the in-process tracing
//! subscriber rather than a live node.

#![allow(
    missing_docs,
    reason = "Integration test names describe the phase-emission contract under test."
)]

use tracing::Level;
use zinder_runtime::StartupPhase;
use zinder_testkit::LogCapture;

#[test]
fn complete_emits_entry_then_ok_exit() {
    let capture = LogCapture::install_for_target("zinder::startup");
    let phase = StartupPhase::OpenStorage.start();
    phase.complete();

    let events = capture.events();
    assert_eq!(
        events.len(),
        2,
        "complete must emit exactly one entry and one exit event",
    );
    assert_eq!(events[0].field("phase"), Some("open_storage"));
    assert_eq!(events[0].field("phase_state"), Some("entry"));
    assert_eq!(events[1].field("phase"), Some("open_storage"));
    assert_eq!(events[1].field("phase_state"), Some("exit"));
    assert_eq!(events[1].field("outcome"), Some("ok"));
    assert_eq!(events[0].level, Level::INFO);
    assert_eq!(events[1].level, Level::INFO);
    assert!(
        events[1].field("elapsed_ms").is_some(),
        "exit event must carry elapsed_ms",
    );
}

#[test]
fn fail_emits_failed_exit_with_reason() {
    let capture = LogCapture::install_for_target("zinder::startup");
    let phase = StartupPhase::ConnectNode.start();
    phase.fail(&"upstream node refused connection");

    let events = capture.events();
    assert_eq!(events.len(), 2);
    let exit = &events[1];
    assert_eq!(exit.field("phase_state"), Some("exit"));
    assert_eq!(exit.field("outcome"), Some("failed"));
    assert_eq!(
        exit.field("reason"),
        Some("upstream node refused connection"),
    );
}

#[test]
fn drop_without_close_emits_aborted_outcome() {
    let capture = LogCapture::install_for_target("zinder::startup");
    {
        let _phase = StartupPhase::CheckSchema.start();
        // No complete() and no fail(): the guard is dropped at scope exit.
    }

    let events = capture.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].field("phase_state"), Some("exit"));
    assert_eq!(events[1].field("outcome"), Some("aborted"));
    assert!(
        events[1].field("reason").is_none(),
        "aborted exits carry no reason field",
    );
}

#[test]
fn ordered_phases_keep_their_individual_entry_exit_pairs() {
    let capture = LogCapture::install_for_target("zinder::startup");
    StartupPhase::LoadConfig.start().complete();
    StartupPhase::OpenStorage.start().complete();
    StartupPhase::ConnectNode.start().complete();
    StartupPhase::Ready.start().complete();

    let events = capture.events();
    let phase_sequence: Vec<&str> = events
        .iter()
        .filter_map(|event| event.field("phase"))
        .collect();
    assert_eq!(
        phase_sequence,
        vec![
            "load_config",
            "load_config",
            "open_storage",
            "open_storage",
            "connect_node",
            "connect_node",
            "ready",
            "ready",
        ],
        "each phase emits one entry then one exit, in the order it was started",
    );
}
