//! Deterministic source-block artifact builders.

use std::fmt;

use prost::Message;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use zebra_chain::{
    block::Block as ZebraBlock,
    serialization::{ZcashDeserializeInto, ZcashSerialize},
    transaction::Transaction as ZebraTransaction,
    transparent::Input as ZebraTransparentInput,
};
use zinder_core::{
    BlockArtifact, ChainTipMetadata, CompactBlockArtifact, ShieldedProtocol, TransactionArtifact,
    TransactionId, TransparentAddressScriptHash, TransparentAddressUtxoArtifact,
    TransparentOutPoint, TransparentUtxoSpendArtifact,
};
use zinder_proto::compat::lightwalletd::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend,
    CompactTx, CompactTxIn, TxOut as CompactTxOut,
};
use zinder_source::SourceBlock;

const LIGHTWALLETD_COMPACT_BLOCK_PROTO_VERSION: u32 = 1;
const COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN: usize = 52;
const SAPLING_TREE_POOL: &str = "sapling";
const ORCHARD_TREE_POOL: &str = "orchard";

/// Error returned while deriving canonical artifacts from source blocks.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ArtifactDeriveError {
    /// Source block payload is empty.
    #[error("source block payload is empty")]
    EmptySourcePayload,

    /// Zebra-chain consensus block parse failed.
    #[error("zebra-chain block parse failed: {source}")]
    BlockParseFailed {
        /// Underlying parse error.
        #[source]
        source: zebra_chain::serialization::SerializationError,
    },

    /// Parsed block is missing the coinbase height.
    #[error("parsed block is missing coinbase height")]
    ParsedBlockMissingCoinbaseHeight,

    /// Parsed block field disagrees with the source-supplied value.
    #[error("parsed {field} {actual} does not match source {field} {expected}")]
    SourceBlockMismatch {
        /// Block identity field that disagrees.
        field: BlockMismatchField,
        /// Source-supplied value.
        expected: String,
        /// Parsed value.
        actual: String,
    },

    /// Commitment-tree size advanced past `u32::MAX`.
    #[error("{protocol:?} commitment tree size overflowed u32")]
    CommitmentTreeOverflow {
        /// Protocol whose tree overflowed.
        protocol: ShieldedProtocol,
    },

    /// Built compact-block tree sizes do not match node-observed tree sizes.
    #[error(
        "compact block tree sizes sapling={expected_sapling} orchard={expected_orchard} do not match observed sapling={observed_sapling} orchard={observed_orchard}"
    )]
    ObservedTreeSizeMismatch {
        /// Expected Sapling tree size after this block.
        expected_sapling: u32,
        /// Expected Orchard tree size after this block.
        expected_orchard: u32,
        /// Observed Sapling tree size from node-supplied tree state.
        observed_sapling: u32,
        /// Observed Orchard tree size from node-supplied tree state.
        observed_orchard: u32,
    },

    /// Node-supplied tree-state payload is not valid JSON.
    #[error("tree-state JSON parse failed: {source}")]
    TreeStateParseFailed {
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// Tree-state pool size does not fit a u32.
    #[error("tree-state {pool} size does not fit u32")]
    TreeStateSizeOverflow {
        /// Pool name from the node tree-state payload.
        pool: &'static str,
    },

    /// Tree-state pool size string failed to parse as u32.
    #[error("tree-state {pool} size string failed to parse as u32: {source}")]
    TreeStateSizeStringInvalid {
        /// Pool name from the node tree-state payload.
        pool: &'static str,
        /// Underlying integer parse error.
        #[source]
        source: std::num::ParseIntError,
    },

    /// Tree-state pool size is neither an integer nor an integer string.
    #[error("tree-state {pool} size must be an integer or integer string")]
    TreeStateSizeShape {
        /// Pool name from the node tree-state payload.
        pool: &'static str,
    },

    /// Node-supplied tree-state payload has no recognized pool sizes.
    #[error("tree-state payload does not contain recognized Sapling or Orchard pool sizes")]
    UnrecognizedTreeStateShape,

    /// A counted field cannot be encoded as u32.
    #[error("{field} does not fit u32")]
    CountOverflow {
        /// Counted field name.
        field: &'static str,
    },

    /// Compact note ciphertext is shorter than the lightwalletd prefix.
    #[error("compact note ciphertext is shorter than {COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN} bytes")]
    CompactCiphertextTooShort,

    /// Compact transaction id has the wrong length.
    #[error("compact transaction id is {byte_count} bytes, expected 32")]
    CompactTransactionIdMalformed {
        /// Observed byte count.
        byte_count: usize,
    },

    /// Transparent input previous transaction id has the wrong length.
    #[error("transparent prevout transaction id is {byte_count} bytes, expected 32")]
    TransparentPrevoutTransactionIdMalformed {
        /// Observed byte count.
        byte_count: usize,
    },

    /// Transparent output index cannot be represented as `u32`.
    #[error("transparent output index does not fit u32")]
    TransparentOutputIndexOverflow,

    /// Round-tripping a parsed transaction back to canonical bytes failed.
    #[error("transaction serialization failed: {source}")]
    TransactionSerializationFailed {
        /// Underlying serialization error.
        #[source]
        source: std::io::Error,
    },

    /// Serializing the parsed block header back to canonical bytes failed.
    #[error("block header serialization failed: {source}")]
    BlockHeaderSerializationFailed {
        /// Underlying serialization error.
        #[source]
        source: std::io::Error,
    },
}

