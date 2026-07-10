//! Deterministic source-block artifact builders.

use std::fmt;

use prost::Message;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zebra_chain::{
    block::Block as ZebraBlock,
    serialization::{ZcashDeserializeInto, ZcashSerialize},
    transaction::Transaction as ZebraTransaction,
    transparent::Input as ZebraTransparentInput,
};
use zinder_core::{
    BlockBlobArtifact, BlockHash, BlockHeaderArtifact, BlockTransactionIndexArtifact,
    ChainTipMetadata, CompactBlockArtifact, NetworkUpgradeActivations, ShieldedProtocol,
    TransactionBlobArtifact, TransactionFactsArtifact, TransactionId, TransactionLocation,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentOutputArtifact,
    wire::{encode_internal_block_hash, encode_rpc_block_hash_hex},
};

use crate::chain_ingest::BuiltArtifacts;
use zinder_proto::compat::lightwalletd::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend,
    CompactTx, CompactTxIn, TxOut as CompactTxOut,
};
use zinder_source::{
    SourceBlock, block_header_info_from_raw_block_bytes, parse_transaction_public_fact_set,
};
use zinder_store::RawBlobRetention;

const COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN: usize = 52;

/// Policy controlling optional raw-byte blob writes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawBlobPolicy {
    /// Write no raw block or transaction blobs.
    None,
    /// Write raw transaction blobs only.
    Transactions,
    /// Write both raw block blobs and raw transaction blobs.
    All,
}

impl RawBlobPolicy {
    /// Returns the config spelling for this policy.
    #[must_use]
    pub const fn as_kebab_case(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Transactions => "transactions",
            Self::All => "all",
        }
    }

    const fn writes_block_blobs(self) -> bool {
        matches!(self, Self::All)
    }

    const fn writes_transaction_blobs(self) -> bool {
        matches!(self, Self::Transactions | Self::All)
    }

    /// Maps the policy to the reader-facing retention signal the writer
    /// persists for capability discovery.
    #[must_use]
    pub const fn to_retention(self) -> RawBlobRetention {
        match self {
            Self::None => RawBlobRetention::None,
            Self::Transactions => RawBlobRetention::Transactions,
            Self::All => RawBlobRetention::All,
        }
    }
}

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

    /// Zebra-chain consensus transaction parse failed.
    #[error("zebra-chain transaction parse failed: {source}")]
    TransactionParseFailed {
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
    #[error("transparent input prevout transaction id is {byte_count} bytes, expected 32")]
    TransparentOutputTransactionIdMalformed {
        /// Observed byte count.
        byte_count: usize,
    },

    /// Transparent output index cannot be represented as `u32`.
    #[error("transparent output index does not fit u32")]
    TransparentOutputIndexOverflow,

    /// Transaction index cannot be represented as `u32`.
    #[error("transaction index does not fit u32")]
    TransactionIndexOverflow,

    /// Parsing typed block-header facts failed.
    #[error("block header fact parse failed: {reason}")]
    BlockHeaderParseFailed {
        /// Parser failure reason.
        reason: String,
    },

    /// Parsing typed transaction facts failed.
    #[error("transaction fact parse failed: {reason}")]
    TransactionFactsParseFailed {
        /// Parser failure reason.
        reason: String,
    },

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

/// Commitment-tree position (Sapling and Orchard output counts) at one block boundary.
///
/// `derive_block` returns a per-block delta; `finalize_derived_block` folds
/// those deltas into the running position. The same type expresses both
/// shapes, so callers can carry one running offset through a serial loop
/// or seed it from a `ChainTipMetadata` recovered from the store.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommitmentTreeSizes {
    /// Sapling output count contributed by (or accumulated up to) this point.
    pub sapling: u32,
    /// Orchard action count contributed by (or accumulated up to) this point.
    pub orchard: u32,
    /// Ironwood action count contributed by (or accumulated up to) this point.
    pub ironwood: u32,
}

impl CommitmentTreeSizes {
    /// Seeds a running offset from chain-tip metadata recovered from the
    /// store.
    #[must_use]
    pub const fn from_tip_metadata(tip_metadata: ChainTipMetadata) -> Self {
        Self {
            sapling: tip_metadata.sapling_commitment_tree_size,
            orchard: tip_metadata.orchard_commitment_tree_size,
            ironwood: tip_metadata.ironwood_commitment_tree_size,
        }
    }

