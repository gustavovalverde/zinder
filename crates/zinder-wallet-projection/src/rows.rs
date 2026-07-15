//! Exact version-1 wallet projection query rows and durable byte layouts.

use zinder_core::wire::UtxoSetCommitmentElement;
use zinder_core::{
    BlockHash, BlockHeight, BlockId, Network, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint,
};

use crate::WalletProjectionContractError;
use crate::contract_error::encoded_len;

const OUTPOINT_KEY_LEN: usize = 36;
const ADDRESS_LIVE_OUTPUT_KEY_LEN: usize = 72;
const ADDRESS_HISTORY_KEY_LEN: usize = 40;
const LIVE_OUTPUT_FIXED_VALUE_LEN: usize = 80;
const SPENT_OUTPUT_TRAILER_LEN: usize = 76;

/// Position of one transaction in the canonical chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletTransactionPosition {
    /// Transaction identifier.
    pub transaction_id: TransactionId,
    /// Block-local transaction index.
    pub tx_index_in_block: u32,
    /// Canonical block containing the transaction.
    pub block: BlockId,
}

impl WalletTransactionPosition {
    /// Creates a canonical transaction position.
    #[must_use]
    pub const fn new(
        transaction_id: TransactionId,
        tx_index_in_block: u32,
        block: BlockId,
    ) -> Self {
        Self {
            transaction_id,
            tx_index_in_block,
            block,
        }
    }
}

/// Full transparent output retained while it remains unspent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletLiveOutput {
    /// Output identity.
    pub outpoint: TransparentOutPoint,
    /// Hash of the raw output script.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Output value in zatoshi.
    pub value_zat: u64,
    /// Raw `scriptPubKey` bytes.
    pub script_pub_key: Vec<u8>,
    /// Transaction and block that created the output.
    pub created_at: WalletTransactionPosition,
}

impl WalletLiveOutput {
    /// Creates an unspent wallet output.
    pub fn new(
        outpoint: TransparentOutPoint,
        address_script_hash: TransparentAddressScriptHash,
        value_zat: u64,
        script_pub_key: impl Into<Vec<u8>>,
        created_at: WalletTransactionPosition,
    ) -> Result<Self, WalletProjectionContractError> {
        if outpoint.transaction_id != created_at.transaction_id {
            return Err(WalletProjectionContractError::OutputCreatorMismatch);
        }
        Ok(Self {
            outpoint,
            address_script_hash,
            value_zat,
            script_pub_key: script_pub_key.into(),
            created_at,
        })
    }

    /// Encodes the exact version-1 `live_output` value.
    pub fn encode_value(&self) -> Result<Vec<u8>, WalletProjectionContractError> {
        let script_len = encoded_len(self.script_pub_key.len(), "live output script")?;
        let mut bytes = Vec::with_capacity(LIVE_OUTPUT_FIXED_VALUE_LEN + self.script_pub_key.len());
        bytes.extend_from_slice(&self.address_script_hash.as_bytes());
        bytes.extend_from_slice(&self.value_zat.to_be_bytes());
        bytes.extend_from_slice(&self.created_at.block.height.value().to_be_bytes());
        bytes.extend_from_slice(&self.created_at.block.hash.as_bytes());
        bytes.extend_from_slice(&script_len.to_be_bytes());
        bytes.extend_from_slice(&self.script_pub_key);
        Ok(bytes)
    }

    pub(crate) fn commitment_element(&self, network: Network) -> UtxoSetCommitmentElement<'_> {
        UtxoSetCommitmentElement {
            network_id: network.id(),
            outpoint: self.outpoint,
            value_zat: self.value_zat,
            script_pub_key: &self.script_pub_key,
            block_height: self.created_at.block.height,
        }
    }
}

/// Full historical transparent output paired with its consuming input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletSpentOutput {
    /// Original output and creator location.
    pub output: WalletLiveOutput,
    /// Transaction and block that spent the output.
    pub spent_at: WalletTransactionPosition,
    /// Transparent input index inside the spending transaction.
    pub input_index: u32,
}

