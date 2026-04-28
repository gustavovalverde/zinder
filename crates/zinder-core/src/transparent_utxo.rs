//! Transparent UTXO artifact values.

use crate::{BlockHash, BlockHeight, TransactionId};

/// Hash of a transparent address `scriptPubKey`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransparentAddressScriptHash([u8; 32]);

impl TransparentAddressScriptHash {
    /// Creates a script hash from fixed 32-byte material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the script hash bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Transparent transaction output identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransparentOutPoint {
    /// Transaction that created the transparent output.
    pub transaction_id: TransactionId,
    /// Output index within the transaction.
    pub output_index: u32,
}

impl TransparentOutPoint {
    /// Creates a transparent outpoint.
    #[must_use]
    pub const fn new(transaction_id: TransactionId, output_index: u32) -> Self {
        Self {
            transaction_id,
            output_index,
        }
    }
}

/// Mined transparent output indexed by address script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressUtxoArtifact {
    /// Hash of the transparent output script.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Raw `scriptPubKey` bytes for the output.
    pub script_pub_key: Vec<u8>,
    /// Output identity.
    pub outpoint: TransparentOutPoint,
    /// Output value in zatoshis.
    pub value_zat: u64,
    /// Height of the block that mined the output.
    pub block_height: BlockHeight,
    /// Hash of the block that mined the output.
    pub block_hash: BlockHash,
}

impl TransparentAddressUtxoArtifact {
    /// Creates a transparent address UTXO artifact.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "transparent UTXO artifacts are immutable persisted records; grouping fields would hide the storage identity"
    )]
    pub fn new(
        address_script_hash: TransparentAddressScriptHash,
        script_pub_key: impl Into<Vec<u8>>,
        outpoint: TransparentOutPoint,
        value_zat: u64,
        block_height: BlockHeight,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            address_script_hash,
            script_pub_key: script_pub_key.into(),
            outpoint,
            value_zat,
            block_height,
            block_hash,
        }
    }
}

/// Transparent input that spends a previously mined transparent output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransparentUtxoSpendArtifact {
    /// Output consumed by this transparent input.
    pub spent_outpoint: TransparentOutPoint,
    /// Height of the block that mined the spending transaction.
    pub block_height: BlockHeight,
    /// Hash of the block that mined the spending transaction.
    pub block_hash: BlockHash,
}

impl TransparentUtxoSpendArtifact {
    /// Creates a transparent spend artifact.
    #[must_use]
    pub const fn new(
        spent_outpoint: TransparentOutPoint,
        block_height: BlockHeight,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            spent_outpoint,
            block_height,
            block_hash,
        }
    }
}
