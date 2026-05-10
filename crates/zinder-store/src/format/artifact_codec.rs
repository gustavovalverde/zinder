//! Protobuf and envelope codecs for persisted chain records.

use bytes::Bytes;
use prost::Message;
use zinder_core::{
    ArtifactSchemaVersion, AuthDigest, BlockArtifact, BlockHash, BlockHeight, BlockHeightRange,
    ChainEpoch, ChainEpochId, ChainTipMetadata, CompactBlockArtifact, MempoolEntry,
    MempoolEvictionReason, Network, RawTransactionBytes, ShieldedProtocol, SubtreeRootArtifact,
    SubtreeRootHash, SubtreeRootIndex, TransactionArtifact, TransactionId,
    TransparentAddressScriptHash, TransparentAddressTxIndexArtifact,
    TransparentAddressUtxoArtifact, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, TransparentUtxoSpendArtifact, TreeStateArtifact, UnixTimestampMillis,
};

use crate::{
    ArtifactFamily, ChainEpochCommitted, ChainEvent, ChainEventEnvelope, ChainRangeReverted,
    MempoolEvent, MempoolEventEnvelope, StoreError, StreamCursorTokenV1,
};

use super::{
    ArtifactEnvelopeError, ArtifactEnvelopeHeaderV1, ChainEventCursorAnchor,
    ChainEventStreamFamily, MempoolEventStreamFamily, PayloadFormat, StoreKey,
};

pub(crate) fn encode_chain_epoch(chain_epoch: &ChainEpoch) -> Vec<u8> {
    chain_epoch_record(chain_epoch).encode_to_vec()
}

pub(crate) fn decode_chain_epoch(
    key: &StoreKey,
    record_bytes: &[u8],
) -> Result<ChainEpoch, StoreError> {
    let record =
        ChainEpochRecord::decode(record_bytes).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEpoch,
            key: key.clone().into(),
            reason: "chain epoch record is not valid protobuf",
        })?;

    decode_chain_epoch_record(ArtifactFamily::ChainEpoch, key, &record)
}

pub(crate) fn encode_chain_event_envelope(event_envelope: &ChainEventEnvelope) -> Vec<u8> {
    ChainEventEnvelopeRecord {
        event_sequence: event_envelope.event_sequence,
        chain_epoch: Some(chain_epoch_record(&event_envelope.chain_epoch)),
        finalized_height: event_envelope.finalized_height.value(),
        event: Some(chain_event_record(&event_envelope.event)),
    }
    .encode_to_vec()
}

pub(crate) fn decode_chain_event_envelope(
    key: &StoreKey,
    record_bytes: &[u8],
    family: ChainEventStreamFamily,
    cursor_auth_key: [u8; 32],
) -> Result<ChainEventEnvelope, StoreError> {
    // This decoder is intentionally pure over `record_bytes`. Chain-event
    // history callers rely on their snapshot-bound read happening before this
    // function and must not add fallback database reads here.
    let record = ChainEventEnvelopeRecord::decode(record_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEvent,
            key: key.clone().into(),
            reason: "chain event envelope record is not valid protobuf",
        }
    })?;
    let chain_epoch_record = record.chain_epoch.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain event envelope is missing chain epoch",
    })?;
    let chain_epoch =
        decode_chain_epoch_record(ArtifactFamily::ChainEvent, key, &chain_epoch_record)?;
    let event_record = record.event.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain event envelope is missing event",
    })?;
    let event = decode_chain_event_record(key, event_record)?;
    let cursor = StreamCursorTokenV1::chain_event(
        chain_epoch.network,
        family,
        record.event_sequence,
        ChainEventCursorAnchor {
            height: chain_epoch.tip_height,
            hash: chain_epoch.tip_hash,
        },
        cursor_auth_key,
    )
    .map_err(|_| StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain event cursor could not be reconstructed",
    })?;

    Ok(ChainEventEnvelope::new(
        cursor,
        record.event_sequence,
        chain_epoch,
        BlockHeight::new(record.finalized_height),
        event,
    ))
}

pub(crate) fn encode_mempool_event_envelope(event_envelope: &MempoolEventEnvelope) -> Vec<u8> {
    MempoolEventEnvelopeRecord {
        event_sequence: event_envelope.event_sequence,
        source_observed_unix_millis: event_envelope.source_observed_unix_millis,
        event: Some(mempool_event_record(&event_envelope.event)),
    }
    .encode_to_vec()
}

pub(crate) fn decode_mempool_event_envelope(
    key: &StoreKey,
    record_bytes: &[u8],
    network: Network,
    cursor_auth_key: [u8; 32],
) -> Result<MempoolEventEnvelope, StoreError> {
    let record = MempoolEventEnvelopeRecord::decode(record_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.clone().into(),
            reason: "mempool event envelope record is not valid protobuf",
        }
    })?;
    let event_record = record.event.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::MempoolEvent,
        key: key.clone().into(),
        reason: "mempool event envelope is missing event",
    })?;
    let event = decode_mempool_event_record(key, event_record)?;
    let cursor = StreamCursorTokenV1::mempool_event(
        network,
        MempoolEventStreamFamily::Mempool,
        record.event_sequence,
        event.transaction_id(),
        cursor_auth_key,
    )
    .map_err(|_| StoreError::ArtifactCorrupt {
        family: ArtifactFamily::MempoolEvent,
        key: key.clone().into(),
        reason: "mempool event cursor could not be reconstructed",
    })?;

    Ok(MempoolEventEnvelope {
        cursor,
        event_sequence: record.event_sequence,
        source_observed_unix_millis: record.source_observed_unix_millis,
        event,
    })
}