/// Block identity field whose source/parsed values disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BlockMismatchField {
    /// Block height.
    Height,
    /// Block hash.
    Hash,
    /// Parent block hash.
    ParentHash,
    /// Block time.
    Time,
}

impl fmt::Display for BlockMismatchField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Height => formatter.write_str("block height"),
            Self::Hash => formatter.write_str("block hash"),
            Self::ParentHash => formatter.write_str("parent hash"),
            Self::Time => formatter.write_str("block time"),
        }
    }
}

/// Derives the durable full-block artifact for one node source block.
pub fn derive_block_artifact(
    source_block: &SourceBlock,
) -> Result<BlockArtifact, ArtifactDeriveError> {
    validate_source_block_payload(source_block)?;

    Ok(BlockArtifact::new(
        source_block.height,
        source_block.hash,
        source_block.parent_hash,
        source_block.raw_block_bytes.clone(),
    ))
}

/// Derives the wallet-oriented compact block artifact for one node source block.
pub fn derive_compact_block_artifact(
    source_block: &SourceBlock,
) -> Result<CompactBlockArtifact, ArtifactDeriveError> {
    let mut artifact_builder = CompactBlockArtifactBuilder::default();
    artifact_builder
        .build(source_block)
        .map(|artifact_build| artifact_build.compact_block)
}