impl WalletSpentOutput {
    /// Creates a historical spent output.
    #[must_use]
    pub const fn new(
        output: WalletLiveOutput,
        spent_at: WalletTransactionPosition,
        input_index: u32,
    ) -> Self {
        Self {
            output,
            spent_at,
            input_index,
        }
    }

    /// Encodes the exact version-1 `spent_output` value.
    pub fn encode_value(&self) -> Result<Vec<u8>, WalletProjectionContractError> {
        let mut bytes = self.output.encode_value()?;
        bytes.reserve(SPENT_OUTPUT_TRAILER_LEN);
        bytes.extend_from_slice(&self.spent_at.transaction_id.as_bytes());
        bytes.extend_from_slice(&self.input_index.to_be_bytes());
        bytes.extend_from_slice(&self.spent_at.tx_index_in_block.to_be_bytes());
        bytes.extend_from_slice(&self.spent_at.block.height.value().to_be_bytes());
        bytes.extend_from_slice(&self.spent_at.block.hash.as_bytes());
        Ok(bytes)
    }
}

/// Lexicographically ordered version-1 `live_output` and `spent_output` key.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WalletOutpointKey([u8; OUTPOINT_KEY_LEN]);

impl WalletOutpointKey {
    /// Encodes an outpoint as `txid || output_index_be`.
    #[must_use]
    pub const fn new(outpoint: TransparentOutPoint) -> Self {
        let mut bytes = [0u8; OUTPOINT_KEY_LEN];
        let transaction_id = outpoint.transaction_id.as_bytes();
        let output_index = outpoint.output_index.to_be_bytes();
        let mut index = 0;
        while index < transaction_id.len() {
            bytes[index] = transaction_id[index];
            index += 1;
        }
        bytes[32] = output_index[0];
        bytes[33] = output_index[1];
        bytes[34] = output_index[2];
        bytes[35] = output_index[3];
        Self(bytes)
    }

    /// Returns the exact durable key bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; OUTPOINT_KEY_LEN] {
        &self.0
    }
}

/// Ordered version-1 secondary key for live outputs belonging to an address.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WalletAddressLiveOutputKey([u8; ADDRESS_LIVE_OUTPUT_KEY_LEN]);

impl WalletAddressLiveOutputKey {
    /// Encodes `address_hash || creation_height_be || txid || output_index_be`.
    #[must_use]
    pub fn new(output: &WalletLiveOutput) -> Self {
        let mut bytes = [0u8; ADDRESS_LIVE_OUTPUT_KEY_LEN];
        bytes[..32].copy_from_slice(&output.address_script_hash.as_bytes());
        bytes[32..36].copy_from_slice(&output.created_at.block.height.value().to_be_bytes());
        bytes[36..68].copy_from_slice(&output.outpoint.transaction_id.as_bytes());
        bytes[68..].copy_from_slice(&output.outpoint.output_index.to_be_bytes());
        Self(bytes)
    }

    /// Returns the exact durable key bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; ADDRESS_LIVE_OUTPUT_KEY_LEN] {
        &self.0
    }
}

/// Ordered version-1 key for one address-touching transaction.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WalletAddressHistoryKey([u8; ADDRESS_HISTORY_KEY_LEN]);

impl WalletAddressHistoryKey {
    /// Encodes `address_hash || height_be || transaction_index_be`.
    #[must_use]
    pub fn new(
        address_script_hash: TransparentAddressScriptHash,
        block_height: BlockHeight,
        tx_index_in_block: u32,
    ) -> Self {
        let mut bytes = [0u8; ADDRESS_HISTORY_KEY_LEN];
        bytes[..32].copy_from_slice(&address_script_hash.as_bytes());
        bytes[32..36].copy_from_slice(&block_height.value().to_be_bytes());
        bytes[36..].copy_from_slice(&tx_index_in_block.to_be_bytes());
        Self(bytes)
    }

    /// Returns the exact durable key bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; ADDRESS_HISTORY_KEY_LEN] {
        &self.0
    }
}