pub(crate) fn decode_mempool_event_observed_at(
    key: &StoreKey,
    record_bytes: &[u8],
) -> Result<UnixTimestampMillis, StoreError> {
    let record = MempoolEventEnvelopeRecord::decode(record_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.clone().into(),
            reason: "mempool event envelope record is not valid protobuf",
        }
    })?;
    Ok(UnixTimestampMillis::new(record.source_observed_unix_millis))
}

pub(crate) fn decode_mempool_event_kind(
    key: &StoreKey,
    record_bytes: &[u8],
) -> Result<MempoolEventKind, StoreError> {
    let record = MempoolEventEnvelopeRecord::decode(record_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.clone().into(),
            reason: "mempool event envelope record is not valid protobuf",
        }
    })?;
    let event_record = record.event.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::MempoolEvent,
        key: key.clone().into(),
        reason: "mempool event envelope is missing event",
    })?;
    MempoolEventKind::from_kind(event_record.event_kind).ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::MempoolEvent,
        key: key.clone().into(),
        reason: "mempool event kind is unknown",
    })
}

/// Coarse classification of a persisted mempool event used by retention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MempoolEventKind {
    Added,
    Invalidated,
    Mined,
}

impl MempoolEventKind {
    const fn from_kind(kind: u32) -> Option<Self> {
        match kind {
            MEMPOOL_EVENT_KIND_ADDED => Some(Self::Added),
            MEMPOOL_EVENT_KIND_INVALIDATED => Some(Self::Invalidated),
            MEMPOOL_EVENT_KIND_MINED => Some(Self::Mined),
            _ => None,
        }
    }
}

fn mempool_event_record(event: &MempoolEvent) -> MempoolEventRecord {
    match event {
        MempoolEvent::Added { entry } => MempoolEventRecord {
            event_kind: MEMPOOL_EVENT_KIND_ADDED,
            added: Some(mempool_entry_record(entry)),
            invalidated: None,
            mined: None,
        },
        MempoolEvent::Invalidated {
            transaction_id,
            reason,
        } => MempoolEventRecord {
            event_kind: MEMPOOL_EVENT_KIND_INVALIDATED,
            added: None,
            invalidated: Some(MempoolInvalidatedRecord {
                transaction_id: transaction_id.as_bytes().to_vec(),
                reason_id: mempool_eviction_reason_id(*reason),
            }),
            mined: None,
        },
        MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        } => MempoolEventRecord {
            event_kind: MEMPOOL_EVENT_KIND_MINED,
            added: None,
            invalidated: None,
            mined: Some(MempoolMinedRecord {
                transaction_id: transaction_id.as_bytes().to_vec(),
                mined_height: mined_height.value(),
                block_hash: block_hash.as_bytes().to_vec(),
            }),
        },
        // The Rust enum is `#[non_exhaustive]`; future variants force a
        // build error in this match because the encoder is not behind
        // `unreachable_patterns`.
    }
}

fn decode_mempool_event_record(
    key: &StoreKey,
    record: MempoolEventRecord,
) -> Result<MempoolEvent, StoreError> {
    match MempoolEventKind::from_kind(record.event_kind).ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::MempoolEvent,
        key: key.clone().into(),
        reason: "mempool event kind is unknown",
    })? {
        MempoolEventKind::Added => {
            let entry_record = record.added.ok_or(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::MempoolEvent,
                key: key.clone().into(),
                reason: "added mempool event is missing entry",
            })?;
            let entry = decode_mempool_entry_record(key, entry_record)?;
            Ok(MempoolEvent::Added { entry })
        }
        MempoolEventKind::Invalidated => {
            let invalidated = record.invalidated.ok_or(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::MempoolEvent,
                key: key.clone().into(),
                reason: "invalidated mempool event is missing payload",
            })?;
            Ok(MempoolEvent::Invalidated {
                transaction_id: decode_transaction_id_for_family(
                    ArtifactFamily::MempoolEvent,
                    key,
                    &invalidated.transaction_id,
                )?,
                reason: mempool_eviction_reason_from_id(invalidated.reason_id).ok_or(
                    StoreError::ArtifactCorrupt {
                        family: ArtifactFamily::MempoolEvent,
                        key: key.clone().into(),
                        reason: "mempool eviction reason id is unknown",
                    },
                )?,
            })
        }
        MempoolEventKind::Mined => {
            let mined = record.mined.ok_or(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::MempoolEvent,
                key: key.clone().into(),
                reason: "mined mempool event is missing payload",
            })?;
            Ok(MempoolEvent::Mined {
                transaction_id: decode_transaction_id_for_family(
                    ArtifactFamily::MempoolEvent,
                    key,
                    &mined.transaction_id,
                )?,
                mined_height: BlockHeight::new(mined.mined_height),
                block_hash: decode_block_hash(
                    ArtifactFamily::MempoolEvent,
                    key,
                    &mined.block_hash,
                )?,
            })
        }
    }
}

fn mempool_entry_record(entry: &MempoolEntry) -> MempoolEntryRecord {
    MempoolEntryRecord {
        transaction_id: entry.transaction_id.as_bytes().to_vec(),
        auth_digest: entry
            .auth_digest
            .map_or_else(Vec::new, |digest| digest.as_bytes().to_vec()),
        raw_transaction_bytes: entry.raw_transaction_bytes.as_slice().to_vec(),
        compact_transaction_bytes: entry.compact_transaction_bytes.clone(),
        first_seen_unix_millis: entry.first_seen_unix_millis.value(),
        first_seen_chain_epoch: Some(chain_epoch_record(&entry.first_seen_chain_epoch)),
        transparent_outputs: entry
            .transparent_outputs
            .iter()
            .map(transparent_mempool_output_record)
            .collect(),
        transparent_spends: entry
            .transparent_spends
            .iter()
            .map(transparent_mempool_spend_record)
            .collect(),
    }
}