/// Derives one [`TransactionArtifact`] per transaction in the source block.
///
/// Each artifact carries the canonical transaction id, the containing block
/// height and hash, and the transaction's serialized bytes obtained by
/// round-tripping through `zebra_chain`'s `ZcashSerialize`. Coinbase and
/// non-coinbase transactions are emitted in block order.
pub fn derive_transaction_artifacts(
    source_block: &SourceBlock,
) -> Result<Vec<TransactionArtifact>, ArtifactDeriveError> {
    validate_source_block_payload(source_block)?;
    let parsed_block = parse_source_block(source_block)?;
    validate_parsed_block_identity(&parsed_block, source_block)?;
    derive_transaction_artifacts_from_parsed(&parsed_block, source_block)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CommitmentTreeSizes {
    sapling: u32,
    orchard: u32,
}

impl CommitmentTreeSizes {
    const fn from_tip_metadata(tip_metadata: ChainTipMetadata) -> Self {
        Self {
            sapling: tip_metadata.sapling_commitment_tree_size,
            orchard: tip_metadata.orchard_commitment_tree_size,
        }
    }

    fn checked_add(self, additions: Self) -> Result<Self, ArtifactDeriveError> {
        let sapling = self.sapling.checked_add(additions.sapling).ok_or(
            ArtifactDeriveError::CommitmentTreeOverflow {
                protocol: ShieldedProtocol::Sapling,
            },
        )?;
        let orchard = self.orchard.checked_add(additions.orchard).ok_or(
            ArtifactDeriveError::CommitmentTreeOverflow {
                protocol: ShieldedProtocol::Orchard,
            },
        )?;

        Ok(Self { sapling, orchard })
    }

    fn chain_metadata(self) -> ChainMetadata {
        ChainMetadata {
            sapling_commitment_tree_size: self.sapling,
            orchard_commitment_tree_size: self.orchard,
        }
    }

    const fn tip_metadata(self) -> ChainTipMetadata {
        ChainTipMetadata::new(self.sapling, self.orchard)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObservedCommitmentTreeSizes {
    sapling: Option<u32>,
    orchard: Option<u32>,
}

impl ObservedCommitmentTreeSizes {
    const fn has_observation(self) -> bool {
        self.sapling.is_some() || self.orchard.is_some()
    }
}

#[derive(Debug)]
pub(crate) struct CompactBlockArtifactBuild {
    pub(crate) compact_block: CompactBlockArtifact,
    pub(crate) tip_metadata: ChainTipMetadata,
    pub(crate) transactions: Vec<TransactionArtifact>,
    pub(crate) transparent_address_utxos: Vec<TransparentAddressUtxoArtifact>,
    pub(crate) transparent_utxo_spends: Vec<TransparentUtxoSpendArtifact>,
}

#[derive(Debug, Default)]
pub(crate) struct CompactBlockArtifactBuilder {
    current_tree_sizes: CommitmentTreeSizes,
}

impl CompactBlockArtifactBuilder {
    pub(crate) const fn with_initial_tip_metadata(initial_tip_metadata: ChainTipMetadata) -> Self {
        Self {
            current_tree_sizes: CommitmentTreeSizes::from_tip_metadata(initial_tip_metadata),
        }
    }

    pub(crate) fn build(
        &mut self,
        source_block: &SourceBlock,
    ) -> Result<CompactBlockArtifactBuild, ArtifactDeriveError> {
        let artifact_build = build_compact_block_artifact(source_block, self.current_tree_sizes)?;
        self.current_tree_sizes =
            CommitmentTreeSizes::from_tip_metadata(artifact_build.tip_metadata);

        Ok(artifact_build)
    }
}

fn build_compact_block_artifact(
    source_block: &SourceBlock,
    initial_tree_sizes: CommitmentTreeSizes,
) -> Result<CompactBlockArtifactBuild, ArtifactDeriveError> {
    validate_source_block_payload(source_block)?;
    let parsed_block = parse_source_block(source_block)?;
    validate_parsed_block_identity(&parsed_block, source_block)?;

    let (compact_transactions, tree_size_additions) = compact_transactions(&parsed_block)?;
    let (transparent_address_utxos, transparent_utxo_spends) =
        transparent_utxo_artifacts(source_block, &compact_transactions)?;
    let final_tree_sizes = initial_tree_sizes.checked_add(tree_size_additions)?;
    validate_observed_tree_sizes(source_block, final_tree_sizes)?;

    let transactions = derive_transaction_artifacts_from_parsed(&parsed_block, source_block)?;
    let block_header_bytes = parsed_block
        .header
        .zcash_serialize_to_vec()
        .map_err(|source| ArtifactDeriveError::BlockHeaderSerializationFailed { source })?;

    let compact_block = CompactBlock {
        proto_version: LIGHTWALLETD_COMPACT_BLOCK_PROTO_VERSION,
        height: u64::from(source_block.height.value()),
        hash: source_block.hash.as_bytes().to_vec(),
        prev_hash: source_block.parent_hash.as_bytes().to_vec(),
        time: source_block.block_time_seconds,
        header: block_header_bytes,
        vtx: compact_transactions,
        chain_metadata: Some(final_tree_sizes.chain_metadata()),
    };

    Ok(CompactBlockArtifactBuild {
        compact_block: CompactBlockArtifact::new(
            source_block.height,
            source_block.hash,
            compact_block.encode_to_vec(),
        ),
        tip_metadata: final_tree_sizes.tip_metadata(),
        transactions,
        transparent_address_utxos,
        transparent_utxo_spends,
    })
}

fn derive_transaction_artifacts_from_parsed(
    parsed_block: &ZebraBlock,
    source_block: &SourceBlock,
) -> Result<Vec<TransactionArtifact>, ArtifactDeriveError> {
    parsed_block
        .transactions
        .iter()
        .map(|transaction| {
            let payload_bytes = transaction
                .zcash_serialize_to_vec()
                .map_err(|source| ArtifactDeriveError::TransactionSerializationFailed { source })?;
            Ok(TransactionArtifact::new(
                TransactionId::from_bytes(transaction.hash().0),
                source_block.height,
                source_block.hash,
                payload_bytes,
            ))
        })
        .collect()
}

fn validate_source_block_payload(source_block: &SourceBlock) -> Result<(), ArtifactDeriveError> {
    if source_block.raw_block_bytes.is_empty() {
        return Err(ArtifactDeriveError::EmptySourcePayload);
    }

    Ok(())
}

fn parse_source_block(source_block: &SourceBlock) -> Result<ZebraBlock, ArtifactDeriveError> {
    source_block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into::<ZebraBlock>()
        .map_err(|source| ArtifactDeriveError::BlockParseFailed { source })
}

fn validate_parsed_block_identity(
    parsed_block: &ZebraBlock,
    source_block: &SourceBlock,
) -> Result<(), ArtifactDeriveError> {
    let parsed_height = parsed_block
        .coinbase_height()
        .ok_or(ArtifactDeriveError::ParsedBlockMissingCoinbaseHeight)?
        .0;
    if parsed_height != source_block.height.value() {
        return Err(ArtifactDeriveError::SourceBlockMismatch {
            field: BlockMismatchField::Height,
            expected: source_block.height.value().to_string(),
            actual: parsed_height.to_string(),
        });
    }

    let parsed_hash = parsed_block.hash().0;
    if parsed_hash != source_block.hash.as_bytes() {
        return Err(ArtifactDeriveError::SourceBlockMismatch {
            field: BlockMismatchField::Hash,
            expected: format_block_hash(source_block.hash.as_bytes()),
            actual: format_block_hash(parsed_hash),
        });
    }

    let parsed_parent_hash = parsed_block.header.previous_block_hash.0;
    if parsed_parent_hash != source_block.parent_hash.as_bytes() {
        return Err(ArtifactDeriveError::SourceBlockMismatch {
            field: BlockMismatchField::ParentHash,
            expected: format_block_hash(source_block.parent_hash.as_bytes()),
            actual: format_block_hash(parsed_parent_hash),
        });
    }

    let parsed_time = u32::try_from(parsed_block.header.time.timestamp()).map_err(|_| {
        ArtifactDeriveError::CountOverflow {
            field: "parsed block time",
        }
    })?;
    if parsed_time != source_block.block_time_seconds {
        return Err(ArtifactDeriveError::SourceBlockMismatch {
            field: BlockMismatchField::Time,
            expected: source_block.block_time_seconds.to_string(),
            actual: parsed_time.to_string(),
        });
    }

    Ok(())
}

fn compact_transactions(
    parsed_block: &ZebraBlock,
) -> Result<(Vec<CompactTx>, CommitmentTreeSizes), ArtifactDeriveError> {
    let mut compact_transactions = Vec::new();
    let mut tree_size_additions = CommitmentTreeSizes::default();

    for (transaction_index, transaction) in parsed_block.transactions.iter().enumerate() {
        let compact_transaction = compact_transaction(
            u64::try_from(transaction_index).map_err(|_| ArtifactDeriveError::CountOverflow {
                field: "transaction index",
            })?,
            transaction.as_ref(),
        )?;
        tree_size_additions = tree_size_additions.checked_add(CommitmentTreeSizes {
            sapling: count_to_u32(compact_transaction.outputs.len(), "Sapling output count")?,
            orchard: count_to_u32(compact_transaction.actions.len(), "Orchard action count")?,
        })?;

        if compact_transaction_has_payload(&compact_transaction) {
            compact_transactions.push(compact_transaction);
        }
    }

    Ok((compact_transactions, tree_size_additions))
}

fn compact_transaction(
    index: u64,
    transaction: &ZebraTransaction,
) -> Result<CompactTx, ArtifactDeriveError> {
    Ok(CompactTx {
        index,
        txid: transaction.hash().0.to_vec(),
        fee: 0,
        spends: compact_sapling_spends(transaction),
        outputs: compact_sapling_outputs(transaction)?,
        actions: compact_orchard_actions(transaction)?,
        vin: compact_transparent_inputs(transaction),
        vout: compact_transparent_outputs(transaction),
    })
}

fn compact_sapling_spends(transaction: &ZebraTransaction) -> Vec<CompactSaplingSpend> {
    transaction
        .sapling_spends_per_anchor()
        .map(|spend| CompactSaplingSpend {
            nf: <[u8; 32]>::from(spend.nullifier).to_vec(),
        })
        .collect()
}

fn compact_sapling_outputs(
    transaction: &ZebraTransaction,
) -> Result<Vec<CompactSaplingOutput>, ArtifactDeriveError> {
    transaction
        .sapling_outputs()
        .map(|output| {
            let enc_ciphertext: [u8; 580] = output.enc_ciphertext.into();
            Ok(CompactSaplingOutput {
                cmu: output.cm_u.to_bytes().to_vec(),
                ephemeral_key: <[u8; 32]>::from(output.ephemeral_key).to_vec(),
                ciphertext: compact_note_ciphertext_prefix(&enc_ciphertext)?,
            })
        })
        .collect()
}

fn compact_orchard_actions(
    transaction: &ZebraTransaction,
) -> Result<Vec<CompactOrchardAction>, ArtifactDeriveError> {
    transaction
        .orchard_actions()
        .map(|action| {
            let enc_ciphertext: [u8; 580] = action.enc_ciphertext.into();
            Ok(CompactOrchardAction {
                nullifier: <[u8; 32]>::from(action.nullifier).to_vec(),
                cmx: <[u8; 32]>::from(action.cm_x).to_vec(),
                ephemeral_key: <[u8; 32]>::from(action.ephemeral_key).to_vec(),
                ciphertext: compact_note_ciphertext_prefix(&enc_ciphertext)?,
            })
        })
        .collect()
}

fn compact_note_ciphertext_prefix(ciphertext: &[u8]) -> Result<Vec<u8>, ArtifactDeriveError> {
    ciphertext
        .get(..COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN)
        .map(<[u8]>::to_vec)
        .ok_or(ArtifactDeriveError::CompactCiphertextTooShort)
}

fn compact_transparent_inputs(transaction: &ZebraTransaction) -> Vec<CompactTxIn> {
    transaction
        .inputs()
        .iter()
        .filter_map(|input| match input {
            ZebraTransparentInput::PrevOut { outpoint, .. } => Some(CompactTxIn {
                prevout_txid: outpoint.hash.0.to_vec(),
                prevout_index: outpoint.index,
            }),
            ZebraTransparentInput::Coinbase { .. } => None,
        })
        .collect()
}

fn compact_transparent_outputs(transaction: &ZebraTransaction) -> Vec<CompactTxOut> {
    transaction
        .outputs()
        .iter()
        .map(|output| CompactTxOut {
            value: u64::from(output.value()),
            script_pub_key: output.lock_script.as_raw_bytes().to_vec(),
        })
        .collect()
}

fn transparent_utxo_artifacts(
    source_block: &SourceBlock,
    compact_transactions: &[CompactTx],
) -> Result<
    (
        Vec<TransparentAddressUtxoArtifact>,
        Vec<TransparentUtxoSpendArtifact>,
    ),
    ArtifactDeriveError,
> {
    let mut transparent_address_utxos = Vec::new();
    let mut transparent_utxo_spends = Vec::new();

    for transaction in compact_transactions {
        let transaction_id = transaction_id_from_compact_tx(transaction)?;
        for (output_index, output) in transaction.vout.iter().enumerate() {
            let output_index = u32::try_from(output_index)
                .map_err(|_| ArtifactDeriveError::TransparentOutputIndexOverflow)?;
            transparent_address_utxos.push(TransparentAddressUtxoArtifact::new(
                transparent_address_script_hash(&output.script_pub_key),
                output.script_pub_key.clone(),
                TransparentOutPoint::new(transaction_id, output_index),
                output.value,
                source_block.height,
                source_block.hash,
            ));
        }

        for input in &transaction.vin {
            let previous_transaction_id = previous_transaction_id_from_compact_input(input)?;
            transparent_utxo_spends.push(TransparentUtxoSpendArtifact::new(
                TransparentOutPoint::new(previous_transaction_id, input.prevout_index),
                source_block.height,
                source_block.hash,
            ));
        }
    }

    Ok((transparent_address_utxos, transparent_utxo_spends))
}

fn transparent_address_script_hash(script_pub_key: &[u8]) -> TransparentAddressScriptHash {
    let mut hasher = Sha256::new();
    hasher.update(script_pub_key);
    TransparentAddressScriptHash::from_bytes(hasher.finalize().into())
}

fn transaction_id_from_compact_tx(
    transaction: &CompactTx,
) -> Result<TransactionId, ArtifactDeriveError> {
    let txid_bytes = <[u8; 32]>::try_from(transaction.txid.as_slice()).map_err(|_| {
        ArtifactDeriveError::CompactTransactionIdMalformed {
            byte_count: transaction.txid.len(),
        }
    })?;
    Ok(TransactionId::from_bytes(txid_bytes))
}

fn previous_transaction_id_from_compact_input(
    input: &CompactTxIn,
) -> Result<TransactionId, ArtifactDeriveError> {
    let txid_bytes = <[u8; 32]>::try_from(input.prevout_txid.as_slice()).map_err(|_| {
        ArtifactDeriveError::TransparentPrevoutTransactionIdMalformed {
            byte_count: input.prevout_txid.len(),
        }
    })?;
    Ok(TransactionId::from_bytes(txid_bytes))
}

fn compact_transaction_has_payload(transaction: &CompactTx) -> bool {
    !transaction.spends.is_empty()
        || !transaction.outputs.is_empty()
        || !transaction.actions.is_empty()
        || !transaction.vin.is_empty()
        || !transaction.vout.is_empty()
}

fn validate_observed_tree_sizes(
    source_block: &SourceBlock,
    expected_tree_sizes: CommitmentTreeSizes,
) -> Result<(), ArtifactDeriveError> {
    let Some(observed_tree_sizes) = observed_tree_sizes(source_block)? else {
        return Ok(());
    };

    if let Some(observed_sapling) = observed_tree_sizes.sapling
        && observed_sapling != expected_tree_sizes.sapling
    {
        return Err(ArtifactDeriveError::ObservedTreeSizeMismatch {
            expected_sapling: expected_tree_sizes.sapling,
            expected_orchard: expected_tree_sizes.orchard,
            observed_sapling,
            observed_orchard: observed_tree_sizes
                .orchard
                .unwrap_or(expected_tree_sizes.orchard),
        });
    }

    if let Some(observed_orchard) = observed_tree_sizes.orchard
        && observed_orchard != expected_tree_sizes.orchard
    {
        return Err(ArtifactDeriveError::ObservedTreeSizeMismatch {
            expected_sapling: expected_tree_sizes.sapling,
            expected_orchard: expected_tree_sizes.orchard,
            observed_sapling: observed_tree_sizes
                .sapling
                .unwrap_or(expected_tree_sizes.sapling),
            observed_orchard,
        });
    }

    Ok(())
}

fn observed_tree_sizes(
    source_block: &SourceBlock,
) -> Result<Option<ObservedCommitmentTreeSizes>, ArtifactDeriveError> {
    let Some(tree_state_payload_bytes) = source_block.tree_state_payload_bytes.as_deref() else {
        return Ok(None);
    };

    let tree_state_payload: Value = serde_json::from_slice(tree_state_payload_bytes)
        .map_err(|source| ArtifactDeriveError::TreeStateParseFailed { source })?;
    let sapling = tree_size_observation(&tree_state_payload, SAPLING_TREE_POOL)?;
    let orchard = tree_size_observation(&tree_state_payload, ORCHARD_TREE_POOL)?;
    let observed_tree_sizes = ObservedCommitmentTreeSizes { sapling, orchard };

    if observed_tree_sizes.has_observation() {
        Ok(Some(observed_tree_sizes))
    } else {
        Ok(None)
    }
}

/// Returns the observed commitment-tree size for `pool`, or `None` when the
/// node response lacks the size shape Zinder can interpret.
///
/// Recognized shapes:
/// - `{pool}.commitments.size` (legacy explicit size, used in tests)
/// - `{pool}.size` and `trees.{pool}.size` (alternate explicit shapes)
/// - `{pool}.commitments` is `{}` (Zebra reports an empty pool)
/// - `{pool}.commitments.finalState` is `"000000"` or empty (Zebra v6.0.2's
///   serialized form for an empty Sapling/Orchard commitment tree)
///
/// Non-empty trees without an explicit `size` field return `None`. Validation
/// in [`validate_observed_tree_sizes`] then skips for that pool; the
/// builder's internal output count is the authoritative source.
fn tree_size_observation(
    tree_state_payload: &Value,
    pool: &'static str,
) -> Result<Option<u32>, ArtifactDeriveError> {
    if let Some(size) = explicit_tree_size_field(tree_state_payload, pool)? {
        return Ok(Some(size));
    }

    let Some(commitments) = tree_state_payload
        .get(pool)
        .and_then(|pool_value| pool_value.get("commitments"))
    else {
        return Ok(None);
    };
    let Some(commitments_object) = commitments.as_object() else {
        return Ok(None);
    };

    if commitments_object.is_empty() {
        return Ok(Some(0));
    }
    if let Some(final_state) = commitments_object.get("finalState").and_then(Value::as_str)
        && is_empty_commitment_final_state(final_state)
    {
        return Ok(Some(0));
    }

    Ok(None)
}

fn is_empty_commitment_final_state(final_state: &str) -> bool {
    final_state.is_empty() || final_state == "000000"
}

fn explicit_tree_size_field(
    tree_state_payload: &Value,
    pool: &'static str,
) -> Result<Option<u32>, ArtifactDeriveError> {
    if let Some(field) = [
        tree_state_payload
            .get(pool)
            .and_then(|pool_value| pool_value.get("commitments"))
            .and_then(|commitments| commitments.get("size")),
        tree_state_payload
            .get(pool)
            .and_then(|pool_value| pool_value.get("size")),
        tree_state_payload
            .get("trees")
            .and_then(|trees| trees.get(pool))
            .and_then(|pool_value| pool_value.get("size")),
    ]
    .into_iter()
    .flatten()
    .next()
    {
        return parse_tree_size_field(field, pool).map(Some);
    }

    Ok(None)
}

fn parse_tree_size_field(field: &Value, pool: &'static str) -> Result<u32, ArtifactDeriveError> {
    if let Some(size) = field.as_u64() {
        return u32::try_from(size)
            .map_err(|_| ArtifactDeriveError::TreeStateSizeOverflow { pool });
    }

    if let Some(size) = field.as_str() {
        return size
            .parse::<u32>()
            .map_err(|source| ArtifactDeriveError::TreeStateSizeStringInvalid { pool, source });
    }

    Err(ArtifactDeriveError::TreeStateSizeShape { pool })
}

fn count_to_u32(count: usize, field: &'static str) -> Result<u32, ArtifactDeriveError> {
    u32::try_from(count).map_err(|_| ArtifactDeriveError::CountOverflow { field })
}

fn format_block_hash(bytes: [u8; 32]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use zinder_core::{BlockHash, BlockHeight, Network};
    use zinder_source::SourceBlock;

    use super::{
        ArtifactDeriveError, COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN, ObservedCommitmentTreeSizes,
        compact_note_ciphertext_prefix, observed_tree_sizes,
    };

    #[test]
    fn compact_note_ciphertext_prefix_rejects_short_buffers() -> Result<(), Box<dyn Error>> {
        let short_ciphertext = [0u8; COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN - 1];
        let error = match compact_note_ciphertext_prefix(&short_ciphertext) {
            Ok(prefix) => {
                return Err(format!(
                    "expected short ciphertext failure, got {} bytes",
                    prefix.len()
                )
                .into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ArtifactDeriveError::CompactCiphertextTooShort
        ));
        Ok(())
    }

    #[test]
    fn compact_note_ciphertext_prefix_returns_first_lightwalletd_prefix_bytes()
    -> Result<(), Box<dyn Error>> {
        let ciphertext = [1u8; 580];
        let prefix = compact_note_ciphertext_prefix(&ciphertext)?;

        assert_eq!(prefix, vec![1u8; COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN]);
        Ok(())
    }

    #[test]
    fn observed_tree_sizes_returns_none_when_payload_lacks_pool_shape()
    -> Result<(), ArtifactDeriveError> {
        let observed = observed_tree_sizes(&source_block_with_tree_state_payload(
            br#"{"hash":"block"}"#,
        ))?;
        assert!(observed.is_none());
        Ok(())
    }

    #[test]
    fn observed_tree_sizes_accepts_one_present_pool_size() -> Result<(), Box<dyn Error>> {
        let observed_tree_sizes = observed_tree_sizes(&source_block_with_tree_state_payload(
            br#"{"sapling":{"commitments":{"size":7}}}"#,
        ))?
        .ok_or("expected tree-size observation when sapling.commitments.size is present")?;

        assert_eq!(
            observed_tree_sizes,
            ObservedCommitmentTreeSizes {
                sapling: Some(7),
                orchard: None,
            }
        );
        Ok(())
    }

    #[test]
    fn observed_tree_sizes_treats_zebra_v6_empty_tree_state_as_size_zero()
    -> Result<(), Box<dyn Error>> {
        let observed = observed_tree_sizes(&source_block_with_tree_state_payload(
            br#"{"sapling":{"commitments":{"finalState":"000000"}},"orchard":{"commitments":{}}}"#,
        ))?
        .ok_or("expected tree-size observation when Zebra reports empty pools")?;

        assert_eq!(
            observed,
            ObservedCommitmentTreeSizes {
                sapling: Some(0),
                orchard: Some(0),
            }
        );
        Ok(())
    }

    #[test]
    fn observed_tree_sizes_returns_none_for_non_empty_tree_state_without_explicit_size()
    -> Result<(), ArtifactDeriveError> {
        let observed = observed_tree_sizes(&source_block_with_tree_state_payload(
            br#"{"sapling":{"commitments":{"finalRoot":"deadbeef","finalState":"abc1230102"}}}"#,
        ))?;
        assert!(observed.is_none());
        Ok(())
    }

    #[test]
    fn observed_tree_sizes_does_not_default_unknown_pool_to_zero() -> Result<(), Box<dyn Error>> {
        let observed = observed_tree_sizes(&source_block_with_tree_state_payload(
            br#"{"sapling":{"commitments":{"finalRoot":"deadbeef","finalState":"abc1230102"}},"orchard":{"commitments":{}}}"#,
        ))?
        .ok_or("expected partial tree-size observation when one pool is explicit empty")?;

        assert_eq!(
            observed,
            ObservedCommitmentTreeSizes {
                sapling: None,
                orchard: Some(0),
            }
        );
        Ok(())
    }

    fn source_block_with_tree_state_payload(tree_state_payload_bytes: &[u8]) -> SourceBlock {
        SourceBlock {
            network: Network::ZcashRegtest,
            height: BlockHeight::new(1),
            hash: BlockHash::from_bytes([1; 32]),
            parent_hash: BlockHash::from_bytes([0; 32]),
            block_time_seconds: 0,
            raw_block_bytes: vec![1],
            tree_state_payload_bytes: Some(tree_state_payload_bytes.to_vec()),
        }
    }
}
