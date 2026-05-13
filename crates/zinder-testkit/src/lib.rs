//! Deterministic chain, store, and node fixtures for Zinder tests.
//!
//! `zinder-testkit` is the single source of truth for synthetic chain shapes,
//! tempdir-backed [`PrimaryChainStore`] instances, and trait fakes used across the
//! workspace. Tests should reach for these helpers instead of duplicating
//! `synthetic_store()`, `FakeBroadcaster`, or `TestNodeSource` patterns.
//!
//! # Vocabulary
//!
//! - [`ChainFixture`] is a deterministic in-memory chain. It exposes block,
//!   compact-block, source-block, tree-state, and chain-epoch values keyed by
//!   height, plus fork helpers for reorg shape construction.
//! - [`StoreFixture`] is a tempdir-backed [`PrimaryChainStore`] with a builder that
//!   commits a [`ChainFixture`] before handing the store to tests.
//! - [`MockNodeSource`] implements [`zinder_source::NodeSource`]
//!   against a [`ChainFixture`]. It supports tip mutation, error injection,
//!   and configurable capabilities.
//! - [`MockTransactionBroadcaster`] implements
//!   [`zinder_source::TransactionBroadcaster`] with configurable per-call
//!   outcomes and a recording mode that captures calls for later inspection.
//!
//! # DX
//!
//! The crate is `publish = false` and exists purely to give tests a shared
//! vocabulary. Each helper has a small hand-rolled builder API; importing it
//! should never require knowing the layout of any internal struct field.
//!
//! # Boundary contract
//!
//! Every consumer must list `zinder-testkit` under `[dev-dependencies]` only.
//! Items in this crate (notably [`TransparentTestKey`], which uses
//! `zcash_primitives::transaction::builder::Builder::mock_build`) are
//! unsuitable for production signing or production transaction handling, and
//! the dev-dep-only rule is what keeps them out of release binaries. The CI
//! workspace gate (`cargo deny check`) enforces no production crate links
//! against this one.
//!
//! [`PrimaryChainStore`]: zinder_store::PrimaryChainStore

pub mod chain_fixture;
pub mod deploy;
pub mod live;
pub mod log_capture;
pub mod mock_mempool_source;
pub mod mock_node_source;
pub mod mock_transaction_broadcaster;
pub mod network_upgrade_fixtures;
pub mod store_fixture;
pub mod transparent_signer;

pub use chain_fixture::{ChainFixture, FixtureBlock};
pub use log_capture::{CapturedEvent, LogCapture};
pub use mock_mempool_source::{
    MockMempoolSource, MockMempoolSourceClosed, MockMempoolSourceControl,
};
pub use mock_node_source::{MockNodeSource, NodeFailureScript};
pub use mock_transaction_broadcaster::MockTransactionBroadcaster;
pub use network_upgrade_fixtures::{
    local_network_from_activations, sample_regtest_upgrade_activations,
};
pub use store_fixture::StoreFixture;
pub use transparent_signer::{
    LocalNetwork, P2pkhSpendArgs, TransparentAddress, TransparentSignerError, TransparentTestKey,
    ZIP317_FEE_ONE_IN_ONE_OUT_ZATS, regtest_local_network,
};