    /// Sums two positions; errors if any pool overflows `u32`.
    pub fn checked_add(self, additions: Self) -> Result<Self, ArtifactDeriveError> {
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
        let ironwood = self.ironwood.checked_add(additions.ironwood).ok_or(
            ArtifactDeriveError::CountOverflow {
                field: "Ironwood commitment tree size",
            },
        )?;

        Ok(Self {
            sapling,
            orchard,
            ironwood,
        })
    }

    /// Lowers to the lightwalletd `ChainMetadata` wire shape.
    #[must_use]
    pub const fn chain_metadata(self) -> ChainMetadata {
        ChainMetadata {
            sapling_commitment_tree_size: self.sapling,
            orchard_commitment_tree_size: self.orchard,
            ironwood_commitment_tree_size: self.ironwood,
        }
    }

    /// Lowers to the canonical `ChainTipMetadata` stored alongside each
    /// chain epoch.
    #[must_use]
    pub const fn tip_metadata(self) -> ChainTipMetadata {
        ChainTipMetadata::new(self.sapling, self.orchard, self.ironwood)
    }
}

/// Parallel-safe derivation output for one source block.
///
/// `derive_block` populates every field that depends only on the block
/// content. `finalize_derived_block` stamps the position-dependent fields
/// (the `chain_metadata` inside `partial_compact_block` and the
/// `tip_metadata` in the returned `BuiltArtifacts`) after folding
/// `tree_size_additions` into the running commitment-tree position.
#[derive(Debug)]
pub struct DerivedBlockArtifacts {
    /// Canonical block-header facts.
    pub block_header: BlockHeaderArtifact,
    /// Optional raw block blob.
    pub block_blob: Option<BlockBlobArtifact>,
    /// Lightwalletd compact block with `chain_metadata = None`; the
    /// serial folder stamps the final tree-size position before encoding.
    pub partial_compact_block: CompactBlock,
    /// Commitment-tree-size delta this block contributes.
    pub tree_size_additions: CommitmentTreeSizes,
    /// Block-local transaction id index rows.
    pub block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    /// Transaction location rows.
    pub transaction_locations: Vec<TransactionLocation>,
    /// Per-transaction public facts.
    pub transaction_facts: Vec<TransactionFactsArtifact>,
    /// Optional raw transaction blobs.
    pub transaction_blobs: Vec<TransactionBlobArtifact>,
    /// Transparent-output artifacts for this block. One per transparent
    /// output; writer, derive, and query paths resolve spent outputs from
    /// these rows without re-fetching the producing transaction, and the
    /// store derives the address-output projection rows from them.
    pub transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
}

/// Parallel-safe derivation: parse the source block and build every artifact
/// that does not depend on the block's position in the running chain state.
///
/// The output's `partial_compact_block.chain_metadata` is left `None`;
/// [`finalize_derived_block`] stamps the final commitment-tree position
/// before encoding the proto and validating against any source-supplied
/// observation.
pub fn derive_block(
    source_block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
) -> Result<DerivedBlockArtifacts, ArtifactDeriveError> {
    derive_block_with_raw_blob_policy(source_block, activations, RawBlobPolicy::None)
}

/// Parallel-safe derivation with an explicit optional raw-blob policy.
pub fn derive_block_with_raw_blob_policy(
    source_block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
    raw_blob_policy: RawBlobPolicy,
) -> Result<DerivedBlockArtifacts, ArtifactDeriveError> {
    validate_source_block_payload(source_block)?;
    let parsed_block = parse_source_block(source_block)?;
    validate_parsed_block_identity(&parsed_block, source_block)?;
    let (compact_transactions, tree_size_additions) = compact_transactions(&parsed_block)?;
    let transparent_outputs_by_outpoint =
        transparent_output_artifacts(source_block, &compact_transactions)?;
    let transaction_artifacts = derive_transaction_artifacts_from_parsed(
        &parsed_block,
        source_block,
        activations,
        raw_blob_policy,
    )?;
    let block_header_bytes = parsed_block
        .header
        .zcash_serialize_to_vec()
        .map_err(|source| ArtifactDeriveError::BlockHeaderSerializationFailed { source })?;
    let block_header = BlockHeaderArtifact::from_header_info_with_block_size(
        block_header_info_from_raw_block_bytes(source_block.height, &source_block.raw_block_bytes)
            .map_err(|source| ArtifactDeriveError::BlockHeaderParseFailed {
                reason: source.to_string(),
            })?,
        usize_to_u64_saturating(source_block.raw_block_bytes.len()),
    );
    let block_blob = if raw_blob_policy.writes_block_blobs() {
        Some(BlockBlobArtifact::new(
            source_block.height,
            source_block.hash,
            source_block.parent_hash,
            source_block.raw_block_bytes.clone(),
        ))
    } else {
        record_raw_blob_disabled("block_blob", 1);
        None
    };
    let partial_compact_block = CompactBlock {
        height: u64::from(source_block.height.value()),
        hash: encode_internal_block_hash(source_block.hash).to_vec(),
        prev_hash: encode_internal_block_hash(source_block.parent_hash).to_vec(),
        time: source_block.block_time_seconds,
        header: block_header_bytes,
        vtx: compact_transactions,
        chain_metadata: None,
    };

    Ok(DerivedBlockArtifacts {
        block_header,
        block_blob,
        partial_compact_block,
        tree_size_additions,
        block_transaction_index: transaction_artifacts.block_transaction_index,
        transaction_locations: transaction_artifacts.transaction_locations,
        transaction_facts: transaction_artifacts.transaction_facts,
        transaction_blobs: transaction_artifacts.transaction_blobs,
        transparent_outputs_by_outpoint,
    })
}