/// One address transaction-history row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletAddressHistoryEntry {
    /// Ordered address-history key.
    pub key: WalletAddressHistoryKey,
    /// Transaction touching the address.
    pub transaction_id: TransactionId,
    /// Canonical block containing the transaction.
    pub block_hash: BlockHash,
}

impl WalletAddressHistoryEntry {
    /// Creates one address history row.
    #[must_use]
    pub const fn new(
        key: WalletAddressHistoryKey,
        transaction_id: TransactionId,
        block_hash: BlockHash,
    ) -> Self {
        Self {
            key,
            transaction_id,
            block_hash,
        }
    }

    /// Encodes `txid || block_hash`.
    #[must_use]
    pub fn encode_value(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&self.transaction_id.as_bytes());
        bytes[32..].copy_from_slice(&self.block_hash.as_bytes());
        bytes
    }
}

/// Current transparent balance for one address script hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletAddressBalance {
    /// Address script hash.
    pub address_script_hash: TransparentAddressScriptHash,
    /// Sum of currently unspent outputs in zatoshi.
    pub balance_zat: u64,
}

impl WalletAddressBalance {
    /// Encodes the exact `address_balance` key.
    #[must_use]
    pub const fn encode_key(self) -> [u8; 32] {
        self.address_script_hash.as_bytes()
    }

    /// Encodes the exact version-1 balance value.
    #[must_use]
    pub const fn encode_value(self) -> [u8; 8] {
        self.balance_zat.to_be_bytes()
    }
}

/// Reversible changes made while projecting one canonical block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletReorgUndo {
    /// Canonical block whose wallet changes this record reverses.
    pub block: BlockId,
    /// Outputs created by this block.
    pub created_outpoints: Vec<WalletOutpointKey>,
    /// Prior outputs spent by this block.
    pub spent_outpoints: Vec<WalletOutpointKey>,
    /// Exact address-history rows inserted by this block.
    pub address_history_keys: Vec<WalletAddressHistoryKey>,
}

impl WalletReorgUndo {
    /// Encodes the exact version-1 height key.
    #[must_use]
    pub const fn encode_key(&self) -> [u8; 4] {
        self.block.height.value().to_be_bytes()
    }

