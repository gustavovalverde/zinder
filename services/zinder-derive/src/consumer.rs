//! Derive-consumer trait and shared types.
//!
//! Every derive consumer (balance accumulator in M5, analytics views in M6+)
//! implements [`DeriveConsumer`]. The trait is the seam between the
//! consumer-agnostic infrastructure in `zinder-derive` (store, `ChainEvents`
//! subscriber, backfill helper, ops surface) and the consumer-specific
//! aggregation logic that lives in each consumer module.

use async_trait::async_trait;

/// Stable name of a derive consumer used to scope cursor and metadata rows.
///
/// The name is part of the on-disk key prefix in the `cursor` column family;
/// renaming a consumer between releases is a schema migration, not a config
/// change. Names are short, lowercase, snake-case, and stable across binary
/// versions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeriveConsumerName(&'static str);

impl DeriveConsumerName {
    /// Creates a derive-consumer name from a static string.
    ///
    /// The caller must ensure the name is stable across releases; renaming a
    /// consumer between deployments orphans its persisted cursor.
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        Self(name)
    }

    /// Returns the underlying string value used in cursor and metadata keys.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl AsRef<[u8]> for DeriveConsumerName {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Trait every derive consumer implements.
///
/// `DeriveConsumer` is the shared seam between the consumer-agnostic
/// infrastructure in `zinder-derive` and the consumer-specific aggregation
/// logic. Implementations are passed to
/// `backfill::backfill_then_attach`, which calls `apply_chain_committed`
/// for every committed range and `apply_chain_reorged` for every reorged
/// range. Implementations decide how to revert their derived state under
/// reorg.
///
/// The trait is async because most implementations need to read previous
/// running totals from the same `DeriveStore` they write to, which crosses
/// `RocksDB` boundaries.
#[async_trait]
pub trait DeriveConsumer: Send + Sync {
    /// Stable consumer identity used for cursor and metadata key prefixes.
    fn name(&self) -> DeriveConsumerName;
}