fn decode_mempool_entry_record(
    key: &StoreKey,
    record: MempoolEntryRecord,
) -> Result<MempoolEntry, StoreError> {
    let chain_epoch_record = record
        .first_seen_chain_epoch
        .ok_or(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::MempoolEvent,
            key: key.clone().into(),
            reason: "mempool entry record is missing first-seen chain epoch",
        })?;
    let auth_digest = if record.auth_digest.is_empty() {
        None
    } else {
        let auth_digest_bytes =
            <[u8; 32]>::try_from(record.auth_digest.as_slice()).map_err(|_| {
                StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::MempoolEvent,
                    key: key.clone().into(),
                    reason: "auth digest must be 32 bytes",
                }
            })?;
        Some(AuthDigest::from_bytes(auth_digest_bytes))
    };

    Ok(MempoolEntry {
        transaction_id: decode_transaction_id_for_family(
            ArtifactFamily::MempoolEvent,
            key,
            &record.transaction_id,
        )?,
        auth_digest,
        raw_transaction_bytes: RawTransactionBytes::new(record.raw_transaction_bytes),
        compact_transaction_bytes: record.compact_transaction_bytes,
        first_seen_unix_millis: UnixTimestampMillis::new(record.first_seen_unix_millis),
        first_seen_chain_epoch: decode_chain_epoch_record(
            ArtifactFamily::MempoolEvent,
            key,
            &chain_epoch_record,
        )?,
        transparent_outputs: record
            .transparent_outputs
            .iter()
            .map(|record| decode_transparent_mempool_output_record(key, record))
            .collect::<Result<Vec<_>, _>>()?,
        transparent_spends: record
            .transparent_spends
            .iter()
            .map(|record| decode_transparent_mempool_spend_record(key, record))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn transparent_mempool_output_record(
    transparent_output: &TransparentMempoolOutput,
) -> TransparentMempoolOutputRecord {
    TransparentMempoolOutputRecord {
        address_script_hash: transparent_output.address_script_hash.as_bytes().to_vec(),
        script_pub_key: transparent_output.script_pub_key.clone(),
        spending_transaction_id: transparent_output
            .outpoint
            .transaction_id
            .as_bytes()
            .to_vec(),
        output_index: transparent_output.outpoint.output_index,
        value_zat: transparent_output.value_zat,
    }
}

fn decode_transparent_mempool_output_record(
    key: &StoreKey,
    record: &TransparentMempoolOutputRecord,
) -> Result<TransparentMempoolOutput, StoreError> {
    Ok(TransparentMempoolOutput {
        address_script_hash: decode_transparent_address_script_hash_for_family(
            ArtifactFamily::MempoolEvent,
            key,
            &record.address_script_hash,
        )?,
        script_pub_key: record.script_pub_key.clone(),
        outpoint: TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::MempoolEvent,
                key,
                &record.spending_transaction_id,
            )?,
            record.output_index,
        ),
        value_zat: record.value_zat,
    })
}

fn transparent_mempool_spend_record(
    transparent_spend: &TransparentMempoolSpend,
) -> TransparentMempoolSpendRecord {
    TransparentMempoolSpendRecord {
        spent_transaction_id: transparent_spend
            .spent_outpoint
            .transaction_id
            .as_bytes()
            .to_vec(),
        spent_output_index: transparent_spend.spent_outpoint.output_index,
        spending_transaction_id: transparent_spend
            .spending_transaction_id
            .as_bytes()
            .to_vec(),
    }
}

fn decode_transparent_mempool_spend_record(
    key: &StoreKey,
    record: &TransparentMempoolSpendRecord,
) -> Result<TransparentMempoolSpend, StoreError> {
    Ok(TransparentMempoolSpend {
        spent_outpoint: TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::MempoolEvent,
                key,
                &record.spent_transaction_id,
            )?,
            record.spent_output_index,
        ),
        spending_transaction_id: decode_transaction_id_for_family(
            ArtifactFamily::MempoolEvent,
            key,
            &record.spending_transaction_id,
        )?,
    })
}

#[allow(
    clippy::match_same_arms,
    reason = "MempoolEvictionReason is #[non_exhaustive]; the wildcard arm intentionally projects future variants onto the Unknown id so storage stays forward-compatible."
)]
const fn mempool_eviction_reason_id(reason: MempoolEvictionReason) -> u32 {
    match reason {
        MempoolEvictionReason::Conflict => 1,
        MempoolEvictionReason::Expired => 2,
        MempoolEvictionReason::LowFee => 3,
        MempoolEvictionReason::NodeRejected => 4,
        MempoolEvictionReason::Unknown => 5,
        _ => 5,
    }
}

const fn mempool_eviction_reason_from_id(reason_id: u32) -> Option<MempoolEvictionReason> {
    match reason_id {
        1 => Some(MempoolEvictionReason::Conflict),
        2 => Some(MempoolEvictionReason::Expired),
        3 => Some(MempoolEvictionReason::LowFee),
        4 => Some(MempoolEvictionReason::NodeRejected),
        5 => Some(MempoolEvictionReason::Unknown),
        _ => None,
    }
}

fn decode_transparent_address_script_hash_for_family(
    family: ArtifactFamily,
    key: &StoreKey,
    hash_bytes: &[u8],
) -> Result<TransparentAddressScriptHash, StoreError> {
    let hash_bytes = <[u8; 32]>::try_from(hash_bytes).map_err(|_| StoreError::ArtifactCorrupt {
        family,
        key: key.clone().into(),
        reason: "transparent address script hash must be 32 bytes",
    })?;

    Ok(TransparentAddressScriptHash::from_bytes(hash_bytes))
}

