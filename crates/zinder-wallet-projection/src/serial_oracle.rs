//! Deterministic serial correctness oracle for wallet projection builders.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use zinder_core::{
    BlockHash, BlockHeight, BlockId, CanonicalBlockFacts, Network, TransparentAddressScriptHash,
    TransparentOutPoint, TransparentUtxoSetCommitment,
};

use crate::contract_error::encoded_len;
use crate::{
    WALLET_PROJECTION_VALUE_ENCODING_VERSION, WalletAddressHistoryEntry, WalletAddressHistoryKey,
    WalletAddressLiveOutputKey, WalletLiveOutput, WalletOutpointKey, WalletProjectionContractError,
    WalletProjectionDigest, WalletProjectionFamilyRowCounts, WalletReorgUndo, WalletSpentOutput,
    WalletTransactionPosition, WalletUtxoSetSummary,
};

const PROJECTION_DIGEST_DOMAIN: &[u8] = b"zinder:wallet-projection:rows:v1\0";

/// Small deterministic reference model for complete-history wallet projection.
///
/// This oracle deliberately favors obvious serial transitions over throughput.
/// Bulk `RocksDB` and Postgres builders can compare their logical rows, balances,
/// UTXO aggregate, and projection digest against this model on bounded fixtures.
#[derive(Clone, Debug)]
pub struct WalletProjectionSerialOracle {
    network: Network,
    last_projected_block: Option<BlockId>,
    live_outputs: BTreeMap<WalletOutpointKey, WalletLiveOutput>,
    spent_outputs: BTreeMap<WalletOutpointKey, WalletSpentOutput>,
    address_history: BTreeMap<WalletAddressHistoryKey, WalletAddressHistoryEntry>,
    balance_by_address: BTreeMap<[u8; 32], u64>,
    utxo_count: u64,
    total_utxo_value_zat: u64,
    utxo_commitment: TransparentUtxoSetCommitment,
}

