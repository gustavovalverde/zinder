//! Chain-wide transparent UTXO-set summary read-model.
//!
//! Aggregates the current-UTXO projection into two unambiguous totals: the
//! number of unspent transparent outputs and the sum of their values. The
//! aggregate is taken at the chain epoch's settled tip, where the projection
//! holds exactly the irreversible unspent set: reorged-away creations and
//! finalized spends are already removed below that height.
//!
//! An optional [`TransparentUtxoSetCommitment`] binds the full unspent set to a
//! single order-independent value under the `LtHash16` scheme: each output is
//! expanded through a BLAKE2X XOF to 1024 little-endian `u16` lanes and the
//! lanes are summed componentwise modulo `2^16`. The summation is commutative
//! and invertible, so the commitment is independent of scan order and two
//! deployments at the same settled tip agree byte-for-byte. The fold has real
//! per-output CPU cost, so it runs only when the operator opts in; otherwise the
//! field is absent.
//!
//! Every transparent output is counted, including non-standard and
//! provably-unspendable scripts (`OP_RETURN`, bare data outputs): the projection
//! keys outputs by the hash of their raw `scriptPubKey` and never inspects the
//! script template. The totals are therefore the full unspent set, not zcashd's
//! `IsUnspendable`-filtered set.

use crate::{BlockHeight, ChainEpoch, TransparentUtxoSetCommitment};

/// Chain-wide transparent UTXO-set aggregate bound to one chain epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentUtxoSetSummary {
    /// Number of unspent transparent outputs at the summarized settled tip.
    pub utxo_count: u64,
    /// Sum of the values of every unspent transparent output, in zatoshi.
    pub total_value_zat: u64,
    /// Homomorphic commitment to the full unspent set, present only when the
    /// operator opted into the request-time fold.
    pub commitment: Option<TransparentUtxoSetCommitment>,
    /// Settled tip height the aggregate was taken at. Below this height the
    /// projection is the irreversible unspent set, so no spend re-check or
    /// producing-block visibility check is required.
    pub summarized_height: BlockHeight,
    /// Chain epoch visible at lookup time; bounds every field above.
    pub chain_epoch: ChainEpoch,
}

impl TransparentUtxoSetSummary {
    /// Returns an empty summary at the given epoch's settled tip.
    ///
    /// The commitment is absent: callers that opted into the fold over a real
    /// scan attach it explicitly.
    #[must_use]
    pub const fn empty(chain_epoch: ChainEpoch) -> Self {
        Self {
            utxo_count: 0,
            total_value_zat: 0,
            commitment: None,
            summarized_height: chain_epoch.settled_tip_height,
            chain_epoch,
        }
    }
}
