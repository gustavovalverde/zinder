//! Chain-wide transparent UTXO-set summary read-model.
//!
//! Aggregates the current-UTXO projection into two unambiguous totals: the
//! number of unspent transparent outputs and the sum of their values. The
//! aggregate is taken at the chain epoch's settled tip, where the projection
//! holds exactly the irreversible unspent set: reorged-away creations and
//! finalized spends are already removed below that height.
//!
//! A serialized-set hash and byte size are intentionally not offered. Both
//! depend on a defined UTXO-set serialization ordering, which Zinder does not
//! commit to; only the order-independent count and value totals are reported.
//!
//! Every transparent output is counted, including non-standard and
//! provably-unspendable scripts (`OP_RETURN`, bare data outputs): the projection
//! keys outputs by the hash of their raw `scriptPubKey` and never inspects the
//! script template. The totals are therefore the full unspent set, not zcashd's
//! `IsUnspendable`-filtered set.

use crate::{BlockHeight, ChainEpoch};

/// Chain-wide transparent UTXO-set aggregate bound to one chain epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentUtxoSetSummary {
    /// Number of unspent transparent outputs at the summarized settled tip.
    pub utxo_count: u64,
    /// Sum of the values of every unspent transparent output, in zatoshi.
    pub total_value_zat: u64,
    /// Settled tip height the aggregate was taken at. Below this height the
    /// projection is the irreversible unspent set, so no spend re-check or
    /// producing-block visibility check is required.
    pub summarized_height: BlockHeight,
    /// Chain epoch visible at lookup time; bounds every field above.
    pub chain_epoch: ChainEpoch,
}

impl TransparentUtxoSetSummary {
    /// Returns an empty summary at the given epoch's settled tip.
    #[must_use]
    pub const fn empty(chain_epoch: ChainEpoch) -> Self {
        Self {
            utxo_count: 0,
            total_value_zat: 0,
            summarized_height: chain_epoch.settled_tip_height,
            chain_epoch,
        }
    }
}