pub(crate) fn encode_block_artifact(block: BlockArtifact) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderBlockArtifactV1,
        &BlockArtifactRecord {
            height: block.height.value(),
            hash: block.block_hash.as_bytes().to_vec(),
            parent_hash: block.parent_hash.as_bytes().to_vec(),
            payload_bytes: Bytes::from(block.payload_bytes),
        },
    )
}

pub(crate) fn decode_block_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<BlockArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::FinalizedBlock,
        key,
        envelope_bytes,
        PayloadFormat::ZinderBlockArtifactV1,
    )?;
    let record =
        BlockArtifactRecord::decode(payload_bytes).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::FinalizedBlock,
            key: key.clone().into(),
            reason: "block artifact record is not valid protobuf",
        })?;

    Ok(BlockArtifact::new(
        BlockHeight::new(record.height),
        decode_block_hash(ArtifactFamily::FinalizedBlock, key, &record.hash)?,
        decode_block_hash(ArtifactFamily::FinalizedBlock, key, &record.parent_hash)?,
        record.payload_bytes.to_vec(),
    ))
}

pub(crate) fn encode_compact_block_artifact(
    block: CompactBlockArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderCompactBlockArtifactV1,
        &CompactBlockArtifactRecord {
            height: block.height.value(),
            block_hash: block.block_hash.as_bytes().to_vec(),
            payload_bytes: Bytes::from(block.payload_bytes),
        },
    )
}

pub(crate) fn decode_compact_block_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<CompactBlockArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::CompactBlock,
        key,
        envelope_bytes,
        PayloadFormat::ZinderCompactBlockArtifactV1,
    )?;
    let record = CompactBlockArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::CompactBlock,
            key: key.clone().into(),
            reason: "compact block artifact record is not valid protobuf",
        }
    })?;

    Ok(CompactBlockArtifact::new(
        BlockHeight::new(record.height),
        decode_block_hash(ArtifactFamily::CompactBlock, key, &record.block_hash)?,
        record.payload_bytes.to_vec(),
    ))
}

pub(crate) fn encode_transaction_artifact(
    transaction: TransactionArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransactionArtifactV1,
        &TransactionArtifactRecord {
            transaction_id: transaction.transaction_id.as_bytes().to_vec(),
            block_height: transaction.block_height.value(),
            block_hash: transaction.block_hash.as_bytes().to_vec(),
            payload_bytes: Bytes::from(transaction.payload_bytes),
        },
    )
}

pub(crate) fn decode_transaction_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransactionArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::Transaction,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransactionArtifactV1,
    )?;
    let record = TransactionArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::Transaction,
            key: key.clone().into(),
            reason: "transaction artifact record is not valid protobuf",
        }
    })?;

    let transaction_id = decode_transaction_id(key, &record.transaction_id)?;

    Ok(TransactionArtifact::new(
        transaction_id,
        BlockHeight::new(record.block_height),
        decode_block_hash(ArtifactFamily::Transaction, key, &record.block_hash)?,
        record.payload_bytes.to_vec(),
    ))
}

pub(crate) fn encode_tree_state_artifact(
    tree_state: TreeStateArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTreeStateArtifactV1,
        &TreeStateArtifactRecord {
            height: tree_state.height.value(),
            block_hash: tree_state.block_hash.as_bytes().to_vec(),
            payload_bytes: Bytes::from(tree_state.payload_bytes),
        },
    )
}

pub(crate) fn decode_tree_state_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TreeStateArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TreeState,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTreeStateArtifactV1,
    )?;
    let record = TreeStateArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TreeState,
            key: key.clone().into(),
            reason: "tree-state artifact record is not valid protobuf",
        }
    })?;

    Ok(TreeStateArtifact::new(
        BlockHeight::new(record.height),
        decode_block_hash(ArtifactFamily::TreeState, key, &record.block_hash)?,
        record.payload_bytes.to_vec(),
    ))
}

pub(crate) fn encode_subtree_root_artifact(
    subtree_root: &SubtreeRootArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderSubtreeRootArtifactV1,
        &SubtreeRootArtifactRecord {
            protocol_id: u32::from(subtree_root.protocol.id()),
            subtree_index: subtree_root.subtree_index.value(),
            root_hash: subtree_root.root_hash.as_bytes().to_vec(),
            completing_block_height: subtree_root.completing_block_height.value(),
            completing_block_hash: subtree_root.completing_block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_subtree_root_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<SubtreeRootArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::SubtreeRoot,
        key,
        envelope_bytes,
        PayloadFormat::ZinderSubtreeRootArtifactV1,
    )?;
    let record = SubtreeRootArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::SubtreeRoot,
            key: key.clone().into(),
            reason: "subtree-root artifact record is not valid protobuf",
        }
    })?;
    let protocol_id =
        u8::try_from(record.protocol_id).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::SubtreeRoot,
            key: key.clone().into(),
            reason: "subtree-root protocol id does not fit u8",
        })?;
    let protocol = ShieldedProtocol::from_id(protocol_id).ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::SubtreeRoot,
        key: key.clone().into(),
        reason: "subtree-root protocol id is unknown",
    })?;

    Ok(SubtreeRootArtifact::new(
        protocol,
        SubtreeRootIndex::new(record.subtree_index),
        decode_subtree_root_hash(key, &record.root_hash)?,
        BlockHeight::new(record.completing_block_height),
        decode_block_hash(
            ArtifactFamily::SubtreeRoot,
            key,
            &record.completing_block_hash,
        )?,
    ))
}

