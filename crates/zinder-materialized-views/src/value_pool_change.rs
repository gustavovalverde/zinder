//! Product-neutral value-pool balance changes derived from transaction facts.
//!
//! Transaction-intrinsic balances use the transaction-pool sign convention:
//! positive values enter the transaction from a pool. This module reverses
//! that direction so its values express each pool's post-block balance minus
//! its pre-block balance. Block-emission facts remain separate because future
//! flow and history consumers use them independently.

use zinder_core::TransactionIntrinsicValueBalances;

/// Invalid intrinsic balance that cannot be represented with the opposite sign.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ValuePoolChangeError {
    /// The source value is outside the representable post-minus-pre delta range.
    #[error("{pool_id} intrinsic balance {balance_zat} cannot be inverted as i64")]
    BalanceOutOfRange {
        /// Product-neutral shielded pool identifier.
        pool_id: &'static str,
        /// Invalid transaction-intrinsic balance in zatoshi.
        balance_zat: i64,
    },
}

/// Shielded-pool balance deltas expressed as post-block minus pre-block zatoshi.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShieldedPoolBalanceDeltas {
    /// Sprout pool balance delta in zatoshi.
    pub sprout_zat: i64,
    /// Sapling pool balance delta in zatoshi.
    pub sapling_zat: i64,
    /// Orchard pool balance delta in zatoshi.
    pub orchard_zat: i64,
    /// Ironwood pool balance delta in zatoshi.
    pub ironwood_zat: i64,
}

/// Converts one transaction's intrinsic balances into value-pool deltas.
///
/// Consensus-valid balances fit this range. The explicit error keeps malformed
/// or synthetic `i64::MIN` facts from panicking or wrapping at this boundary.
pub fn shielded_pool_balance_deltas(
    transaction_balances: TransactionIntrinsicValueBalances,
) -> Result<ShieldedPoolBalanceDeltas, ValuePoolChangeError> {
    Ok(ShieldedPoolBalanceDeltas {
        sprout_zat: pool_balance_delta("sprout", transaction_balances.sprout_zat)?,
        sapling_zat: pool_balance_delta("sapling", transaction_balances.sapling_zat)?,
        orchard_zat: pool_balance_delta("orchard", transaction_balances.orchard_zat)?,
        ironwood_zat: pool_balance_delta("ironwood", transaction_balances.ironwood_zat)?,
    })
}

fn pool_balance_delta(
    pool_id: &'static str,
    balance_zat: i64,
) -> Result<i64, ValuePoolChangeError> {
    balance_zat
        .checked_neg()
        .ok_or(ValuePoolChangeError::BalanceOutOfRange {
            pool_id,
            balance_zat,
        })
}

#[cfg(test)]
mod tests {
    use zinder_core::TransactionIntrinsicValueBalances;

    use super::{ShieldedPoolBalanceDeltas, ValuePoolChangeError, shielded_pool_balance_deltas};

    #[test]
    fn positive_transaction_balances_reduce_pool_balances() -> Result<(), ValuePoolChangeError> {
        let transaction_balances = TransactionIntrinsicValueBalances::new(7, 11, 13, 17);

        let pool_deltas = shielded_pool_balance_deltas(transaction_balances)?;

        assert_eq!(
            pool_deltas,
            ShieldedPoolBalanceDeltas {
                sprout_zat: -7,
                sapling_zat: -11,
                orchard_zat: -13,
                ironwood_zat: -17,
            }
        );
        assert_eq!(transaction_balances.sprout_zat + pool_deltas.sprout_zat, 0);
        assert_eq!(
            transaction_balances.sapling_zat + pool_deltas.sapling_zat,
            0
        );
        assert_eq!(
            transaction_balances.orchard_zat + pool_deltas.orchard_zat,
            0
        );
        assert_eq!(
            transaction_balances.ironwood_zat + pool_deltas.ironwood_zat,
            0
        );
        Ok(())
    }

    #[test]
    fn negative_transaction_balances_increase_pool_balances() -> Result<(), ValuePoolChangeError> {
        let transaction_balances = TransactionIntrinsicValueBalances::new(-7, -11, -13, -17);

        assert_eq!(
            shielded_pool_balance_deltas(transaction_balances)?,
            ShieldedPoolBalanceDeltas {
                sprout_zat: 7,
                sapling_zat: 11,
                orchard_zat: 13,
                ironwood_zat: 17,
            }
        );
        Ok(())
    }

    #[test]
    fn zero_transaction_balances_leave_every_pool_unchanged() -> Result<(), ValuePoolChangeError> {
        assert_eq!(
            shielded_pool_balance_deltas(TransactionIntrinsicValueBalances::default())?,
            ShieldedPoolBalanceDeltas::default()
        );
        Ok(())
    }

    #[test]
    fn minimum_intrinsic_balance_is_rejected_without_overflow() {
        assert_eq!(
            shielded_pool_balance_deltas(
                TransactionIntrinsicValueBalances::new(0, i64::MIN, 0, 0,)
            ),
            Err(ValuePoolChangeError::BalanceOutOfRange {
                pool_id: "sapling",
                balance_zat: i64::MIN,
            })
        );
    }
}