impl WalletProjectionSerialOracle {
    /// Starts an empty complete-history oracle for `network`.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            last_projected_block: None,
            live_outputs: BTreeMap::new(),
            spent_outputs: BTreeMap::new(),
            address_history: BTreeMap::new(),
            balance_by_address: BTreeMap::new(),
            utxo_count: 0,
            total_utxo_value_zat: 0,
            utxo_commitment: TransparentUtxoSetCommitment::empty(),
        }
    }

    /// Applies one contiguous canonical block atomically and returns its undo record.
    pub fn apply_block(
        &mut self,
        facts: &CanonicalBlockFacts,
    ) -> Result<WalletReorgUndo, WalletProjectionContractError> {
        let mut candidate = self.clone();
        let undo = candidate.apply_block_in_place(facts)?;
        *self = candidate;
        Ok(undo)
    }

    /// Returns the last projected block, or `None` before height one is applied.
    #[must_use]
    pub const fn last_projected_block(&self) -> Option<BlockId> {
        self.last_projected_block
    }

    /// Finds one currently live output.
    #[must_use]
    pub fn find_live_output(&self, outpoint: TransparentOutPoint) -> Option<&WalletLiveOutput> {
        self.live_outputs.get(&WalletOutpointKey::new(outpoint))
    }

    /// Finds one historical spent output.
    #[must_use]
    pub fn find_spent_output(&self, outpoint: TransparentOutPoint) -> Option<&WalletSpentOutput> {
        self.spent_outputs.get(&WalletOutpointKey::new(outpoint))
    }

    /// Returns one address's current balance, with absent rows represented as zero.
    #[must_use]
    pub fn address_balance(&self, address_script_hash: TransparentAddressScriptHash) -> u64 {
        self.balance_by_address
            .get(&address_script_hash.as_bytes())
            .copied()
            .unwrap_or_default()
    }

    /// Iterates address history in exact durable key order.
    #[must_use]
    pub fn address_history(&self) -> impl ExactSizeIterator<Item = &WalletAddressHistoryEntry> {
        self.address_history.values()
    }

    /// Returns the logical row counts represented by this reference model.
    #[must_use]
    pub fn row_counts(&self) -> WalletProjectionFamilyRowCounts {
        WalletProjectionFamilyRowCounts {
            live_output_count: count_rows(self.live_outputs.len()),
            live_output_by_address_count: count_rows(self.live_outputs.len()),
            spent_output_count: count_rows(self.spent_outputs.len()),
            address_history_count: count_rows(self.address_history.len()),
            address_balance_count: count_rows(self.balance_by_address.len()),
            reorg_undo_count: 0,
        }
    }

    /// Returns the complete current UTXO aggregate.
    #[must_use]
    pub fn utxo_summary(&self) -> WalletUtxoSetSummary {
        WalletUtxoSetSummary {
            utxo_count: self.utxo_count,
            total_value_zat: self.total_utxo_value_zat,
            commitment: self.utxo_commitment.clone(),
        }
    }

    /// Commits every wallet query row in family and key order.
    pub fn projection_digest(
        &self,
    ) -> Result<WalletProjectionDigest, WalletProjectionContractError> {
        let mut hasher = Sha256::new();
        hasher.update(PROJECTION_DIGEST_DOMAIN);
        hasher.update(WALLET_PROJECTION_VALUE_ENCODING_VERSION.to_be_bytes());

        begin_digest_family(&mut hasher, 1, self.live_outputs.len());
        for (key, output) in &self.live_outputs {
            digest_row(&mut hasher, key.as_bytes(), &output.encode_value()?)?;
        }

        let live_output_address_keys: BTreeSet<_> = self
            .live_outputs
            .values()
            .map(WalletAddressLiveOutputKey::new)
            .collect();
        begin_digest_family(&mut hasher, 2, live_output_address_keys.len());
        for address_key in live_output_address_keys {
            digest_row(&mut hasher, address_key.as_bytes(), &[])?;
        }

        begin_digest_family(&mut hasher, 3, self.spent_outputs.len());
        for (key, output) in &self.spent_outputs {
            digest_row(&mut hasher, key.as_bytes(), &output.encode_value()?)?;
        }

        begin_digest_family(&mut hasher, 4, self.address_history.len());
        for (key, entry) in &self.address_history {
            digest_row(&mut hasher, key.as_bytes(), &entry.encode_value())?;
        }

        begin_digest_family(&mut hasher, 5, self.balance_by_address.len());
        for (address_script_hash, balance_zat) in &self.balance_by_address {
            digest_row(&mut hasher, address_script_hash, &balance_zat.to_be_bytes())?;
        }

        Ok(WalletProjectionDigest::from_bytes(hasher.finalize().into()))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the serial oracle keeps one top-to-bottom block transition visible"
    )]
    fn apply_block_in_place(
        &mut self,
        facts: &CanonicalBlockFacts,
    ) -> Result<WalletReorgUndo, WalletProjectionContractError> {
        let block = BlockId::new(facts.block_header.height, facts.block_header.block_hash);
        self.validate_next_block(block, facts.block_header.parent_hash)?;
        let mut created_outpoints = Vec::new();
        let mut spent_outpoints = Vec::new();
        let mut address_history_keys = Vec::new();

        for (transaction_index, transaction) in facts.transactions.iter().enumerate() {
            let tx_index_in_block = u32::try_from(transaction_index)
                .map_err(|_| WalletProjectionContractError::FactIndexOverflow)?;
            let transaction_id = transaction.public_facts.transaction_id;
            let transaction_position =
                WalletTransactionPosition::new(transaction_id, tx_index_in_block, block);
            let mut touched_addresses = BTreeSet::new();

            for (input_position, input) in transaction.transparent_inputs.iter().enumerate() {
                let expected_input_index = u32::try_from(input_position)
                    .map_err(|_| WalletProjectionContractError::FactIndexOverflow)?;
                if input.input_index != expected_input_index {
                    return Err(WalletProjectionContractError::FactIndexMismatch);
                }
                if input.spent_outpoint.is_coinbase_sentinel() {
                    continue;
                }
                let key = WalletOutpointKey::new(input.spent_outpoint);
                if self.spent_outputs.contains_key(&key) {
                    return Err(WalletProjectionContractError::DuplicateSpend);
                }
                let output = self
                    .live_outputs
                    .remove(&key)
                    .ok_or(WalletProjectionContractError::MissingTransparentPredecessor)?;
                self.subtract_live_output(&output)?;
                touched_addresses.insert(output.address_script_hash.as_bytes());
                self.spent_outputs.insert(
                    key,
                    WalletSpentOutput::new(output, transaction_position, input.input_index),
                );
                spent_outpoints.push(key);
            }

            for (output_position, output) in transaction.transparent_outputs.iter().enumerate() {
                let expected_output_index = u32::try_from(output_position)
                    .map_err(|_| WalletProjectionContractError::FactIndexOverflow)?;
                if output.output_index != expected_output_index {
                    return Err(WalletProjectionContractError::FactIndexMismatch);
                }
                let outpoint = TransparentOutPoint::new(transaction_id, output.output_index);
                let key = WalletOutpointKey::new(outpoint);
                if self.live_outputs.contains_key(&key) || self.spent_outputs.contains_key(&key) {
                    return Err(WalletProjectionContractError::DuplicateOutput);
                }
                let live_output = WalletLiveOutput::new(
                    outpoint,
                    output.address_script_hash,
                    output.value_zat,
                    output.script_pub_key.clone(),
                    transaction_position,
                )?;
                self.add_live_output(&live_output)?;
                touched_addresses.insert(output.address_script_hash.as_bytes());
                self.live_outputs.insert(key, live_output);
                created_outpoints.push(key);
            }

            for address_bytes in touched_addresses {
                let address_script_hash = TransparentAddressScriptHash::from_bytes(address_bytes);
                let key = WalletAddressHistoryKey::new(
                    address_script_hash,
                    block.height,
                    tx_index_in_block,
                );
                self.address_history.insert(
                    key,
                    WalletAddressHistoryEntry::new(key, transaction_id, block.hash),
                );
                address_history_keys.push(key);
            }
        }

        self.last_projected_block = Some(block);
        Ok(WalletReorgUndo {
            block,
            created_outpoints,
            spent_outpoints,
            address_history_keys,
        })
    }

    fn validate_next_block(
        &self,
        block: BlockId,
        parent_hash: BlockHash,
    ) -> Result<(), WalletProjectionContractError> {
        match self.last_projected_block {
            None if block.height == BlockHeight::new(1) => Ok(()),
            Some(previous)
                if previous.height.next() == Some(block.height) && previous.hash == parent_hash =>
            {
                Ok(())
            }
            None | Some(_) => Err(WalletProjectionContractError::NonContiguousBlock),
        }
    }

    fn add_live_output(
        &mut self,
        output: &WalletLiveOutput,
    ) -> Result<(), WalletProjectionContractError> {
        let address = output.address_script_hash.as_bytes();
        let balance = self.balance_by_address.entry(address).or_default();
        *balance = balance
            .checked_add(output.value_zat)
            .ok_or(WalletProjectionContractError::AddressBalanceOverflow)?;
        self.utxo_count = self
            .utxo_count
            .checked_add(1)
            .ok_or(WalletProjectionContractError::UtxoCountOverflow)?;
        self.total_utxo_value_zat = self
            .total_utxo_value_zat
            .checked_add(output.value_zat)
            .ok_or(WalletProjectionContractError::UtxoValueOverflow)?;
        self.utxo_commitment
            .insert(&output.commitment_element(self.network));
        Ok(())
    }

    fn subtract_live_output(
        &mut self,
        output: &WalletLiveOutput,
    ) -> Result<(), WalletProjectionContractError> {
        let address = output.address_script_hash.as_bytes();
        let balance = self
            .balance_by_address
            .get_mut(&address)
            .ok_or(WalletProjectionContractError::AddressBalanceUnderflow)?;
        *balance = balance
            .checked_sub(output.value_zat)
            .ok_or(WalletProjectionContractError::AddressBalanceUnderflow)?;
        if *balance == 0 {
            self.balance_by_address.remove(&address);
        }
        self.utxo_count = self
            .utxo_count
            .checked_sub(1)
            .ok_or(WalletProjectionContractError::UtxoCountUnderflow)?;
        self.total_utxo_value_zat = self
            .total_utxo_value_zat
            .checked_sub(output.value_zat)
            .ok_or(WalletProjectionContractError::UtxoValueUnderflow)?;
        self.utxo_commitment
            .subtract(&output.commitment_element(self.network));
        Ok(())
    }
}