pub(crate) fn encode_transparent_address_utxo_artifact(
    utxo: TransparentAddressUtxoArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentAddressUtxoArtifactV1,
        &TransparentAddressUtxoArtifactRecord {
            address_script_hash: utxo.address_script_hash.as_bytes().to_vec(),
            script_pub_key: Bytes::from(utxo.script_pub_key),
            transaction_id: utxo.outpoint.transaction_id.as_bytes().to_vec(),
            output_index: utxo.outpoint.output_index,
            value_zat: utxo.value_zat,
            block_height: utxo.block_height.value(),
            block_hash: utxo.block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_transparent_address_utxo_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransparentAddressUtxoArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentAddressUtxo,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentAddressUtxoArtifactV1,
    )?;
    let record = TransparentAddressUtxoArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentAddressUtxo,
            key: key.clone().into(),
            reason: "transparent address UTXO artifact record is not valid protobuf",
        }
    })?;

    Ok(TransparentAddressUtxoArtifact::new(
        decode_transparent_address_script_hash(key, &record.address_script_hash)?,
        record.script_pub_key.to_vec(),
        TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::TransparentAddressUtxo,
                key,
                &record.transaction_id,
            )?,
            record.output_index,
        ),
        record.value_zat,
        BlockHeight::new(record.block_height),
        decode_block_hash(
            ArtifactFamily::TransparentAddressUtxo,
            key,
            &record.block_hash,
        )?,
    ))
}

pub(crate) fn encode_transparent_address_tx_index_artifact(
    artifact: TransparentAddressTxIndexArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentAddressTxIndexArtifactV1,
        &TransparentAddressTxIndexArtifactRecord {
            transaction_id: artifact.transaction_id.as_bytes().to_vec(),
            block_hash: artifact.block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_transparent_address_tx_index_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
    address_script_hash: TransparentAddressScriptHash,
    block_height: BlockHeight,
    tx_index_in_block: u32,
) -> Result<TransparentAddressTxIndexArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentAddressTxIndex,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentAddressTxIndexArtifactV1,
    )?;
    let record = TransparentAddressTxIndexArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentAddressTxIndex,
            key: key.clone().into(),
            reason: "transparent address tx index artifact record is not valid protobuf",
        }
    })?;

    Ok(TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        block_height,
        tx_index_in_block,
        decode_transaction_id_for_family(
            ArtifactFamily::TransparentAddressTxIndex,
            key,
            &record.transaction_id,
        )?,
        decode_block_hash(
            ArtifactFamily::TransparentAddressTxIndex,
            key,
            &record.block_hash,
        )?,
    ))
}

pub(crate) fn encode_transparent_utxo_spend_artifact(
    spend: TransparentUtxoSpendArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentUtxoSpendArtifactV1,
        &TransparentUtxoSpendArtifactRecord {
            transaction_id: spend.spent_outpoint.transaction_id.as_bytes().to_vec(),
            output_index: spend.spent_outpoint.output_index,
            block_height: spend.block_height.value(),
            block_hash: spend.block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_transparent_utxo_spend_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransparentUtxoSpendArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentUtxoSpend,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentUtxoSpendArtifactV1,
    )?;
    let record = TransparentUtxoSpendArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentUtxoSpend,
            key: key.clone().into(),
            reason: "transparent UTXO spend artifact record is not valid protobuf",
        }
    })?;

    Ok(TransparentUtxoSpendArtifact::new(
        TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::TransparentUtxoSpend,
                key,
                &record.transaction_id,
            )?,
            record.output_index,
        ),
        BlockHeight::new(record.block_height),
        decode_block_hash(
            ArtifactFamily::TransparentUtxoSpend,
            key,
            &record.block_hash,
        )?,
    ))
}

fn chain_epoch_record(chain_epoch: &ChainEpoch) -> ChainEpochRecord {
    ChainEpochRecord {
        chain_epoch: chain_epoch.id.value(),
        network_id: chain_epoch.network.id(),
        tip_height: chain_epoch.tip_height.value(),
        tip_hash: chain_epoch.tip_hash.as_bytes().to_vec(),
        finalized_height: chain_epoch.finalized_height.value(),
        finalized_hash: chain_epoch.finalized_hash.as_bytes().to_vec(),
        artifact_schema_version: u32::from(chain_epoch.artifact_schema_version.value()),
        sapling_commitment_tree_size: chain_epoch.tip_metadata.sapling_commitment_tree_size,
        orchard_commitment_tree_size: chain_epoch.tip_metadata.orchard_commitment_tree_size,
        created_at_millis: chain_epoch.created_at.value(),
    }
}

fn decode_chain_epoch_record(
    family: ArtifactFamily,
    key: &StoreKey,
    record: &ChainEpochRecord,
) -> Result<ChainEpoch, StoreError> {
    let network = Network::from_id(record.network_id).ok_or(StoreError::ArtifactCorrupt {
        family,
        key: key.clone().into(),
        reason: "chain epoch record has an unknown network id",
    })?;

    Ok(ChainEpoch {
        id: ChainEpochId::new(record.chain_epoch),
        network,
        tip_height: BlockHeight::new(record.tip_height),
        tip_hash: decode_block_hash(family, key, &record.tip_hash)?,
        finalized_height: BlockHeight::new(record.finalized_height),
        finalized_hash: decode_block_hash(family, key, &record.finalized_hash)?,
        artifact_schema_version: ArtifactSchemaVersion::new(
            u16::try_from(record.artifact_schema_version).map_err(|_| {
                StoreError::ArtifactCorrupt {
                    family,
                    key: key.clone().into(),
                    reason: "artifact schema version does not fit u16",
                }
            })?,
        ),
        tip_metadata: ChainTipMetadata::new(
            record.sapling_commitment_tree_size,
            record.orchard_commitment_tree_size,
        ),
        created_at: UnixTimestampMillis::new(record.created_at_millis),
    })
}

