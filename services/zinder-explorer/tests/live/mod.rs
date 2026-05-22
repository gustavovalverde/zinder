//! Live federation tests for the derive-plane balance surface.
//!
//! Each test is double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and a
//! runtime `zinder_testkit::live::require_live*` call, and reads the unified
//! `ZINDER_NETWORK` + `ZINDER_NODE__*` env-var schema rather than inventing a
//! parallel test-only namespace.

mod balance_federation;
mod fee_summary_federation;
mod mempool_federation;
mod search_federation;
mod transaction_detail_federation;
mod value_pool_summary_federation;
