//! Deterministic source-block artifact builders.

use std::{fmt, time::Instant};

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
    CanonicalBlockFacts, CanonicalTransactionFacts, ChainTipMetadata, CompactBlockArtifact,
    NetworkUpgradeActivations, PositionedCanonicalBlock, ShieldedProtocol, TransactionBlobArtifact,
    TransactionFactsArtifact, TransactionIntrinsicValueBalancesArtifact, TransactionLocation,
    TransparentOutPoint, TransparentOutputArtifact,
    wire::{encode_internal_block_hash, encode_rpc_block_hash_hex},
};
use zinder_proto::compat::lightwalletd::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend,
    CompactTx, CompactTxIn, TxOut as CompactTxOut,
};
use zinder_source::{
    SourceBlock, block_header_info_from_raw_block_bytes, transaction_public_fact_set_from_parsed,
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

/// Error returned while constructing canonical blocks from source blocks.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalBlockConstructionError {
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

    /// Transparent input previous transaction id has the wrong length.
    #[error("transparent input prevout transaction id is {byte_count} bytes, expected 32")]
    TransparentOutputTransactionIdMalformed {
        /// Observed byte count.
        byte_count: usize,
    },

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

/// Shielded commitment-tree positions at one block boundary.
///
/// `prepare_canonical_block` returns a per-block delta; `finalize_canonical_block` folds
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
    pub fn checked_add(self, additions: Self) -> Result<Self, CanonicalBlockConstructionError> {
        let sapling = self.sapling.checked_add(additions.sapling).ok_or(
            CanonicalBlockConstructionError::CommitmentTreeOverflow {
                protocol: ShieldedProtocol::Sapling,
            },
        )?;
        let orchard = self.orchard.checked_add(additions.orchard).ok_or(
            CanonicalBlockConstructionError::CommitmentTreeOverflow {
                protocol: ShieldedProtocol::Orchard,
            },
        )?;
        let ironwood = self.ironwood.checked_add(additions.ironwood).ok_or(
            CanonicalBlockConstructionError::CommitmentTreeOverflow {
                protocol: ShieldedProtocol::Ironwood,
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

/// Parallel-safe preparation output for one source block.
///
/// [`prepare_canonical_block`] populates every field that depends only on the
/// block content. [`finalize_canonical_block`] stamps the position-dependent fields
/// (the `chain_metadata` inside `partial_compact_block` and the
/// `tip_metadata` in the returned [`PositionedCanonicalBlock`]) after folding
/// `tree_size_additions` into the running commitment-tree position.
#[derive(Debug)]
pub struct PreparedCanonicalBlock {
    /// Immutable facts computed from this source block alone.
    pub facts: CanonicalBlockFacts,
    /// Lightwalletd compact block with `chain_metadata = None`; the
    /// serial folder stamps the final tree-size position before encoding.
    pub partial_compact_block: CompactBlock,
    /// Commitment-tree-size delta this block contributes.
    pub tree_size_additions: CommitmentTreeSizes,
}

/// Parses one source block and prepares its block-local canonical facts.
///
/// Preparation is parallel-safe because the returned value does not depend on
/// the block's position in the running chain state.
///
/// The output's `partial_compact_block.chain_metadata` is left `None`;
/// [`finalize_canonical_block`] stamps the final commitment-tree position
/// before encoding the proto and validating against any source-supplied
/// observation.
pub fn prepare_canonical_block(
    source_block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
) -> Result<PreparedCanonicalBlock, CanonicalBlockConstructionError> {
    prepare_canonical_block_with_raw_blob_policy(source_block, activations, RawBlobPolicy::None)
}

/// Prepares block-local canonical facts with an explicit raw-blob policy.
#[allow(
    clippy::too_many_lines,
    reason = "one parse-and-construct pipeline keeps stage timing and field ownership auditable"
)]
pub fn prepare_canonical_block_with_raw_blob_policy(
    source_block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
    raw_blob_policy: RawBlobPolicy,
) -> Result<PreparedCanonicalBlock, CanonicalBlockConstructionError> {
    validate_source_block_payload(source_block)?;
    let parsed_block =
        measure_canonical_construction_stage("block_parse", || parse_source_block(source_block))?;
    measure_canonical_construction_stage("identity_validation", || {
        validate_parsed_block_identity(&parsed_block, source_block)
    })?;
    let (compact_transactions, tree_size_additions) =
        measure_canonical_construction_stage("compact_artifacts", || {
            compact_transactions(&parsed_block)
        })?;
    let transactions = measure_canonical_construction_stage("transaction_facts", || {
        prepare_canonical_transactions_from_parsed(
            &parsed_block,
            source_block,
            activations,
            raw_blob_policy,
        )
    })?;
    let (block_header_bytes, block_header) =
        measure_canonical_construction_stage("block_header_artifact", || {
            let block_header_bytes =
                parsed_block
                    .header
                    .zcash_serialize_to_vec()
                    .map_err(|source| {
                        CanonicalBlockConstructionError::BlockHeaderSerializationFailed { source }
                    })?;
            let block_header = BlockHeaderArtifact::from_header_info_with_block_size(
                block_header_info_from_raw_block_bytes(
                    source_block.height,
                    &source_block.raw_block_bytes,
                )
                .map_err(|source| {
                    CanonicalBlockConstructionError::BlockHeaderParseFailed {
                        reason: source.to_string(),
                    }
                })?,
                usize_to_u64_saturating(source_block.raw_block_bytes.len()),
            );
            Ok((block_header_bytes, block_header))
        })?;
    let raw_block_bytes = measure_canonical_construction_stage("raw_block_bytes", || {
        Ok(if raw_blob_policy.writes_block_blobs() {
            Some(source_block.raw_block_bytes.clone())
        } else {
            record_raw_blob_disabled("block_blob", 1);
            None
        })
    })?;
    let partial_compact_block = CompactBlock {
        height: u64::from(source_block.height.value()),
        hash: encode_internal_block_hash(source_block.hash).to_vec(),
        prev_hash: encode_internal_block_hash(source_block.parent_hash).to_vec(),
        time: source_block.block_time_seconds,
        header: block_header_bytes,
        vtx: compact_transactions,
        chain_metadata: None,
    };

    Ok(PreparedCanonicalBlock {
        facts: CanonicalBlockFacts {
            block_header,
            raw_block_bytes,
            transactions,
        },
        partial_compact_block,
        tree_size_additions,
    })
}

/// Finalizes one prepared block at its ordered commitment-tree position.
///
/// Applies `prepared.tree_size_additions` to `running_tree_sizes`, stamps
/// the final `chain_metadata` into the compact block, and returns the
/// finalized [`PositionedCanonicalBlock`].
/// Mutates `running_tree_sizes` in place so a sequential loop can carry
/// it forward across blocks.
pub fn finalize_canonical_block(
    prepared: PreparedCanonicalBlock,
    running_tree_sizes: &mut CommitmentTreeSizes,
) -> Result<PositionedCanonicalBlock, CanonicalBlockConstructionError> {
    let PreparedCanonicalBlock {
        facts,
        mut partial_compact_block,
        tree_size_additions,
    } = prepared;

    let final_tree_sizes = running_tree_sizes.checked_add(tree_size_additions)?;

    partial_compact_block.chain_metadata = Some(final_tree_sizes.chain_metadata());
    let compact_block = CompactBlockArtifact::new(
        facts.block_header.height,
        facts.block_header.block_hash,
        partial_compact_block.encode_to_vec(),
    );

    *running_tree_sizes = final_tree_sizes;

    Ok(PositionedCanonicalBlock {
        facts,
        compact_block,
        tip_metadata: final_tree_sizes.tip_metadata(),
    })
}

/// Physical rows required by the current `RocksDB` writer.
///
/// These redundant index shapes are deliberately private to the current-schema
/// writer boundary and are not part of the backend-neutral fact contract.
pub(crate) struct CurrentSchemaBlockArtifacts {
    pub(crate) block_header: BlockHeaderArtifact,
    pub(crate) block_blob: Option<BlockBlobArtifact>,
    pub(crate) block_transaction_index: Vec<BlockTransactionIndexArtifact>,
    pub(crate) transaction_locations: Vec<TransactionLocation>,
    pub(crate) transaction_facts: Vec<TransactionFactsArtifact>,
    pub(crate) transaction_intrinsic_value_balances: Vec<TransactionIntrinsicValueBalancesArtifact>,
    pub(crate) transaction_blobs: Vec<TransactionBlobArtifact>,
    pub(crate) transparent_outputs_by_outpoint: Vec<TransparentOutputArtifact>,
}

/// Expands the minimal fact envelope into the redundant rows the current
/// `RocksDB` schema still commits.
pub(crate) fn expand_current_schema_block_artifacts(
    facts: CanonicalBlockFacts,
) -> Result<CurrentSchemaBlockArtifacts, CanonicalBlockConstructionError> {
    let transparent_outputs_by_outpoint = current_schema_transparent_outputs(&facts);
    let CanonicalBlockFacts {
        block_header,
        raw_block_bytes,
        transactions,
    } = facts;
    let block_height = block_header.height;
    let block_hash = block_header.block_hash;
    let block_blob = raw_block_bytes.map(|raw_block_bytes| {
        BlockBlobArtifact::new(
            block_height,
            block_hash,
            block_header.parent_hash,
            raw_block_bytes,
        )
    });
    let mut block_transaction_index = Vec::with_capacity(transactions.len());
    let mut transaction_locations = Vec::with_capacity(transactions.len());
    let mut transaction_facts = Vec::with_capacity(transactions.len());
    let mut transaction_intrinsic_value_balances = Vec::with_capacity(transactions.len());
    let mut transaction_blobs = Vec::new();

    for (tx_index_in_block, transaction) in transactions.into_iter().enumerate() {
        let tx_index_in_block = u32::try_from(tx_index_in_block)
            .map_err(|_| CanonicalBlockConstructionError::TransactionIndexOverflow)?;
        let CanonicalTransactionFacts {
            public_facts,
            intrinsic_value_balances,
            transparent_inputs,
            transparent_outputs,
            raw_transaction_bytes,
        } = transaction;
        let transaction_id = public_facts.transaction_id;
        let location =
            TransactionLocation::new(transaction_id, block_height, block_hash, tx_index_in_block);
        block_transaction_index.push(BlockTransactionIndexArtifact::new(
            block_height,
            tx_index_in_block,
            transaction_id,
            block_hash,
        ));
        transaction_locations.push(location);
        transaction_facts.push(
            TransactionFactsArtifact::new(location, public_facts)
                .with_transparent_facts(transparent_inputs, transparent_outputs),
        );
        transaction_intrinsic_value_balances.push(TransactionIntrinsicValueBalancesArtifact::new(
            location,
            intrinsic_value_balances,
        ));
        if let Some(raw_transaction_bytes) = raw_transaction_bytes {
            transaction_blobs.push(TransactionBlobArtifact::new(
                location,
                raw_transaction_bytes,
            ));
        }
    }

    Ok(CurrentSchemaBlockArtifacts {
        block_header,
        block_blob,
        block_transaction_index,
        transaction_locations,
        transaction_facts,
        transaction_intrinsic_value_balances,
        transaction_blobs,
        transparent_outputs_by_outpoint,
    })
}

/// Expands block-local transparent outputs into current-schema outpoint rows.
pub(crate) fn current_schema_transparent_outputs(
    facts: &CanonicalBlockFacts,
) -> Vec<TransparentOutputArtifact> {
    let block_height = facts.block_header.height;
    let block_hash = facts.block_header.block_hash;
    facts
        .transactions
        .iter()
        .flat_map(|transaction| {
            let transaction_id = transaction.public_facts.transaction_id;
            transaction.transparent_outputs.iter().map(move |output| {
                TransparentOutputArtifact::new(
                    TransparentOutPoint::new(transaction_id, output.output_index),
                    output.value_zat,
                    output.script_pub_key.clone(),
                    output.address_script_hash,
                    block_height,
                    block_hash,
                )
            })
        })
        .collect()
}

fn prepare_canonical_transactions_from_parsed(
    parsed_block: &ZebraBlock,
    source_block: &SourceBlock,
    activations: &NetworkUpgradeActivations,
    raw_blob_policy: RawBlobPolicy,
) -> Result<Vec<CanonicalTransactionFacts>, CanonicalBlockConstructionError> {
    let mut transactions = Vec::with_capacity(parsed_block.transactions.len());

    for (tx_index_in_block, transaction) in parsed_block.transactions.iter().enumerate() {
        let _tx_index_in_block = u32::try_from(tx_index_in_block)
            .map_err(|_| CanonicalBlockConstructionError::TransactionIndexOverflow)?;
        let serialized_size = transaction.zcash_serialized_size();
        let fact_set = transaction_public_fact_set_from_parsed(
            transaction,
            serialized_size,
            Some(source_block.height),
            activations,
        )
        .map_err(|source| {
            CanonicalBlockConstructionError::TransactionFactsParseFailed {
                reason: source.to_string(),
            }
        })?;
        let raw_transaction_bytes = if raw_blob_policy.writes_transaction_blobs() {
            Some(transaction.zcash_serialize_to_vec().map_err(|source| {
                CanonicalBlockConstructionError::TransactionSerializationFailed { source }
            })?)
        } else {
            record_raw_blob_disabled("transaction_blob", 1);
            None
        };
        transactions.push(CanonicalTransactionFacts {
            public_facts: fact_set.public_facts,
            intrinsic_value_balances: fact_set.intrinsic_value_balances,
            transparent_inputs: fact_set.transparent_inputs,
            transparent_outputs: fact_set.transparent_outputs,
            raw_transaction_bytes,
        });
    }

    Ok(transactions)
}

fn record_raw_blob_disabled(table: &'static str, row_count: u64) {
    metrics::counter!("zinder_ingest_raw_blob_disabled_total", "table" => table)
        .increment(row_count);
}

fn record_canonical_construction_stage<T>(
    stage: &'static str,
    started_at: Instant,
    outcome: &Result<T, CanonicalBlockConstructionError>,
) {
    metrics::histogram!(
        "zinder_ingest_canonical_block_construction_stage_duration_seconds",
        "stage" => stage,
        "status" => if outcome.is_ok() { "ok" } else { "error" }
    )
    .record(started_at.elapsed());
}

fn measure_canonical_construction_stage<T>(
    stage: &'static str,
    operation: impl FnOnce() -> Result<T, CanonicalBlockConstructionError>,
) -> Result<T, CanonicalBlockConstructionError> {
    let started_at = Instant::now();
    let outcome = operation();
    record_canonical_construction_stage(stage, started_at, &outcome);
    outcome
}

fn usize_to_u64_saturating(amount: usize) -> u64 {
    u64::try_from(amount).unwrap_or(u64::MAX)
}

fn validate_source_block_payload(
    source_block: &SourceBlock,
) -> Result<(), CanonicalBlockConstructionError> {
    if source_block.raw_block_bytes.is_empty() {
        return Err(CanonicalBlockConstructionError::EmptySourcePayload);
    }

    Ok(())
}

fn parse_source_block(
    source_block: &SourceBlock,
) -> Result<ZebraBlock, CanonicalBlockConstructionError> {
    source_block
        .raw_block_bytes
        .as_slice()
        .zcash_deserialize_into::<ZebraBlock>()
        .map_err(|source| CanonicalBlockConstructionError::BlockParseFailed { source })
}

fn validate_parsed_block_identity(
    parsed_block: &ZebraBlock,
    source_block: &SourceBlock,
) -> Result<(), CanonicalBlockConstructionError> {
    let parsed_height = parsed_block
        .coinbase_height()
        .ok_or(CanonicalBlockConstructionError::ParsedBlockMissingCoinbaseHeight)?
        .0;
    if parsed_height != source_block.height.value() {
        return Err(CanonicalBlockConstructionError::SourceBlockMismatch {
            field: BlockMismatchField::Height,
            expected: source_block.height.value().to_string(),
            actual: parsed_height.to_string(),
        });
    }

    let parsed_hash = parsed_block.hash().0;
    if parsed_hash != source_block.hash.as_bytes() {
        return Err(CanonicalBlockConstructionError::SourceBlockMismatch {
            field: BlockMismatchField::Hash,
            expected: format_block_hash(source_block.hash.as_bytes()),
            actual: format_block_hash(parsed_hash),
        });
    }

    let parsed_parent_hash = parsed_block.header.previous_block_hash.0;
    if parsed_parent_hash != source_block.parent_hash.as_bytes() {
        return Err(CanonicalBlockConstructionError::SourceBlockMismatch {
            field: BlockMismatchField::ParentHash,
            expected: format_block_hash(source_block.parent_hash.as_bytes()),
            actual: format_block_hash(parsed_parent_hash),
        });
    }

    let parsed_time = u32::try_from(parsed_block.header.time.timestamp()).map_err(|_| {
        CanonicalBlockConstructionError::CountOverflow {
            field: "parsed block time",
        }
    })?;
    if parsed_time != source_block.block_time_seconds {
        return Err(CanonicalBlockConstructionError::SourceBlockMismatch {
            field: BlockMismatchField::Time,
            expected: source_block.block_time_seconds.to_string(),
            actual: parsed_time.to_string(),
        });
    }

    Ok(())
}

fn compact_transactions(
    parsed_block: &ZebraBlock,
) -> Result<(Vec<CompactTx>, CommitmentTreeSizes), CanonicalBlockConstructionError> {
    let mut compact_transactions = Vec::new();
    let mut tree_size_additions = CommitmentTreeSizes::default();

    for (transaction_index, transaction) in parsed_block.transactions.iter().enumerate() {
        let compact_transaction = compact_transaction(
            u64::try_from(transaction_index).map_err(|_| {
                CanonicalBlockConstructionError::CountOverflow {
                    field: "transaction index",
                }
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
) -> Result<CompactTx, CanonicalBlockConstructionError> {
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
) -> Result<Vec<CompactSaplingOutput>, CanonicalBlockConstructionError> {
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
) -> Result<Vec<CompactOrchardAction>, CanonicalBlockConstructionError> {
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
) -> Result<Vec<CompactOrchardAction>, CanonicalBlockConstructionError> {
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

fn compact_note_ciphertext_prefix(
    ciphertext: &[u8],
) -> Result<Vec<u8>, CanonicalBlockConstructionError> {
    ciphertext
        .get(..COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN)
        .map(<[u8]>::to_vec)
        .ok_or(CanonicalBlockConstructionError::CompactCiphertextTooShort)
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

fn compact_transaction_has_payload(transaction: &CompactTx) -> bool {
    !transaction.spends.is_empty()
        || !transaction.outputs.is_empty()
        || !transaction.actions.is_empty()
        || !transaction.ironwood_actions.is_empty()
        || !transaction.vin.is_empty()
        || !transaction.vout.is_empty()
}

fn count_to_u32(count: usize, field: &'static str) -> Result<u32, CanonicalBlockConstructionError> {
    u32::try_from(count).map_err(|_| CanonicalBlockConstructionError::CountOverflow { field })
}

fn format_block_hash(bytes: [u8; 32]) -> String {
    encode_rpc_block_hash_hex(BlockHash::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use serde_json::Value;
    use zinder_core::{
        ArtifactSchemaVersion, BlockHeight, ChainEpoch, ChainEpochId, Network, UnixTimestampMillis,
    };
    use zinder_source::SourceBlock;
    use zinder_store::ChainEpochArtifacts;
    use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

    use super::{
        COMPACT_NOTE_CIPHERTEXT_PREFIX_LEN, CanonicalBlockConstructionError, CommitmentTreeSizes,
        CompactOrchardAction, CompactTx, RawBlobPolicy, ShieldedProtocol,
        compact_note_ciphertext_prefix, compact_transaction_has_payload,
        expand_current_schema_block_artifacts, finalize_canonical_block,
        prepare_canonical_block_with_raw_blob_policy,
    };

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the boundary test keeps real-fixture expansion, commit, and row reads together"
    )]
    fn current_schema_expansion_round_trips_real_fixture_rows() -> Result<(), Box<dyn Error>> {
        let source_block = regtest_fixture_block()?;
        let prepared = prepare_canonical_block_with_raw_blob_policy(
            &source_block,
            &sample_regtest_upgrade_activations(),
            RawBlobPolicy::All,
        )?;
        let mut tree_sizes = CommitmentTreeSizes::default();
        let positioned = finalize_canonical_block(prepared, &mut tree_sizes)?;
        let compact_block = positioned.compact_block;
        let tip_metadata = positioned.tip_metadata;
        let current_schema = expand_current_schema_block_artifacts(positioned.facts)?;
        let location = *current_schema
            .transaction_locations
            .first()
            .ok_or("fixture transaction location is missing")?;
        let transaction_facts = current_schema
            .transaction_facts
            .first()
            .cloned()
            .ok_or("fixture transaction facts are missing")?;
        let transaction_blob = current_schema
            .transaction_blobs
            .first()
            .cloned()
            .ok_or("fixture transaction blob is missing")?;
        let transparent_output = current_schema
            .transparent_outputs_by_outpoint
            .first()
            .cloned()
            .ok_or("fixture transparent output is missing")?;
        let fixture = StoreFixture::open()?;
        let chain_epoch = ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            visible_tip_height: source_block.height,
            visible_tip_hash: source_block.hash,
            settled_tip_height: source_block.height,
            settled_tip_hash: source_block.hash,
            artifact_schema_version: ArtifactSchemaVersion::new(18),
            tip_metadata,
            created_at: UnixTimestampMillis::new(1_774_669_000_000),
        };
        let block_blobs = current_schema.block_blob.into_iter().collect();
        fixture.chain_store().commit_chain_epoch(
            ChainEpochArtifacts::new(
                chain_epoch,
                vec![current_schema.block_header],
                vec![compact_block],
            )
            .with_block_blobs(block_blobs)
            .with_block_transaction_index(current_schema.block_transaction_index)
            .with_transaction_locations(current_schema.transaction_locations)
            .with_transaction_facts(current_schema.transaction_facts)
            .with_transaction_intrinsic_value_balances(
                current_schema.transaction_intrinsic_value_balances,
            )
            .with_transaction_blobs(current_schema.transaction_blobs)
            .with_transparent_outputs_by_outpoint(current_schema.transparent_outputs_by_outpoint),
        )?;

        let reader = fixture.chain_store().current_chain_epoch_reader()?;
        assert_eq!(
            reader.transaction_id_at_block_index(source_block.height, 0)?,
            Some(location.transaction_id)
        );
        assert_eq!(
            reader.transaction_location_by_id(location.transaction_id)?,
            Some(location)
        );
        assert_eq!(
            reader.transaction_facts_by_id(location.transaction_id)?,
            Some(transaction_facts)
        );
        assert_eq!(
            reader.transaction_blob_by_id(location.transaction_id)?,
            Some(transaction_blob)
        );
        assert_eq!(
            reader
                .transparent_outputs_by_outpoints(&[transparent_output.outpoint])?
                .get(&transparent_output.outpoint),
            Some(&transparent_output)
        );
        Ok(())
    }

    fn regtest_fixture_block() -> Result<SourceBlock, Box<dyn Error>> {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/z3-regtest-block-1.json"))?;
        let raw_block_hex = fixture
            .get("raw_block_hex")
            .and_then(Value::as_str)
            .ok_or("fixture raw_block_hex is missing")?;
        let raw_block_bytes = hex::decode(raw_block_hex)?;
        let height = fixture
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|height| u32::try_from(height).ok())
            .ok_or("fixture height is missing")?;
        Ok(SourceBlock::from_raw_block_bytes(
            Network::ZcashRegtest,
            BlockHeight::new(height),
            raw_block_bytes,
        )?)
    }

    #[test]
    fn commitment_tree_size_overflow_identifies_ironwood() -> Result<(), Box<dyn Error>> {
        let running_sizes = CommitmentTreeSizes {
            ironwood: u32::MAX,
            ..CommitmentTreeSizes::default()
        };
        let additions = CommitmentTreeSizes {
            ironwood: 1,
            ..CommitmentTreeSizes::default()
        };
        let error = match running_sizes.checked_add(additions) {
            Ok(sizes) => {
                return Err(format!("expected Ironwood overflow, got {sizes:?}").into());
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            CanonicalBlockConstructionError::CommitmentTreeOverflow {
                protocol: ShieldedProtocol::Ironwood
            }
        ));
        Ok(())
    }

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
            CanonicalBlockConstructionError::CompactCiphertextTooShort
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