fn chain_event_record(event: &ChainEvent) -> ChainEventRecord {
    match event {
        ChainEvent::TipAdvanced { committed } => ChainEventRecord {
            event_kind: CHAIN_EVENT_KIND_COMMITTED,
            committed: Some(chain_epoch_committed_record(committed)),
            reverted: None,
        },
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => ChainEventRecord {
            event_kind: CHAIN_EVENT_KIND_REORGED,
            committed: Some(chain_epoch_committed_record(committed)),
            reverted: Some(chain_range_reverted_record(reverted)),
        },
    }
}

fn decode_chain_event_record(
    key: &StoreKey,
    event_record: ChainEventRecord,
) -> Result<ChainEvent, StoreError> {
    match event_record.event_kind {
        CHAIN_EVENT_KIND_COMMITTED => {
            let committed = event_record
                .committed
                .ok_or(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::ChainEvent,
                    key: key.clone().into(),
                    reason: "chain committed event is missing committed range",
                })
                .and_then(|record| decode_chain_epoch_committed_record(key, record))?;

            Ok(ChainEvent::TipAdvanced { committed })
        }
        CHAIN_EVENT_KIND_REORGED => {
            let reverted = event_record
                .reverted
                .ok_or(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::ChainEvent,
                    key: key.clone().into(),
                    reason: "chain reorged event is missing reverted range",
                })
                .and_then(|record| decode_chain_range_reverted_record(key, record))?;
            let committed = event_record
                .committed
                .ok_or(StoreError::ArtifactCorrupt {
                    family: ArtifactFamily::ChainEvent,
                    key: key.clone().into(),
                    reason: "chain reorged event is missing committed range",
                })
                .and_then(|record| decode_chain_epoch_committed_record(key, record))?;

            Ok(ChainEvent::ChainReorged {
                reverted,
                committed,
            })
        }
        _ => Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::ChainEvent,
            key: key.clone().into(),
            reason: "chain event kind is unknown",
        }),
    }
}

fn chain_epoch_committed_record(committed: &ChainEpochCommitted) -> ChainEpochCommittedRecord {
    ChainEpochCommittedRecord {
        chain_epoch: Some(chain_epoch_record(&committed.chain_epoch)),
        block_range: Some(block_height_range_record(committed.block_range)),
    }
}

fn decode_chain_epoch_committed_record(
    key: &StoreKey,
    record: ChainEpochCommittedRecord,
) -> Result<ChainEpochCommitted, StoreError> {
    let chain_epoch_record = record.chain_epoch.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain epoch committed record is missing chain epoch",
    })?;

    let block_range = record.block_range.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain epoch committed record is missing block range",
    })?;

    Ok(ChainEpochCommitted {
        chain_epoch: decode_chain_epoch_record(
            ArtifactFamily::ChainEvent,
            key,
            &chain_epoch_record,
        )?,
        block_range: decode_block_height_range_record(block_range),
    })
}

fn chain_range_reverted_record(reverted: &ChainRangeReverted) -> ChainRangeRevertedRecord {
    ChainRangeRevertedRecord {
        chain_epoch: Some(chain_epoch_record(&reverted.chain_epoch)),
        block_range: Some(block_height_range_record(reverted.block_range)),
    }
}

fn decode_chain_range_reverted_record(
    key: &StoreKey,
    record: ChainRangeRevertedRecord,
) -> Result<ChainRangeReverted, StoreError> {
    let chain_epoch_record = record.chain_epoch.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain range reverted record is missing chain epoch",
    })?;
    let block_range = record.block_range.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::ChainEvent,
        key: key.clone().into(),
        reason: "chain range reverted record is missing block range",
    })?;

    Ok(ChainRangeReverted {
        chain_epoch: decode_chain_epoch_record(
            ArtifactFamily::ChainEvent,
            key,
            &chain_epoch_record,
        )?,
        block_range: decode_block_height_range_record(block_range),
    })
}

const fn block_height_range_record(block_range: BlockHeightRange) -> BlockHeightRangeRecord {
    BlockHeightRangeRecord {
        start_height: block_range.start.value(),
        end_height: block_range.end.value(),
    }
}

const fn decode_block_height_range_record(record: BlockHeightRangeRecord) -> BlockHeightRange {
    BlockHeightRange::inclusive(
        BlockHeight::new(record.start_height),
        BlockHeight::new(record.end_height),
    )
}

fn encode_artifact_record(
    payload_format: PayloadFormat,
    record: &impl Message,
) -> Result<Vec<u8>, StoreError> {
    let record_bytes = record.encode_to_vec();
    ArtifactEnvelopeHeaderV1::encode_payload(payload_format, &record_bytes).map_err(|error| {
        match error {
            ArtifactEnvelopeError::PayloadTooLarge { payload_len } => {
                StoreError::ArtifactPayloadTooLarge {
                    family: artifact_family_for_payload_format(payload_format),
                    payload_len,
                }
            }
            ArtifactEnvelopeError::EnvelopeTooShort { .. }
            | ArtifactEnvelopeError::UnsupportedMagic
            | ArtifactEnvelopeError::UnsupportedEnvelopeVersion { .. }
            | ArtifactEnvelopeError::UnsupportedPayloadFormat { .. }
            | ArtifactEnvelopeError::PayloadFormatMismatch { .. }
            | ArtifactEnvelopeError::UnsupportedCompressionFormat { .. }
            | ArtifactEnvelopeError::UnsupportedChecksumFormat { .. }
            | ArtifactEnvelopeError::PayloadLengthMismatch { .. }
            | ArtifactEnvelopeError::HeaderFieldTruncated { .. } => {
                StoreError::InvalidChainEpochArtifacts {
                    reason: "artifact envelope could not be encoded",
                }
            }
        }
    })
}