/// Serial fold over a single block's derived artifacts.
///
/// Applies `derived.tree_size_additions` to `running_tree_sizes`, stamps
/// the final `chain_metadata` into the compact block, and returns the
/// built [`BuiltArtifacts`].
/// Mutates `running_tree_sizes` in place so a sequential loop can carry
/// it forward across blocks.
pub fn finalize_derived_block(
    derived: DerivedBlockArtifacts,
    running_tree_sizes: &mut CommitmentTreeSizes,
) -> Result<BuiltArtifacts, ArtifactDeriveError> {
    let DerivedBlockArtifacts {
        block_header,
        block_blob,
        mut partial_compact_block,
        tree_size_additions,
        block_transaction_index,
        transaction_locations,
        transaction_facts,
        transaction_blobs,
        transparent_outputs_by_outpoint,
    } = derived;

    let final_tree_sizes = running_tree_sizes.checked_add(tree_size_additions)?;

    partial_compact_block.chain_metadata = Some(final_tree_sizes.chain_metadata());
    let compact_block = CompactBlockArtifact::new(
        block_header.height,
        block_header.block_hash,
        partial_compact_block.encode_to_vec(),
    );

    *running_tree_sizes = final_tree_sizes;

    Ok(BuiltArtifacts {
        block_header,
        block_blob,
        compact_block,
        block_transaction_index,
        transaction_locations,
        transaction_facts,
        transaction_blobs,
        transparent_outputs_by_outpoint,
        tip_metadata: final_tree_sizes.tip_metadata(),
    })
}

struct DerivedTransactionArtifacts {
    block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    transaction_locations: Vec<TransactionLocation>,
    transaction_facts: Vec<TransactionFactsArtifact>,
    transaction_blobs: Vec<TransactionBlobArtifact>,
}

fn derive_transaction_artifacts_from_parsed(
    parsed_block: &ZebraBlock,
    source_block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
    raw_blob_policy: RawBlobPolicy,
) -> Result<DerivedTransactionArtifacts, ArtifactDeriveError> {
    let mut block_transaction_index = Vec::with_capacity(parsed_block.transactions.len());
    let mut transaction_locations = Vec::with_capacity(parsed_block.transactions.len());
    let mut transaction_facts = Vec::with_capacity(parsed_block.transactions.len());
    let mut transaction_blobs = Vec::with_capacity(parsed_block.transactions.len());

    for (tx_index_in_block, transaction) in parsed_block.transactions.iter().enumerate() {
        let tx_index_in_block = u32::try_from(tx_index_in_block)
            .map_err(|_| ArtifactDeriveError::TransactionIndexOverflow)?;
        let payload_bytes = transaction
            .zcash_serialize_to_vec()
            .map_err(|source| ArtifactDeriveError::TransactionSerializationFailed { source })?;
        let transaction_id = TransactionId::from_bytes(transaction.hash().0);
        let location = TransactionLocation::new(
            transaction_id,
            source_block.height,
            source_block.hash,
            tx_index_in_block,
        );
        let fact_set = parse_transaction_public_fact_set(
            &payload_bytes,
            Some(source_block.height),
            activations,
        )
        .map_err(|source| ArtifactDeriveError::TransactionFactsParseFailed {
            reason: source.to_string(),
        })?;
        block_transaction_index.push(BlockTransactionIndexArtifact::new(
            source_block.height,
            tx_index_in_block,
            transaction_id,
            source_block.hash,
        ));
        transaction_locations.push(location);
        transaction_facts.push(
            TransactionFactsArtifact::new(location, fact_set.public_facts)
                .with_transparent_facts(fact_set.transparent_inputs, fact_set.transparent_outputs),
        );
        if raw_blob_policy.writes_transaction_blobs() {
            transaction_blobs.push(TransactionBlobArtifact::new(location, payload_bytes));
        } else {
            record_raw_blob_disabled("transaction_blob", 1);
        }
    }

    Ok(DerivedTransactionArtifacts {
        block_transaction_index,
        transaction_locations,
        transaction_facts,
        transaction_blobs,
    })
}

