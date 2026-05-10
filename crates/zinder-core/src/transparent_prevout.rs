//! Transparent prevout read-model.
//!
//! Per [docs/specs/m6-prevout-resolution.md] D2 and D3, the wire surface for
//! prevout resolution returns intrinsic output data ([`TransparentPrevout`])
//! per requested outpoint, in input order, bound to one [`ChainEpoch`]. The
//! identifying outpoint stays on the owning [`TransparentPrevoutEntry`] so
//! the inner payload carries no redundant fields.
//!
//! Slice 1 (canonical): `WalletQuery.TransparentPrevouts` reads from
//! `TransactionArtifact.payload_bytes` (Shape C compute-at-read-time) and
//! never touches a dedicated column family.
//!
//! Slice 2 (mempool): `WalletQuery.TransparentMempoolPrevouts` reads from
//! the writer-owned `MempoolIndex`. The mempool variant binds to the chain
//! epoch visible at lookup time without supporting an `at_epoch` pin.
//!
//! Absent `prevout` per entry means the canonical chain at the bound epoch
//! (canonical) or the live mempool index (mempool) does not contain the
//! referenced output. Richer not-found discrimination is reserved for a
//! future revision; v1 follows the M3 mempool precedent of `optional T`.

use crate::{ChainEpoch, TransparentOutPoint};

/// Resolved transparent output referenced by an outpoint.
///
/// Carries the intrinsic fields a consumer needs to compute input value
/// and derive the owning address. Identifying fields like `transaction_id`
/// stay on the owning [`TransparentPrevoutEntry`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentPrevout {
    /// Output value in zatoshis.
    pub value_zat: u64,
    /// Raw scriptPubKey bytes.
    pub script_pub_key: Vec<u8>,
}

/// One entry in a prevout-resolution response, in input order.
///
/// `prevout` is `None` when the resolver could not find the referenced
/// output: for canonical reads, this means the transaction is not on the
/// best chain at the response's [`ChainEpoch`] (or `output_index` is
/// out of bounds); for mempool reads, this means the outpoint does not
/// reference an output of any transaction currently in the live mempool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentPrevoutEntry {
    /// Outpoint requested by the caller.
    pub outpoint: TransparentOutPoint,
    /// Resolved prevout, when found.
    pub prevout: Option<TransparentPrevout>,
}

/// Canonical-chain prevout resolution response bound to one chain epoch.
///
/// closes: G18
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentPrevoutsResponse {
    /// Chain epoch the response binds to. Every entry's `prevout` (when
    /// present) is visible at this epoch.
    pub chain_epoch: ChainEpoch,
    /// Per-outpoint resolution result, in input order. Length matches
    /// the request's outpoint list after server-side truncation.
    pub entries: Vec<TransparentPrevoutEntry>,
}

/// Live-mempool prevout resolution response bound to the chain epoch
/// visible at lookup time.
///
/// closes: G18
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentMempoolPrevoutsResponse {
    /// Chain epoch visible at lookup time.
    pub chain_epoch: ChainEpoch,
    /// Per-outpoint resolution result, in input order.
    pub entries: Vec<TransparentPrevoutEntry>,
}
