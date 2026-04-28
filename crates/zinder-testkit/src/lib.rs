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
//! [`PrimaryChainStore`]: zinder_store::PrimaryChainStore

pub mod chain_fixture;
pub mod live;
pub mod mock_node_source;
pub mod mock_transaction_broadcaster;
pub mod store_fixture;

pub use chain_fixture::{ChainFixture, FixtureBlock};
pub use mock_node_source::{MockNodeSource, NodeFailureScript};
pub use mock_transaction_broadcaster::MockTransactionBroadcaster;
pub use store_fixture::StoreFixture;
