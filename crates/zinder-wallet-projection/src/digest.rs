//! Shared version-1 projection-row digest framing.

use sha2::{Digest, Sha256};

use crate::contract_error::encoded_len;
use crate::{
    WALLET_PROJECTION_VALUE_ENCODING_VERSION, WalletProjectionContractError,
    WalletProjectionDigest, WalletProjectionFamilyRowCounts,
};

const PROJECTION_DIGEST_DOMAIN: &[u8] = b"zinder:wallet-projection:rows:v1\0";
const PROJECTION_FAMILY_DIGEST_DOMAIN: &[u8] = b"zinder:wallet-projection:family:v1\0";
const FAMILY_COUNT: usize = 6;

/// Durable row families committed by a version-1 wallet projection digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WalletProjectionRowFamily {
    /// Primary rows for currently unspent outputs.
    TransparentUnspentOutput = 1,
    /// Address-ordered secondary keys for unspent outputs.
    TransparentUnspentOutputByAddress = 2,
    /// Historical outputs paired with their consuming inputs.
    TransparentSpentOutput = 3,
    /// Address-ordered transactions touching transparent state.
    TransparentAddressTransaction = 4,
    /// Current non-zero transparent address balances.
    TransparentAddressBalance = 5,
    /// Bounded inverse deltas used for canonical reorgs.
    ReorgUndo = 6,
}

impl WalletProjectionRowFamily {
    const ORDERED: [Self; FAMILY_COUNT] = [
        Self::TransparentUnspentOutput,
        Self::TransparentUnspentOutputByAddress,
        Self::TransparentSpentOutput,
        Self::TransparentAddressTransaction,
        Self::TransparentAddressBalance,
        Self::ReorgUndo,
    ];

    const fn tag(self) -> u8 {
        self as u8
    }

    const fn index(self) -> usize {
        match self {
            Self::TransparentUnspentOutput => 0,
            Self::TransparentUnspentOutputByAddress => 1,
            Self::TransparentSpentOutput => 2,
            Self::TransparentAddressTransaction => 3,
            Self::TransparentAddressBalance => 4,
            Self::ReorgUndo => 5,
        }
    }
}

struct FamilyDigestAccumulator {
    hasher: Sha256,
    row_count: u64,
    previous_key: Option<Vec<u8>>,
}

impl FamilyDigestAccumulator {
    fn new(family: WalletProjectionRowFamily) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PROJECTION_FAMILY_DIGEST_DOMAIN);
        hasher.update(WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());
        hasher.update([family.tag()]);
        Self {
            hasher,
            row_count: 0,
            previous_key: None,
        }
    }

    fn append_row(
        &mut self,
        key: &[u8],
        encoded_value: &[u8],
    ) -> Result<(), WalletProjectionContractError> {
        if self
            .previous_key
            .as_deref()
            .is_some_and(|previous_key| previous_key >= key)
        {
            return Err(WalletProjectionContractError::ProjectionDigestKeyOrder);
        }
        let key_len = encoded_len(key.len(), "projection digest key")?;
        let value_len = encoded_len(encoded_value.len(), "projection digest value")?;
        let next_row_count = self
            .row_count
            .checked_add(1)
            .ok_or(WalletProjectionContractError::ProjectionDigestRowCountOverflow)?;

        self.hasher.update(key_len.to_be_bytes());
        self.hasher.update(key);
        self.hasher.update(value_len.to_be_bytes());
        self.hasher.update(encoded_value);
        self.row_count = next_row_count;
        self.previous_key = Some(key.to_vec());
        Ok(())
    }
}

/// Streaming digest builder with independent, strictly ordered family streams.
///
/// Version 1 hashes every family independently so an external loader may emit
/// families in any interleaving without retaining their rows in memory. Each
/// family digest commits its domain, value-encoding version, family tag, and
/// length-framed rows. Finalization commits the six family tags, observed row
/// counts, and family digests to the root digest in enum order.
pub struct WalletProjectionDigestBuilder {
    family_accumulators: [FamilyDigestAccumulator; FAMILY_COUNT],
}

impl WalletProjectionDigestBuilder {
    /// Starts one empty version-1 wallet projection digest.
    #[must_use]
    pub fn new() -> Self {
        Self {
            family_accumulators: WalletProjectionRowFamily::ORDERED
                .map(FamilyDigestAccumulator::new),
        }
    }

    /// Commits one row after the prior key in the selected family.
    ///
    /// Rows from different families may be interleaved. Keys within one family
    /// must be strictly increasing.
    pub fn append_row(
        &mut self,
        family: WalletProjectionRowFamily,
        key: &[u8],
        encoded_value: &[u8],
    ) -> Result<(), WalletProjectionContractError> {
        self.family_accumulators[family.index()].append_row(key, encoded_value)
    }