const fn artifact_family_for_payload_format(payload_format: PayloadFormat) -> ArtifactFamily {
    match payload_format {
        PayloadFormat::ZinderBlockArtifactV1 => ArtifactFamily::FinalizedBlock,
        PayloadFormat::ZinderCompactBlockArtifactV1 => ArtifactFamily::CompactBlock,
        PayloadFormat::ZinderTransactionArtifactV1 => ArtifactFamily::Transaction,
        PayloadFormat::ZinderTreeStateArtifactV1 => ArtifactFamily::TreeState,
        PayloadFormat::ZinderSubtreeRootArtifactV1 => ArtifactFamily::SubtreeRoot,
        PayloadFormat::ZinderTransparentAddressUtxoArtifactV1 => {
            ArtifactFamily::TransparentAddressUtxo
        }
        PayloadFormat::ZinderTransparentUtxoSpendArtifactV1 => ArtifactFamily::TransparentUtxoSpend,
        PayloadFormat::ZinderTransparentAddressTxIndexArtifactV1 => {
            ArtifactFamily::TransparentAddressTxIndex
        }
    }
}

fn decode_artifact_payload<'a>(
    family: ArtifactFamily,
    key: &StoreKey,
    envelope_bytes: &'a [u8],
    expected_payload_format: PayloadFormat,
) -> Result<&'a [u8], StoreError> {
    ArtifactEnvelopeHeaderV1::decode_payload(envelope_bytes, expected_payload_format).map_err(
        |_| StoreError::ArtifactCorrupt {
            family,
            key: key.clone().into(),
            reason: "artifact envelope is invalid",
        },
    )
}

fn decode_block_hash(
    family: ArtifactFamily,
    key: &StoreKey,
    hash_bytes: &[u8],
) -> Result<BlockHash, StoreError> {
    let hash_bytes = <[u8; 32]>::try_from(hash_bytes).map_err(|_| StoreError::ArtifactCorrupt {
        family,
        key: key.clone().into(),
        reason: "block hash must be 32 bytes",
    })?;

    Ok(BlockHash::from_bytes(hash_bytes))
}

fn decode_transaction_id(
    key: &StoreKey,
    transaction_id_bytes: &[u8],
) -> Result<TransactionId, StoreError> {
    decode_transaction_id_for_family(ArtifactFamily::Transaction, key, transaction_id_bytes)
}

fn decode_transaction_id_for_family(
    family: ArtifactFamily,
    key: &StoreKey,
    transaction_id_bytes: &[u8],
) -> Result<TransactionId, StoreError> {
    let transaction_id_bytes =
        <[u8; 32]>::try_from(transaction_id_bytes).map_err(|_| StoreError::ArtifactCorrupt {
            family,
            key: key.clone().into(),
            reason: "transaction id must be 32 bytes",
        })?;

    Ok(TransactionId::from_bytes(transaction_id_bytes))
}

fn decode_transparent_address_script_hash(
    key: &StoreKey,
    hash_bytes: &[u8],
) -> Result<TransparentAddressScriptHash, StoreError> {
    let hash_bytes = <[u8; 32]>::try_from(hash_bytes).map_err(|_| StoreError::ArtifactCorrupt {
        family: ArtifactFamily::TransparentAddressUtxo,
        key: key.clone().into(),
        reason: "transparent address script hash must be 32 bytes",
    })?;

    Ok(TransparentAddressScriptHash::from_bytes(hash_bytes))
}

fn decode_subtree_root_hash(
    key: &StoreKey,
    root_hash_bytes: &[u8],
) -> Result<SubtreeRootHash, StoreError> {
    let root_hash_bytes =
        <[u8; 32]>::try_from(root_hash_bytes).map_err(|_| StoreError::ArtifactCorrupt {
            family: ArtifactFamily::SubtreeRoot,
            key: key.clone().into(),
            reason: "subtree-root hash must be 32 bytes",
        })?;

    Ok(SubtreeRootHash::from_bytes(root_hash_bytes))
}

#[derive(Clone, PartialEq, Message)]
struct ChainEpochRecord {
    #[prost(uint64, tag = "1")]
    chain_epoch: u64,
    #[prost(uint32, tag = "2")]
    network_id: u32,
    #[prost(uint32, tag = "3")]
    tip_height: u32,
    #[prost(bytes, tag = "4")]
    tip_hash: Vec<u8>,
    #[prost(uint32, tag = "5")]
    finalized_height: u32,
    #[prost(bytes, tag = "6")]
    finalized_hash: Vec<u8>,
    #[prost(uint32, tag = "7")]
    artifact_schema_version: u32,
    #[prost(uint64, tag = "8")]
    created_at_millis: u64,
    #[prost(uint32, tag = "9")]
    sapling_commitment_tree_size: u32,
    #[prost(uint32, tag = "10")]
    orchard_commitment_tree_size: u32,
}

const CHAIN_EVENT_KIND_COMMITTED: u32 = 1;
const CHAIN_EVENT_KIND_REORGED: u32 = 2;