fn record_raw_blob_disabled(table: &'static str, row_count: u64) {
    metrics::counter!("zinder_ingest_raw_blob_disabled_total", "table" => table)
        .increment(row_count);
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
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
            ironwood: count_to_u32(
                compact_transaction.ironwood_actions.len(),
                "Ironwood action count",
            )?,
        })?;

        if compact_transaction_has_payload(&compact_transaction) {
            compact_transactions.push(compact_transaction);
        }
    }

    Ok((compact_transactions, tree_size_additions))
}

/// Builds a lightwalletd-compatible compact transaction from a parsed
/// Zebra transaction.
///
/// `index` is the position of the transaction within its containing block;
/// mempool callers that hydrate a single unmined transaction supply `0`.
pub(crate) fn compact_transaction(
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
        ironwood_actions: compact_ironwood_actions(transaction)?,
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

fn compact_ironwood_actions(
    transaction: &ZebraTransaction,
) -> Result<Vec<CompactOrchardAction>, ArtifactDeriveError> {
    transaction
        .ironwood_actions()
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

fn transparent_output_artifacts(
    source_block: &SourceBlock,
    compact_transactions: &[CompactTx],
) -> Result<Vec<TransparentOutputArtifact>, ArtifactDeriveError> {
    let mut transparent_outputs_by_outpoint = Vec::new();

    for transaction in compact_transactions {
        let transaction_id = transaction_id_from_compact_tx(transaction)?;
        for (output_index, output) in transaction.vout.iter().enumerate() {
            let output_index = u32::try_from(output_index)
                .map_err(|_| ArtifactDeriveError::TransparentOutputIndexOverflow)?;
            let address_script_hash =
                TransparentAddressScriptHash::of_script_pub_key(&output.script_pub_key);
            let outpoint = TransparentOutPoint::new(transaction_id, output_index);
            transparent_outputs_by_outpoint.push(TransparentOutputArtifact::new(
                outpoint,
                output.value,
                output.script_pub_key.clone(),
                address_script_hash,
                source_block.height,
                source_block.hash,
            ));
        }
    }

    Ok(transparent_outputs_by_outpoint)
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

fn compact_transaction_has_payload(transaction: &CompactTx) -> bool {
    !transaction.spends.is_empty()
        || !transaction.outputs.is_empty()
        || !transaction.actions.is_empty()
        || !transaction.ironwood_actions.is_empty()
        || !transaction.vin.is_empty()
        || !transaction.vout.is_empty()
}

fn count_to_u32(count: usize, field: &'static str) -> Result<u32, ArtifactDeriveError> {
    u32::try_from(count).map_err(|_| ArtifactDeriveError::CountOverflow { field })
}

fn format_block_hash(bytes: [u8; 32]) -> String {
    encode_rpc_block_hash_hex(BlockHash::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{
        ArtifactDeriveError, COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN, CompactOrchardAction, CompactTx,
        compact_note_ciphertext_prefix, compact_transaction_has_payload,
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
    fn has_payload_keeps_an_ironwood_only_transaction() {
        let ironwood_only_transaction = CompactTx {
            index: 0,
            txid: vec![0u8; 32],
            fee: 0,
            spends: Vec::new(),
            outputs: Vec::new(),
            actions: Vec::new(),
            ironwood_actions: vec![CompactOrchardAction {
                nullifier: vec![0u8; 32],
                cmx: vec![0u8; 32],
                ephemeral_key: vec![0u8; 32],
                ciphertext: vec![0u8; 52],
            }],
            vin: Vec::new(),
            vout: Vec::new(),
        };

        assert!(
            compact_transaction_has_payload(&ironwood_only_transaction),
            "a transaction with only Ironwood actions and no transparent, Sapling, or Orchard \
             components must not be dropped from the compact block's transaction list"
        );
    }
}