    /// Returns the row counts observed by all six family accumulators.
    #[must_use]
    pub fn row_counts(&self) -> WalletProjectionFamilyRowCounts {
        WalletProjectionFamilyRowCounts {
            transparent_unspent_output_count: self
                .row_count(WalletProjectionRowFamily::TransparentUnspentOutput),
            transparent_unspent_output_by_address_count: self
                .row_count(WalletProjectionRowFamily::TransparentUnspentOutputByAddress),
            transparent_spent_output_count: self
                .row_count(WalletProjectionRowFamily::TransparentSpentOutput),
            transparent_address_transaction_count: self
                .row_count(WalletProjectionRowFamily::TransparentAddressTransaction),
            transparent_address_balance_count: self
                .row_count(WalletProjectionRowFamily::TransparentAddressBalance),
            reorg_undo_count: self.row_count(WalletProjectionRowFamily::ReorgUndo),
        }
    }

    /// Commits all six family digests and observed counts in fixed version-1 order.
    #[must_use]
    pub fn finish(self) -> WalletProjectionDigest {
        let mut hasher = Sha256::new();
        hasher.update(PROJECTION_DIGEST_DOMAIN);
        hasher.update(WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());
        for (family, accumulator) in WalletProjectionRowFamily::ORDERED
            .into_iter()
            .zip(self.family_accumulators)
        {
            hasher.update([family.tag()]);
            hasher.update(accumulator.row_count.to_be_bytes());
            hasher.update(accumulator.hasher.finalize());
        }
        WalletProjectionDigest::from_bytes(hasher.finalize().into())
    }

    const fn row_count(&self, family: WalletProjectionRowFamily) -> u64 {
        self.family_accumulators[family.index()].row_count
    }
}

impl Default for WalletProjectionDigestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_builder_accepts_interleaved_families_and_counts_rows() {
        let mut digest = WalletProjectionDigestBuilder::new();
        digest
            .append_row(
                WalletProjectionRowFamily::TransparentSpentOutput,
                b"a",
                b"spent",
            )
            .unwrap_or_else(|error| unreachable!("valid spent row: {error}"));
        digest
            .append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                b"a",
                b"one",
            )
            .unwrap_or_else(|error| unreachable!("valid first unspent row: {error}"));
        digest
            .append_row(
                WalletProjectionRowFamily::TransparentAddressBalance,
                b"address",
                b"balance",
            )
            .unwrap_or_else(|error| unreachable!("valid balance row: {error}"));
        digest
            .append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                b"b",
                b"two",
            )
            .unwrap_or_else(|error| unreachable!("valid second unspent row: {error}"));

        assert_eq!(
            digest.row_counts(),
            WalletProjectionFamilyRowCounts {
                transparent_unspent_output_count: 2,
                transparent_unspent_output_by_address_count: 0,
                transparent_spent_output_count: 1,
                transparent_address_transaction_count: 0,
                transparent_address_balance_count: 1,
                reorg_undo_count: 0,
            }
        );
    }

    #[test]
    fn digest_builder_enforces_key_order_per_family_only() {
        let mut digest = WalletProjectionDigestBuilder::new();
        digest
            .append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                b"a",
                b"one",
            )
            .unwrap_or_else(|error| unreachable!("valid unspent row: {error}"));
        digest
            .append_row(
                WalletProjectionRowFamily::TransparentSpentOutput,
                b"a",
                b"spent",
            )
            .unwrap_or_else(|error| unreachable!("same key in another family is valid: {error}"));
        assert_eq!(
            digest.append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                b"a",
                b"duplicate",
            ),
            Err(WalletProjectionContractError::ProjectionDigestKeyOrder)
        );
    }

    #[test]
    fn digest_is_independent_of_family_interleaving() {
        let mut family_order = WalletProjectionDigestBuilder::new();
        family_order
            .append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                b"a",
                b"one",
            )
            .unwrap_or_else(|error| unreachable!("valid unspent row: {error}"));
        family_order
            .append_row(
                WalletProjectionRowFamily::TransparentSpentOutput,
                b"b",
                b"two",
            )
            .unwrap_or_else(|error| unreachable!("valid spent row: {error}"));

        let mut reverse_order = WalletProjectionDigestBuilder::new();
        reverse_order
            .append_row(
                WalletProjectionRowFamily::TransparentSpentOutput,
                b"b",
                b"two",
            )
            .unwrap_or_else(|error| unreachable!("valid spent row: {error}"));
        reverse_order
            .append_row(
                WalletProjectionRowFamily::TransparentUnspentOutput,
                b"a",
                b"one",
            )
            .unwrap_or_else(|error| unreachable!("valid unspent row: {error}"));

        assert_eq!(family_order.finish(), reverse_order.finish());
    }
}