#[derive(Clone, PartialEq, Message)]
struct ChainEventEnvelopeRecord {
    #[prost(uint64, tag = "1")]
    event_sequence: u64,
    #[prost(message, optional, tag = "3")]
    chain_epoch: Option<ChainEpochRecord>,
    #[prost(uint32, tag = "4")]
    finalized_height: u32,
    #[prost(message, optional, tag = "5")]
    event: Option<ChainEventRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct ChainEventRecord {
    #[prost(uint32, tag = "1")]
    event_kind: u32,
    #[prost(message, optional, tag = "2")]
    committed: Option<ChainEpochCommittedRecord>,
    #[prost(message, optional, tag = "3")]
    reverted: Option<ChainRangeRevertedRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct ChainEpochCommittedRecord {
    #[prost(message, optional, tag = "1")]
    chain_epoch: Option<ChainEpochRecord>,
    #[prost(message, optional, tag = "2")]
    block_range: Option<BlockHeightRangeRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct ChainRangeRevertedRecord {
    #[prost(message, optional, tag = "1")]
    chain_epoch: Option<ChainEpochRecord>,
    #[prost(message, optional, tag = "2")]
    block_range: Option<BlockHeightRangeRecord>,
}

#[derive(Clone, Copy, PartialEq, Message)]
struct BlockHeightRangeRecord {
    #[prost(uint32, tag = "1")]
    start_height: u32,
    #[prost(uint32, tag = "2")]
    end_height: u32,
}

#[derive(Clone, PartialEq, Message)]
struct BlockArtifactRecord {
    #[prost(uint32, tag = "1")]
    height: u32,
    #[prost(bytes, tag = "2")]
    hash: Vec<u8>,
    #[prost(bytes, tag = "3")]
    parent_hash: Vec<u8>,
    #[prost(bytes = "bytes", tag = "4")]
    payload_bytes: Bytes,
}

#[derive(Clone, PartialEq, Message)]
struct CompactBlockArtifactRecord {
    #[prost(uint32, tag = "1")]
    height: u32,
    #[prost(bytes, tag = "2")]
    block_hash: Vec<u8>,
    #[prost(bytes = "bytes", tag = "3")]
    payload_bytes: Bytes,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    block_height: u32,
    #[prost(bytes, tag = "3")]
    block_hash: Vec<u8>,
    #[prost(bytes = "bytes", tag = "4")]
    payload_bytes: Bytes,
}

#[derive(Clone, PartialEq, Message)]
struct TreeStateArtifactRecord {
    #[prost(uint32, tag = "1")]
    height: u32,
    #[prost(bytes, tag = "2")]
    block_hash: Vec<u8>,
    #[prost(bytes = "bytes", tag = "3")]
    payload_bytes: Bytes,
}

#[derive(Clone, PartialEq, Message)]
struct SubtreeRootArtifactRecord {
    #[prost(uint32, tag = "1")]
    protocol_id: u32,
    #[prost(uint32, tag = "2")]
    subtree_index: u32,
    #[prost(bytes, tag = "3")]
    root_hash: Vec<u8>,
    #[prost(uint32, tag = "4")]
    completing_block_height: u32,
    #[prost(bytes, tag = "5")]
    completing_block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentAddressUtxoArtifactRecord {
    #[prost(bytes, tag = "1")]
    address_script_hash: Vec<u8>,
    #[prost(bytes = "bytes", tag = "2")]
    script_pub_key: Bytes,
    #[prost(bytes, tag = "3")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "4")]
    output_index: u32,
    #[prost(uint64, tag = "5")]
    value_zat: u64,
    #[prost(uint32, tag = "6")]
    block_height: u32,
    #[prost(bytes, tag = "7")]
    block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentUtxoSpendArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    output_index: u32,
    #[prost(uint32, tag = "3")]
    block_height: u32,
    #[prost(bytes, tag = "4")]
    block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentAddressTxIndexArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(bytes, tag = "2")]
    block_hash: Vec<u8>,
}

const MEMPOOL_EVENT_KIND_ADDED: u32 = 1;
const MEMPOOL_EVENT_KIND_INVALIDATED: u32 = 2;
const MEMPOOL_EVENT_KIND_MINED: u32 = 3;

#[derive(Clone, PartialEq, Message)]
struct MempoolEventEnvelopeRecord {
    #[prost(uint64, tag = "1")]
    event_sequence: u64,
    #[prost(uint64, tag = "2")]
    source_observed_unix_millis: u64,
    #[prost(message, optional, tag = "3")]
    event: Option<MempoolEventRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct MempoolEventRecord {
    #[prost(uint32, tag = "1")]
    event_kind: u32,
    #[prost(message, optional, tag = "2")]
    added: Option<MempoolEntryRecord>,
    #[prost(message, optional, tag = "3")]
    invalidated: Option<MempoolInvalidatedRecord>,
    #[prost(message, optional, tag = "4")]
    mined: Option<MempoolMinedRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct MempoolEntryRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(bytes, tag = "2")]
    auth_digest: Vec<u8>,
    #[prost(bytes, tag = "3")]
    raw_transaction_bytes: Vec<u8>,
    #[prost(bytes, tag = "4")]
    compact_transaction_bytes: Vec<u8>,
    #[prost(uint64, tag = "5")]
    first_seen_unix_millis: u64,
    #[prost(message, optional, tag = "6")]
    first_seen_chain_epoch: Option<ChainEpochRecord>,
    #[prost(message, repeated, tag = "7")]
    transparent_outputs: Vec<TransparentMempoolOutputRecord>,
    #[prost(message, repeated, tag = "8")]
    transparent_spends: Vec<TransparentMempoolSpendRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct MempoolInvalidatedRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    reason_id: u32,
}

#[derive(Clone, PartialEq, Message)]
struct MempoolMinedRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    mined_height: u32,
    #[prost(bytes, tag = "3")]
    block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentMempoolOutputRecord {
    #[prost(bytes, tag = "1")]
    address_script_hash: Vec<u8>,
    #[prost(bytes, tag = "2")]
    script_pub_key: Vec<u8>,
    #[prost(bytes, tag = "3")]
    spending_transaction_id: Vec<u8>,
    #[prost(uint32, tag = "4")]
    output_index: u32,
    #[prost(uint64, tag = "5")]
    value_zat: u64,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentMempoolSpendRecord {
    #[prost(bytes, tag = "1")]
    spent_transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    spent_output_index: u32,
    #[prost(bytes, tag = "3")]
    spending_transaction_id: Vec<u8>,
}