fn count_rows(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn begin_digest_family(hasher: &mut Sha256, family_tag: u8, row_count: usize) {
    hasher.update([family_tag]);
    hasher.update(count_rows(row_count).to_be_bytes());
}

fn digest_row(
    hasher: &mut Sha256,
    key: &[u8],
    row_bytes: &[u8],
) -> Result<(), WalletProjectionContractError> {
    let key_len = encoded_len(key.len(), "projection digest key")?;
    let value_len = encoded_len(row_bytes.len(), "projection digest value")?;
    hasher.update(key_len.to_be_bytes());
    hasher.update(key);
    hasher.update(value_len.to_be_bytes());
    hasher.update(row_bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{
        BlockHeaderArtifact, CanonicalTransactionFacts, LockTime, PrivacyShape,
        SerializedBytesDigest, TransactionComponentCounts, TransactionId,
        TransactionIntrinsicValueBalances, TransactionPublicFacts, TransactionVersion,
        TransparentInputFact, TransparentOutputFact,
    };

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one end-to-end oracle test keeps the create, spend, and atomic refusal proof together"
    )]
    fn serial_oracle_projects_create_then_spend() {
        let address_one = TransparentAddressScriptHash::from_bytes([0xa1; 32]);
        let address_two = TransparentAddressScriptHash::from_bytes([0xa2; 32]);
        let transaction_one = TransactionId::from_bytes([0xb1; 32]);
        let transaction_two = TransactionId::from_bytes([0xb2; 32]);
        let outpoint_one = TransparentOutPoint::new(transaction_one, 0);
        let outpoint_two = TransparentOutPoint::new(transaction_two, 0);
        let block_one = block_facts(
            1,
            [0x00; 32],
            [0xc1; 32],
            transaction_facts(
                transaction_one,
                Vec::new(),
                vec![TransparentOutputFact::new(0, 7, [0x51], address_one)],
            ),
        );
        let block_two = block_facts(
            2,
            [0xc1; 32],
            [0xc2; 32],
            transaction_facts(
                transaction_two,
                vec![TransparentInputFact::new(0, outpoint_one)],
                vec![TransparentOutputFact::new(0, 4, [0x52], address_two)],
            ),
        );

        let mut oracle = WalletProjectionSerialOracle::new(Network::ZcashRegtest);
        let first_undo = oracle
            .apply_block(&block_one)
            .unwrap_or_else(|error| unreachable!("valid first block: {error}"));
        let second_undo = oracle
            .apply_block(&block_two)
            .unwrap_or_else(|error| unreachable!("valid second block: {error}"));

        assert_eq!(first_undo.created_outpoints.len(), 1);
        assert_eq!(second_undo.created_outpoints.len(), 1);
        assert_eq!(second_undo.spent_outpoints.len(), 1);
        assert_eq!(second_undo.address_history_keys.len(), 2);
        assert!(oracle.find_live_output(outpoint_one).is_none());
        assert!(oracle.find_spent_output(outpoint_one).is_some());
        assert!(oracle.find_live_output(outpoint_two).is_some());
        assert_eq!(oracle.address_balance(address_one), 0);
        assert_eq!(oracle.address_balance(address_two), 4);
        assert_eq!(oracle.address_history().len(), 3);
        assert_eq!(
            oracle.row_counts(),
            WalletProjectionFamilyRowCounts {
                live_output_count: 1,
                live_output_by_address_count: 1,
                spent_output_count: 1,
                address_history_count: 3,
                address_balance_count: 1,
                reorg_undo_count: 0,
            }
        );
        assert_eq!(oracle.utxo_summary().utxo_count, 1);
        assert_eq!(oracle.utxo_summary().total_value_zat, 4);

        let digest_before_error = oracle
            .projection_digest()
            .unwrap_or_else(|error| unreachable!("valid oracle digest: {error}"));
        let missing_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0xdd; 32]), 0);
        let invalid_block = block_facts(
            3,
            [0xc2; 32],
            [0xc3; 32],
            transaction_facts(
                TransactionId::from_bytes([0xb3; 32]),
                vec![TransparentInputFact::new(0, missing_outpoint)],
                Vec::new(),
            ),
        );
        assert_eq!(
            oracle.apply_block(&invalid_block),
            Err(WalletProjectionContractError::MissingTransparentPredecessor)
        );
        assert_eq!(
            oracle.last_projected_block(),
            Some(block_two.block_header.into_header_info().block_id)
        );
        assert_eq!(
            oracle
                .projection_digest()
                .unwrap_or_else(|error| unreachable!("valid oracle digest: {error}")),
            digest_before_error
        );
    }

    #[test]

    fn serial_oracle_digests_address_index_in_address_key_order() {
        let first_transaction = TransactionId::from_bytes([0x01; 32]);
        let second_transaction = TransactionId::from_bytes([0x02; 32]);
        let block = block_facts_with_transactions(
            1,
            [0x00; 32],
            [0xc1; 32],
            vec![
                transaction_facts(
                    first_transaction,
                    Vec::new(),
                    vec![TransparentOutputFact::new(
                        0,
                        1,
                        [0x51],
                        TransparentAddressScriptHash::from_bytes([0xff; 32]),
                    )],
                ),
                transaction_facts(
                    second_transaction,
                    Vec::new(),
                    vec![TransparentOutputFact::new(
                        0,
                        2,
                        [0x52],
                        TransparentAddressScriptHash::from_bytes([0x00; 32]),
                    )],
                ),
            ],
        );
        let mut oracle = WalletProjectionSerialOracle::new(Network::ZcashRegtest);
        oracle
            .apply_block(&block)
            .unwrap_or_else(|error| unreachable!("valid ordering fixture: {error}"));

        let outpoint_ordered_address_keys: Vec<_> = oracle
            .live_outputs
            .values()
            .map(WalletAddressLiveOutputKey::new)
            .collect();
        let address_ordered_keys: Vec<_> = outpoint_ordered_address_keys
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_ne!(outpoint_ordered_address_keys, address_ordered_keys);
        assert_eq!(
            hex::encode(
                oracle
                    .projection_digest()
                    .unwrap_or_else(|error| unreachable!("valid ordering digest: {error}"))
                    .as_bytes()
            ),
            "74b4a85bcf65e55859e0a4dc00237e2662098084ef523de75cec7164a0ce4a10"
        );
    }

    fn block_facts(
        height: u32,
        parent_hash: [u8; 32],
        block_hash: [u8; 32],
        transaction: CanonicalTransactionFacts,
    ) -> CanonicalBlockFacts {
        block_facts_with_transactions(height, parent_hash, block_hash, vec![transaction])
    }

    fn block_facts_with_transactions(
        height: u32,
        parent_hash: [u8; 32],
        block_hash: [u8; 32],
        transactions: Vec<CanonicalTransactionFacts>,
    ) -> CanonicalBlockFacts {
        CanonicalBlockFacts {
            block_header: BlockHeaderArtifact::new(
                BlockHeight::new(height),
                BlockHash::from_bytes(block_hash),
                BlockHash::from_bytes(parent_hash),
                [0; 32],
                [0; 32],
                0,
                0,
                [0; 32],
                0,
                0,
            ),
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[0]),
            transactions,
        }
    }

    fn transaction_facts(
        transaction_id: TransactionId,
        transparent_inputs: Vec<TransparentInputFact>,
        transparent_outputs: Vec<TransparentOutputFact>,
    ) -> CanonicalTransactionFacts {
        CanonicalTransactionFacts {
            public_facts: TransactionPublicFacts {
                transaction_id,
                auth_digest: None,
                wtxid: None,
                version: TransactionVersion::Unsupported {
                    effective_version: 0,
                    version_group_id: None,
                },
                consensus_branch_id: None,
                lock_time: LockTime::Unlocked,
                expiry_height: None,
                size_bytes: 0,
                counts: TransactionComponentCounts::EMPTY,
                orchard_value_balance_zat: None,
                orchard_anchor: None,
                ironwood_value_balance_zat: None,
                privacy_shape: PrivacyShape::Unclassified,
                is_coinbase: false,
                unsupported_sections: Vec::new(),
            },
            serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&[0]),
            intrinsic_value_balances: TransactionIntrinsicValueBalances::default(),
            transparent_inputs,
            transparent_outputs,
        }
    }
}
