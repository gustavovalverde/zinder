//! Shared version-1 projection-row digest framing.

use sha2::{Digest, Sha256};

use crate::contract_error::encoded_len;
use crate::{
    WALLET_PROJECTION_VALUE_ENCODING_VERSION, WalletProjectionContractError, WalletProjectionDigest,
};

const PROJECTION_DIGEST_DOMAIN: &[u8] = b"zinder:wallet-projection:rows:v1\0";
const FIRST_FAMILY_TAG: u8 = 1;
const LAST_FAMILY_TAG: u8 = 6;

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
    const fn tag(self) -> u8 {
        self as u8
    }
}

/// Streaming digest builder that enforces complete family and key ordering.
pub struct WalletProjectionDigestBuilder {
    hasher: Sha256,
    next_family_tag: u8,
    remaining_rows: u64,
    previous_key: Option<Vec<u8>>,
}

impl WalletProjectionDigestBuilder {
    /// Starts one empty version-1 wallet projection digest.
    #[must_use]
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PROJECTION_DIGEST_DOMAIN);
        hasher.update(WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());
        Self {
            hasher,
            next_family_tag: FIRST_FAMILY_TAG,
            remaining_rows: 0,
            previous_key: None,
        }
    }

    /// Frames the next exact family and its complete row count.
    pub fn begin_family(
        &mut self,
        family: WalletProjectionRowFamily,
        row_count: u64,
    ) -> Result<(), WalletProjectionContractError> {
        if self.remaining_rows != 0 {
            return Err(WalletProjectionContractError::ProjectionDigestRowCountMismatch);
        }
        let family_tag = family.tag();
        if family_tag != self.next_family_tag {
            return Err(WalletProjectionContractError::ProjectionDigestFamilyOrder {
                expected: self.next_family_tag,
                actual: family_tag,
            });
        }
        self.hasher.update([family_tag]);
        self.hasher.update(row_count.to_be_bytes());
        self.next_family_tag = self.next_family_tag.saturating_add(1);
        self.remaining_rows = row_count;
        self.previous_key = None;
        Ok(())
    }

    /// Commits one row whose durable key is strictly after the prior key.
    pub fn append_row(
        &mut self,
        key: &[u8],
        encoded_value: &[u8],
    ) -> Result<(), WalletProjectionContractError> {
        if self.remaining_rows == 0 {
            return Err(WalletProjectionContractError::ProjectionDigestRowCountMismatch);
        }
        if self
            .previous_key
            .as_deref()
            .is_some_and(|previous_key| previous_key >= key)
        {
            return Err(WalletProjectionContractError::ProjectionDigestKeyOrder);
        }
        let key_len = encoded_len(key.len(), "projection digest key")?;
        let value_len = encoded_len(encoded_value.len(), "projection digest value")?;
        self.hasher.update(key_len.to_be_bytes());
        self.hasher.update(key);
        self.hasher.update(value_len.to_be_bytes());
        self.hasher.update(encoded_value);
        self.previous_key = Some(key.to_vec());
        self.remaining_rows = self.remaining_rows.saturating_sub(1);
        Ok(())
    }

    /// Finalizes only after all six families and declared rows were committed.
    pub fn finish(self) -> Result<WalletProjectionDigest, WalletProjectionContractError> {
        if self.remaining_rows != 0 {
            return Err(WalletProjectionContractError::ProjectionDigestRowCountMismatch);
        }
        if self.next_family_tag != LAST_FAMILY_TAG.saturating_add(1) {
            return Err(WalletProjectionContractError::ProjectionDigestIncomplete);
        }
        Ok(WalletProjectionDigest::from_bytes(
            self.hasher.finalize().into(),
        ))
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
    fn digest_builder_requires_every_family_and_exact_key_order() {
        let mut digest = WalletProjectionDigestBuilder::new();
        assert_eq!(
            digest.begin_family(WalletProjectionRowFamily::TransparentSpentOutput, 0),
            Err(WalletProjectionContractError::ProjectionDigestFamilyOrder {
                expected: 1,
                actual: 3,
            })
        );
        digest
            .begin_family(WalletProjectionRowFamily::TransparentUnspentOutput, 2)
            .unwrap_or_else(|error| unreachable!("valid first family: {error}"));
        digest
            .append_row(b"a", b"one")
            .unwrap_or_else(|error| unreachable!("valid first row: {error}"));
        assert_eq!(
            digest.append_row(b"a", b"duplicate"),
            Err(WalletProjectionContractError::ProjectionDigestKeyOrder)
        );
        digest
            .append_row(b"b", b"two")
            .unwrap_or_else(|error| unreachable!("valid second row: {error}"));
        for family in [
            WalletProjectionRowFamily::TransparentUnspentOutputByAddress,
            WalletProjectionRowFamily::TransparentSpentOutput,
            WalletProjectionRowFamily::TransparentAddressTransaction,
            WalletProjectionRowFamily::TransparentAddressBalance,
            WalletProjectionRowFamily::ReorgUndo,
        ] {
            digest
                .begin_family(family, 0)
                .unwrap_or_else(|error| unreachable!("valid empty family: {error}"));
        }
        assert!(digest.finish().is_ok());
    }
}