    /// Encodes the exact version-1 undo value.
    pub fn encode_value(&self) -> Result<Vec<u8>, WalletProjectionContractError> {
        let created_count = encoded_len(self.created_outpoints.len(), "created outpoint list")?;
        let spent_count = encoded_len(self.spent_outpoints.len(), "spent outpoint list")?;
        let history_count = encoded_len(self.address_history_keys.len(), "history key list")?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.block.hash.as_bytes());
        bytes.extend_from_slice(&created_count.to_be_bytes());
        for key in &self.created_outpoints {
            bytes.extend_from_slice(key.as_bytes());
        }
        bytes.extend_from_slice(&spent_count.to_be_bytes());
        for key in &self.spent_outpoints {
            bytes.extend_from_slice(key.as_bytes());
        }
        bytes.extend_from_slice(&history_count.to_be_bytes());
        for key in &self.address_history_keys {
            bytes.extend_from_slice(key.as_bytes());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_outpoint() -> TransparentOutPoint {
        TransparentOutPoint::new(TransactionId::from_bytes([0x11; 32]), 0x0102_0304)
    }

    fn sample_live_output() -> WalletLiveOutput {
        WalletLiveOutput::new(
            sample_outpoint(),
            TransparentAddressScriptHash::from_bytes([0x22; 32]),
            0x0102_0304_0506_0708,
            [0x51, 0xac],
            WalletTransactionPosition::new(
                TransactionId::from_bytes([0x11; 32]),
                0x1112_1314,
                BlockId::new(
                    BlockHeight::new(0x0a0b_0c0d),
                    BlockHash::from_bytes([0x33; 32]),
                ),
            ),
        )
        .unwrap_or_else(|error| unreachable!("valid sample output: {error}"))
    }

    #[test]
    fn outpoint_and_address_keys_have_exact_version_one_bytes() {
        let output = sample_live_output();
        assert_eq!(
            hex::encode(WalletOutpointKey::new(output.outpoint).as_bytes()),
            concat!(
                "1111111111111111111111111111111111111111111111111111111111111111",
                "01020304"
            )
        );
        assert_eq!(
            hex::encode(WalletAddressLiveOutputKey::new(&output).as_bytes()),
            concat!(
                "2222222222222222222222222222222222222222222222222222222222222222",
                "0a0b0c0d",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "01020304"
            )
        );
    }

    #[test]

    fn live_and_spent_values_have_exact_version_one_bytes() {
        let output = sample_live_output();
        assert_eq!(
            hex::encode(
                output
                    .encode_value()
                    .unwrap_or_else(|error| unreachable!("valid sample output: {error}"))
            ),
            concat!(
                "2222222222222222222222222222222222222222222222222222222222222222",
                "0102030405060708",
                "0a0b0c0d",
                "3333333333333333333333333333333333333333333333333333333333333333",
                "00000002",
                "51ac"
            )
        );

        let spent = WalletSpentOutput::new(
            output,
            WalletTransactionPosition::new(
                TransactionId::from_bytes([0x44; 32]),
                0x3132_3334,
                BlockId::new(
                    BlockHeight::new(0x4142_4344),
                    BlockHash::from_bytes([0x55; 32]),
                ),
            ),
            0x2122_2324,
        );
        assert_eq!(
            hex::encode(
                spent
                    .encode_value()
                    .unwrap_or_else(|error| unreachable!("valid sample spend: {error}"))
            ),
            concat!(
                "2222222222222222222222222222222222222222222222222222222222222222",
                "0102030405060708",
                "0a0b0c0d",
                "3333333333333333333333333333333333333333333333333333333333333333",
                "00000002",
                "51ac",
                "4444444444444444444444444444444444444444444444444444444444444444",
                "21222324",
                "31323334",
                "41424344",
                "5555555555555555555555555555555555555555555555555555555555555555"
            )
        );
    }

    #[test]

    fn history_balance_and_undo_have_exact_version_one_bytes() {
        let address = TransparentAddressScriptHash::from_bytes([0x22; 32]);
        let history_key =
            WalletAddressHistoryKey::new(address, BlockHeight::new(0x0a0b_0c0d), 0x1112_1314);
        let history = WalletAddressHistoryEntry::new(
            history_key,
            TransactionId::from_bytes([0x11; 32]),
            BlockHash::from_bytes([0x33; 32]),
        );
        assert_eq!(
            hex::encode(history_key.as_bytes()),
            concat!(
                "2222222222222222222222222222222222222222222222222222222222222222",
                "0a0b0c0d",
                "11121314"
            )
        );
        assert_eq!(
            hex::encode(history.encode_value()),
            concat!(
                "1111111111111111111111111111111111111111111111111111111111111111",
                "3333333333333333333333333333333333333333333333333333333333333333"
            )
        );
        let balance = WalletAddressBalance {
            address_script_hash: address,
            balance_zat: 0x0102_0304_0506_0708,
        };
        assert_eq!(hex::encode(balance.encode_key()), "22".repeat(32));
        assert_eq!(hex::encode(balance.encode_value()), "0102030405060708");

        let undo = WalletReorgUndo {
            block: BlockId::new(
                BlockHeight::new(0x0a0b_0c0d),
                BlockHash::from_bytes([0x33; 32]),
            ),
            created_outpoints: vec![WalletOutpointKey::new(sample_outpoint())],
            spent_outpoints: vec![WalletOutpointKey::new(sample_outpoint())],
            address_history_keys: vec![history_key],
        };
        assert_eq!(hex::encode(undo.encode_key()), "0a0b0c0d");
        assert_eq!(
            hex::encode(
                undo.encode_value()
                    .unwrap_or_else(|error| unreachable!("valid sample undo: {error}"))
            ),
            concat!(
                "3333333333333333333333333333333333333333333333333333333333333333",
                "00000001",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "01020304",
                "00000001",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "01020304",
                "00000001",
                "2222222222222222222222222222222222222222222222222222222222222222",
                "0a0b0c0d",
                "11121314"
            )
        );
    }
}
