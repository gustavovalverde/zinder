//! Live federation tests for the derive-plane balance surface.
//!
//! Submodule per the workspace test-tier convention ([ADR-0006]). Each test
//! is double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and a runtime
//! `zinder_testkit::live::require_live*` call, and reads the unified
//! `ZINDER_NETWORK` + `ZINDER_NODE__*` env-var schema rather than inventing a
//! parallel test-only namespace.
//!
//! [ADR-0006]: ../../../docs/adrs/0006-test-tiers-and-live-config.md

mod balance_federation;
