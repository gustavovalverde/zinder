//! Transparent prevout read-model.
//!
//! The wire surface for prevout resolution returns intrinsic output data
//! ([`TransparentPrevout`]) per requested outpoint, in input order, bound
//! to one [`ChainEpoch`]. The identifying outpoint stays on the owning
//! [`TransparentPrevoutEntry`] so the inner payload carries no redundant
//! fields.
//!
//! Canonical and mempool prevout resolution share the same response shape
//! ([`TransparentPrevoutsResponse`]). The two RPCs differ at the request
//! boundary: the canonical surface accepts an optional `at_epoch` pin while
//! the mempool surface always binds to the chain epoch visible at lookup
//! time. Source-of-truth differs too: canonical reads consult the persisted
//! `transparent_prevout` row written for each mined transparent output;
//! mempool reads consult the writer-owned `MempoolIndex` directly.
//!
//! Absent `prevout` per entry means the canonical chain at the bound epoch
//! (canonical) or the live mempool index (mempool) does not contain the
//! referenced output. Richer not-found discrimination is reserved for a
//! future revision; v1 returns `optional T` for consistency with the rest
//! of the wallet API.

use crate::{
    BlockHash, BlockHeight, ChainEpoch, TransparentAddressScriptHash, TransparentOutPoint,
};

/// Hard cap on the number of transparent outpoints one prevout-resolution
/// request may resolve.
///
/// Requests above the cap are silently truncated to the first N outpoints so
/// canonical, mempool, local, and remote paths expose the same batch contract.
/// The cap sizes against the wallet's server-side handler cost (one indexed
/// prevout read per unique outpoint) and the explorer's per-block prevout
/// fan-out (typical blocks carry a few dozen transparent inputs, so one batch
/// usually drains a block).
pub const MAX_TRANSPARENT_PREVOUTS_PER_REQUEST: usize = 1024;

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

/// Canonical transparent prevout artifact keyed by outpoint.
///
/// This row is written for every mined transparent output. It carries the
/// intrinsic output payload used by `WalletQuery.TransparentPrevouts` plus the
/// address and block identity needed by writer-side spend indexing and
/// reorg-safe reader visibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentPrevoutArtifact {
    /// Output identity (transaction id plus output index).
    pub outpoint: TransparentOutPoint,
    /// Output value in zatoshis.
    pub value_zat: u64,
    /// Raw scriptPubKey bytes.
    pub script_pub_key: Vec<u8>,
    /// Hash of the transparent output script.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Height of the block that mined the output.
    pub block_height: BlockHeight,
    /// Hash of the block that mined the output.
    pub block_hash: BlockHash,
}

impl TransparentPrevoutArtifact {
    /// Creates a transparent prevout artifact.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "transparent prevout artifacts are immutable persisted records; grouping fields would hide the storage identity"
    )]
    pub fn new(
        outpoint: TransparentOutPoint,
        value_zat: u64,
        script_pub_key: impl Into<Vec<u8>>,
        address_script_hash: TransparentAddressScriptHash,
        block_height: BlockHeight,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            outpoint,
            value_zat,
            script_pub_key: script_pub_key.into(),
            address_script_hash,
            block_height,
            block_hash,
        }
    }

    /// Converts the persisted artifact into the public prevout payload.
    #[must_use]
    pub fn into_prevout(self) -> TransparentPrevout {
        TransparentPrevout {
            value_zat: self.value_zat,
            script_pub_key: self.script_pub_key,
        }
    }
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

/// Prevout-resolution response bound to one chain epoch.
///
/// Used by both `WalletQuery.TransparentPrevouts` (canonical, accepts
/// `at_epoch`) and `WalletQuery.TransparentMempoolPrevouts` (live mempool,
/// always binds to the chain epoch visible at lookup time).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentPrevoutsResponse {
    /// Chain epoch the response binds to. Every entry's `prevout` (when
    /// present) is visible at this epoch.
    pub chain_epoch: ChainEpoch,
    /// Per-outpoint resolution result, in input order. Length matches
    /// the request's outpoint list after server-side truncation.
    pub entries: Vec<TransparentPrevoutEntry>,
}
