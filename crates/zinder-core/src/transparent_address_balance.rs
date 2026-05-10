//! Transparent-address balance read-model.
//!
//! A balance response splits the canonical confirmed total from a signed
//! mempool delta. The delta is computed at read time from the live mempool
//! surfaces and is not persisted; the response binds both values to the
//! [`ChainEpoch`] visible at lookup time.
//!
//! `unconfirmed_delta_zat` is signed because pending spends from the address
//! reduce the visible balance, while pending receives raise it. A miner that
//! has pending outflows greater than pending inflows would observe a negative
//! delta. Saturating arithmetic at the construction sites guarantees the wire
//! never carries an under- or over-flowed value.
//!
//! `address_count` records how many transparent addresses participated in the
//! response so consumers can distinguish "zero balance because none of these
//! addresses received anything" from "this exact address has zero outputs".

use crate::ChainEpoch;

/// Transparent-address balance bound to one chain epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentAddressBalance {
    /// Sum of unspent canonical outputs at the requested addresses.
    pub confirmed_zat: u64,
    /// Signed mempool delta: positive contributions (pending inflows) minus
    /// negative contributions (pending outflows). Computed at read time;
    /// never persisted.
    pub unconfirmed_delta_zat: i64,
    /// Number of transparent addresses included in this response. Equal to
    /// the requested address count when the request resolved cleanly.
    pub address_count: u32,
    /// Chain epoch visible at lookup time; bounds every numeric field above.
    pub chain_epoch: ChainEpoch,
}

impl TransparentAddressBalance {
    /// Returns a balance with all numeric fields zeroed at the given epoch.
    ///
    /// The wire shape always carries an `address_count`; callers that ask
    /// for an empty address list still receive a structured response that
    /// records the visible chain epoch.
    #[must_use]
    pub const fn empty(chain_epoch: ChainEpoch) -> Self {
        Self {
            confirmed_zat: 0,
            unconfirmed_delta_zat: 0,
            address_count: 0,
            chain_epoch,
        }
    }

    /// Convenience: total visible balance projected onto a non-negative
    /// integer, saturating to `0` when pending outflows exceed the
    /// confirmed total. Useful for legacy lightwalletd consumers that
    /// expose only the confirmed value.
    #[must_use]
    pub fn projected_total_zat(self) -> u64 {
        let signed_total =
            i128::from(self.confirmed_zat).saturating_add(i128::from(self.unconfirmed_delta_zat));
        u64::try_from(signed_total.max(0)).unwrap_or(u64::MAX)
    }
}
