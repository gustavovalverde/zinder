//! Protobuf and envelope codecs for persisted chain records.

use bytes::Bytes;
use prost::Message;
use zinder_core::{
    ArtifactSchemaVersion, AuthDigest, BlockBlobArtifact, BlockHash, BlockHeaderArtifact,
    BlockHeight, BlockHeightRange, BlockTransactionIndexArtifact, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, ConsensusBranchId, LockTime, MempoolEntry,
    MempoolEvictionReason, Network, PrivacyShape, RawTransactionBytes, ShieldedProtocol,
    SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, TransactionBlobArtifact,
    TransactionComponentCounts, TransactionFactsArtifact, TransactionId, TransactionLocation,
    TransactionPublicFacts, TransactionVersion, TransparentAddressScriptHash,
    TransparentAddressTxIndexArtifact, TransparentInputFact, TransparentMempoolOutput,
    TransparentMempoolSpend, TransparentOutPoint, TransparentOutputArtifact, TransparentOutputFact,
    TransparentSpendFact, TransparentUnspentOutput, TreeStateArtifact, UnixTimestampMillis,
    UnsupportedSection, Wtxid,
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
        safe_tip_height: event_envelope.safe_tip_height.value(),
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
        BlockHeight::new(record.safe_tip_height),
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
    Suppressed,
}

impl MempoolEventKind {
    const fn from_kind(kind: u32) -> Option<Self> {
        match kind {
            MEMPOOL_EVENT_KIND_ADDED => Some(Self::Added),
            MEMPOOL_EVENT_KIND_INVALIDATED => Some(Self::Invalidated),
            MEMPOOL_EVENT_KIND_MINED => Some(Self::Mined),
            MEMPOOL_EVENT_KIND_SUPPRESSED => Some(Self::Suppressed),
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
            suppressed: None,
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
            suppressed: None,
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
            suppressed: None,
        },
        MempoolEvent::Suppressed { transaction_id } => MempoolEventRecord {
            event_kind: MEMPOOL_EVENT_KIND_SUPPRESSED,
            added: None,
            invalidated: None,
            mined: None,
            suppressed: Some(MempoolSuppressedRecord {
                transaction_id: transaction_id.as_bytes().to_vec(),
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
        MempoolEventKind::Suppressed => {
            let suppressed = record.suppressed.ok_or(StoreError::ArtifactCorrupt {
                family: ArtifactFamily::MempoolEvent,
                key: key.clone().into(),
                reason: "suppressed mempool event is missing payload",
            })?;
            Ok(MempoolEvent::Suppressed {
                transaction_id: decode_transaction_id_for_family(
                    ArtifactFamily::MempoolEvent,
                    key,
                    &suppressed.transaction_id,
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

pub(crate) fn encode_block_header_artifact(
    block: &BlockHeaderArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderBlockHeaderArtifactV1,
        &BlockHeaderArtifactRecord {
            height: block.height.value(),
            block_hash: block.block_hash.as_bytes().to_vec(),
            parent_hash: block.parent_hash.as_bytes().to_vec(),
            merkle_root_hash: block.merkle_root_hash.to_vec(),
            commitment_bytes: block.commitment_bytes.to_vec(),
            block_time: block.block_time,
            bits: block.bits,
            nonce: block.nonce.to_vec(),
            version: block.version,
            block_size_bytes: block.block_size_bytes,
        },
    )
}

pub(crate) fn decode_block_header_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<BlockHeaderArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::BlockHeader,
        key,
        envelope_bytes,
        PayloadFormat::ZinderBlockHeaderArtifactV1,
    )?;
    let record = BlockHeaderArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockHeader,
            key: key.clone().into(),
            reason: "block header artifact record is not valid protobuf",
        }
    })?;

    Ok(BlockHeaderArtifact::new(
        BlockHeight::new(record.height),
        decode_block_hash(ArtifactFamily::BlockHeader, key, &record.block_hash)?,
        decode_block_hash(ArtifactFamily::BlockHeader, key, &record.parent_hash)?,
        decode_fixed_32(
            ArtifactFamily::BlockHeader,
            key,
            &record.merkle_root_hash,
            "merkle root hash",
        )?,
        decode_fixed_32(
            ArtifactFamily::BlockHeader,
            key,
            &record.commitment_bytes,
            "commitment bytes",
        )?,
        record.block_time,
        record.bits,
        decode_fixed_32(ArtifactFamily::BlockHeader, key, &record.nonce, "nonce")?,
        record.version,
        record.block_size_bytes,
    ))
}

pub(crate) fn encode_block_blob_artifact(block: BlockBlobArtifact) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderBlockBlobArtifactV1,
        &BlockBlobArtifactRecord {
            height: block.height.value(),
            block_hash: block.block_hash.as_bytes().to_vec(),
            parent_hash: block.parent_hash.as_bytes().to_vec(),
            raw_block_bytes: Bytes::from(block.raw_block_bytes),
        },
    )
}

pub(crate) fn decode_block_blob_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<BlockBlobArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::BlockBlob,
        key,
        envelope_bytes,
        PayloadFormat::ZinderBlockBlobArtifactV1,
    )?;
    let record = BlockBlobArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockBlob,
            key: key.clone().into(),
            reason: "block blob artifact record is not valid protobuf",
        }
    })?;

    Ok(BlockBlobArtifact::new(
        BlockHeight::new(record.height),
        decode_block_hash(ArtifactFamily::BlockBlob, key, &record.block_hash)?,
        decode_block_hash(ArtifactFamily::BlockBlob, key, &record.parent_hash)?,
        record.raw_block_bytes.to_vec(),
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

pub(crate) fn encode_block_transaction_index_artifact(
    artifact: BlockTransactionIndexArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderBlockTransactionIndexArtifactV1,
        &BlockTransactionIndexArtifactRecord {
            block_height: artifact.block_height.value(),
            tx_index_in_block: artifact.tx_index_in_block,
            transaction_id: artifact.transaction_id.as_bytes().to_vec(),
            block_hash: artifact.block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_block_transaction_index_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<BlockTransactionIndexArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::BlockTransactionIndex,
        key,
        envelope_bytes,
        PayloadFormat::ZinderBlockTransactionIndexArtifactV1,
    )?;
    let record = BlockTransactionIndexArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::BlockTransactionIndex,
            key: key.clone().into(),
            reason: "block transaction-index artifact record is not valid protobuf",
        }
    })?;

    Ok(BlockTransactionIndexArtifact::new(
        BlockHeight::new(record.block_height),
        record.tx_index_in_block,
        decode_transaction_id_for_family(
            ArtifactFamily::BlockTransactionIndex,
            key,
            &record.transaction_id,
        )?,
        decode_block_hash(
            ArtifactFamily::BlockTransactionIndex,
            key,
            &record.block_hash,
        )?,
    ))
}

pub(crate) fn encode_transaction_location_artifact(
    location: TransactionLocation,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransactionLocationArtifactV1,
        &TransactionLocationArtifactRecord {
            transaction_id: location.transaction_id.as_bytes().to_vec(),
            block_height: location.block_height.value(),
            block_hash: location.block_hash.as_bytes().to_vec(),
            tx_index_in_block: location.tx_index_in_block,
        },
    )
}

pub(crate) fn decode_transaction_location_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransactionLocation, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransactionLocation,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransactionLocationArtifactV1,
    )?;
    let record = TransactionLocationArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionLocation,
            key: key.clone().into(),
            reason: "transaction location artifact record is not valid protobuf",
        }
    })?;

    Ok(TransactionLocation::new(
        decode_transaction_id_for_family(
            ArtifactFamily::TransactionLocation,
            key,
            &record.transaction_id,
        )?,
        BlockHeight::new(record.block_height),
        decode_block_hash(ArtifactFamily::TransactionLocation, key, &record.block_hash)?,
        record.tx_index_in_block,
    ))
}

pub(crate) fn encode_transaction_facts_artifact(
    artifact: TransactionFactsArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransactionFactsArtifactV1,
        &transaction_facts_artifact_record(artifact),
    )
}

pub(crate) fn decode_transaction_facts_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransactionFactsArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransactionFacts,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransactionFactsArtifactV1,
    )?;
    let record = TransactionFactsArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionFacts,
            key: key.clone().into(),
            reason: "transaction facts artifact record is not valid protobuf",
        }
    })?;
    decode_transaction_facts_artifact_record(key, record)
}

pub(crate) fn encode_transaction_blob_artifact(
    artifact: TransactionBlobArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransactionBlobArtifactV1,
        &TransactionBlobArtifactRecord {
            transaction_id: artifact.location.transaction_id.as_bytes().to_vec(),
            block_height: artifact.location.block_height.value(),
            block_hash: artifact.location.block_hash.as_bytes().to_vec(),
            tx_index_in_block: artifact.location.tx_index_in_block,
            raw_transaction_bytes: Bytes::from(artifact.raw_transaction_bytes),
        },
    )
}

pub(crate) fn decode_transaction_blob_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransactionBlobArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransactionBlob,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransactionBlobArtifactV1,
    )?;
    let record = TransactionBlobArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionBlob,
            key: key.clone().into(),
            reason: "transaction blob artifact record is not valid protobuf",
        }
    })?;

    Ok(TransactionBlobArtifact::new(
        TransactionLocation::new(
            decode_transaction_id_for_family(
                ArtifactFamily::TransactionBlob,
                key,
                &record.transaction_id,
            )?,
            BlockHeight::new(record.block_height),
            decode_block_hash(ArtifactFamily::TransactionBlob, key, &record.block_hash)?,
            record.tx_index_in_block,
        ),
        record.raw_transaction_bytes.to_vec(),
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

pub(crate) fn encode_address_output_index_artifact(
    output: TransparentUnspentOutput,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentUnspentOutputV1,
        &TransparentUnspentOutputRecord {
            address_script_hash: output.address_script_hash.as_bytes().to_vec(),
            script_pub_key: Bytes::from(output.script_pub_key),
            transaction_id: output.outpoint.transaction_id.as_bytes().to_vec(),
            output_index: output.outpoint.output_index,
            value_zat: output.value_zat,
            block_height: output.block_height.value(),
            block_hash: output.block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_address_output_index_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<TransparentUnspentOutput, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::AddressOutputIndex,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentUnspentOutputV1,
    )?;
    let record = TransparentUnspentOutputRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::AddressOutputIndex,
            key: key.clone().into(),
            reason: "transparent address output artifact record is not valid protobuf",
        }
    })?;

    Ok(TransparentUnspentOutput::new(
        decode_transparent_address_script_hash(key, &record.address_script_hash)?,
        record.script_pub_key.to_vec(),
        TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::AddressOutputIndex,
                key,
                &record.transaction_id,
            )?,
            record.output_index,
        ),
        record.value_zat,
        BlockHeight::new(record.block_height),
        decode_block_hash(ArtifactFamily::AddressOutputIndex, key, &record.block_hash)?,
    ))
}

pub(crate) fn encode_transparent_output_artifact(
    artifact: TransparentOutputArtifact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentOutputArtifactV1,
        &TransparentOutputArtifactRecord {
            transaction_id: artifact.outpoint.transaction_id.as_bytes().to_vec(),
            output_index: artifact.outpoint.output_index,
            value_zat: artifact.value_zat,
            script_pub_key: Bytes::from(artifact.script_pub_key),
            address_script_hash: artifact.address_script_hash.as_bytes().to_vec(),
            block_height: artifact.block_height.value(),
            block_hash: artifact.block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_transparent_output_artifact(
    key: &StoreKey,
    envelope_bytes: &[u8],
    outpoint: TransparentOutPoint,
) -> Result<TransparentOutputArtifact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentOutput,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentOutputArtifactV1,
    )?;
    let record = TransparentOutputArtifactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentOutput,
            key: key.clone().into(),
            reason: "transparent output artifact record is not valid protobuf",
        }
    })?;

    let decoded_outpoint = TransparentOutPoint::new(
        decode_transaction_id_for_family(
            ArtifactFamily::TransparentOutput,
            key,
            &record.transaction_id,
        )?,
        record.output_index,
    );
    if decoded_outpoint != outpoint {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentOutput,
            key: key.clone().into(),
            reason: "transparent output artifact outpoint does not match its key",
        });
    }

    Ok(TransparentOutputArtifact::new(
        outpoint,
        record.value_zat,
        record.script_pub_key.to_vec(),
        decode_transparent_address_script_hash(key, &record.address_script_hash)?,
        BlockHeight::new(record.block_height),
        decode_block_hash(ArtifactFamily::TransparentOutput, key, &record.block_hash)?,
    ))
}

pub(crate) fn encode_transparent_output_block_index(
    block_hash: BlockHash,
    outpoints: &[TransparentOutPoint],
) -> Result<Vec<u8>, StoreError> {
    let outpoints = outpoints
        .iter()
        .map(|outpoint| TransparentOutPointRecord {
            transaction_id: outpoint.transaction_id.as_bytes().to_vec(),
            output_index: outpoint.output_index,
        })
        .collect::<Vec<_>>();

    encode_artifact_record(
        PayloadFormat::ZinderTransparentOutputBlockIndexV1,
        &TransparentOutputBlockIndexRecord {
            block_hash: block_hash.as_bytes().to_vec(),
            outpoints,
        },
    )
}

pub(crate) fn decode_transparent_output_block_index(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<(BlockHash, Vec<TransparentOutPoint>), StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentOutput,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentOutputBlockIndexV1,
    )?;
    let record = TransparentOutputBlockIndexRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentOutput,
            key: key.clone().into(),
            reason: "transparent spend index record is not valid protobuf",
        }
    })?;
    let block_hash = decode_block_hash(ArtifactFamily::TransparentOutput, key, &record.block_hash)?;
    let mut outpoints = Vec::with_capacity(record.outpoints.len());
    for outpoint in record.outpoints {
        outpoints.push(TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::TransparentOutput,
                key,
                &outpoint.transaction_id,
            )?,
            outpoint.output_index,
        ));
    }

    Ok((block_hash, outpoints))
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

pub(crate) fn encode_transparent_spend_fact(
    spend: &TransparentSpendFact,
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentSpendFactV2,
        &TransparentSpendFactRecord {
            transaction_id: spend.spent_outpoint.transaction_id.as_bytes().to_vec(),
            output_index: spend.spent_outpoint.output_index,
            input_index: spend.input_index,
            spending_transaction_id: spend.spending_transaction_id.as_bytes().to_vec(),
            tx_index_in_block: spend.tx_index_in_block,
            block_height: spend.block_height.value(),
            block_hash: spend.block_hash.as_bytes().to_vec(),
            spent_value_zat: spend.spent_value_zat,
            spent_address_script_hash: spend.spent_address_script_hash.as_bytes().to_vec(),
            spent_block_height: spend.spent_block_height.value(),
            spent_block_hash: spend.spent_block_hash.as_bytes().to_vec(),
        },
    )
}

pub(crate) fn decode_transparent_spend_fact(
    key: &StoreKey,
    envelope_bytes: &[u8],
    outpoint: TransparentOutPoint,
) -> Result<TransparentSpendFact, StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentSpendFact,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentSpendFactV2,
    )?;
    let record = TransparentSpendFactRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentSpendFact,
            key: key.clone().into(),
            reason: "transparent spend fact record is not valid protobuf",
        }
    })?;

    let decoded_outpoint = TransparentOutPoint::new(
        decode_transaction_id_for_family(
            ArtifactFamily::TransparentSpendFact,
            key,
            &record.transaction_id,
        )?,
        record.output_index,
    );
    if decoded_outpoint != outpoint {
        return Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentSpendFact,
            key: key.clone().into(),
            reason: "transparent spend fact outpoint does not match its key",
        });
    }

    Ok(TransparentSpendFact {
        spent_outpoint: outpoint,
        input_index: record.input_index,
        spending_transaction_id: decode_transaction_id_for_family(
            ArtifactFamily::TransparentSpendFact,
            key,
            &record.spending_transaction_id,
        )?,
        tx_index_in_block: record.tx_index_in_block,
        block_height: BlockHeight::new(record.block_height),
        block_hash: decode_block_hash(
            ArtifactFamily::TransparentSpendFact,
            key,
            &record.block_hash,
        )?,
        spent_value_zat: record.spent_value_zat,
        spent_address_script_hash: decode_transparent_address_script_hash(
            key,
            &record.spent_address_script_hash,
        )?,
        spent_block_height: BlockHeight::new(record.spent_block_height),
        spent_block_hash: decode_block_hash(
            ArtifactFamily::TransparentSpendFact,
            key,
            &record.spent_block_hash,
        )?,
    })
}

pub(crate) fn encode_transparent_spend_fact_block_index(
    block_hash: BlockHash,
    spent_outpoints: &[TransparentOutPoint],
) -> Result<Vec<u8>, StoreError> {
    encode_artifact_record(
        PayloadFormat::ZinderTransparentSpendFactBlockIndexV1,
        &TransparentOutputBlockIndexRecord {
            block_hash: block_hash.as_bytes().to_vec(),
            outpoints: spent_outpoints
                .iter()
                .map(|outpoint| TransparentOutPointRecord {
                    transaction_id: outpoint.transaction_id.as_bytes().to_vec(),
                    output_index: outpoint.output_index,
                })
                .collect(),
        },
    )
}

pub(crate) fn decode_transparent_spend_fact_block_index(
    key: &StoreKey,
    envelope_bytes: &[u8],
) -> Result<(BlockHash, Vec<TransparentOutPoint>), StoreError> {
    let payload_bytes = decode_artifact_payload(
        ArtifactFamily::TransparentSpendFact,
        key,
        envelope_bytes,
        PayloadFormat::ZinderTransparentSpendFactBlockIndexV1,
    )?;
    let record = TransparentOutputBlockIndexRecord::decode(payload_bytes).map_err(|_| {
        StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransparentSpendFact,
            key: key.clone().into(),
            reason: "transparent spend fact block index record is not valid protobuf",
        }
    })?;
    let block_hash = decode_block_hash(
        ArtifactFamily::TransparentSpendFact,
        key,
        &record.block_hash,
    )?;
    let mut outpoints = Vec::with_capacity(record.outpoints.len());
    for outpoint in record.outpoints {
        outpoints.push(TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::TransparentSpendFact,
                key,
                &outpoint.transaction_id,
            )?,
            outpoint.output_index,
        ));
    }
    Ok((block_hash, outpoints))
}

fn transaction_facts_artifact_record(
    artifact: TransactionFactsArtifact,
) -> TransactionFactsArtifactRecord {
    let facts = artifact.public_facts;
    TransactionFactsArtifactRecord {
        transaction_id: artifact.location.transaction_id.as_bytes().to_vec(),
        block_height: artifact.location.block_height.value(),
        block_hash: artifact.location.block_hash.as_bytes().to_vec(),
        tx_index_in_block: artifact.location.tx_index_in_block,
        auth_digest: facts
            .auth_digest
            .map_or_else(Vec::new, |digest| digest.as_bytes().to_vec()),
        wtxid: facts
            .wtxid
            .map_or_else(Vec::new, |wtxid| wtxid.as_bytes().to_vec()),
        transaction_version: Some(transaction_version_record(facts.version)),
        consensus_branch_id: facts.consensus_branch_id.map(ConsensusBranchId::value),
        lock_time: Some(lock_time_record(facts.lock_time)),
        expiry_height: facts.expiry_height.map(BlockHeight::value),
        size_bytes: facts.size_bytes,
        counts: Some(transaction_component_counts_record(facts.counts)),
        privacy_shape: privacy_shape_id(facts.privacy_shape),
        is_coinbase: facts.is_coinbase,
        unsupported_sections: facts
            .unsupported_sections
            .into_iter()
            .map(unsupported_section_id)
            .collect(),
        transparent_inputs: artifact
            .transparent_inputs
            .into_iter()
            .map(transparent_input_fact_record)
            .collect(),
        transparent_outputs: artifact
            .transparent_outputs
            .into_iter()
            .map(transparent_output_fact_record)
            .collect(),
    }
}

fn decode_transaction_facts_artifact_record(
    key: &StoreKey,
    record: TransactionFactsArtifactRecord,
) -> Result<TransactionFactsArtifact, StoreError> {
    let location = TransactionLocation::new(
        decode_transaction_id_for_family(
            ArtifactFamily::TransactionFacts,
            key,
            &record.transaction_id,
        )?,
        BlockHeight::new(record.block_height),
        decode_block_hash(ArtifactFamily::TransactionFacts, key, &record.block_hash)?,
        record.tx_index_in_block,
    );
    let counts = record.counts.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::TransactionFacts,
        key: key.clone().into(),
        reason: "transaction facts record is missing component counts",
    })?;
    let public_facts = TransactionPublicFacts {
        transaction_id: location.transaction_id,
        auth_digest: decode_optional_auth_digest(
            ArtifactFamily::TransactionFacts,
            key,
            &record.auth_digest,
        )?,
        wtxid: decode_optional_wtxid(key, &record.wtxid)?,
        version: decode_transaction_version_record(key, record.transaction_version)?,
        consensus_branch_id: record.consensus_branch_id.map(ConsensusBranchId::new),
        lock_time: decode_lock_time_record(key, record.lock_time)?,
        expiry_height: record.expiry_height.map(BlockHeight::new),
        size_bytes: record.size_bytes,
        counts: decode_transaction_component_counts_record(counts),
        privacy_shape: decode_privacy_shape_id(key, record.privacy_shape)?,
        is_coinbase: record.is_coinbase,
        unsupported_sections: record
            .unsupported_sections
            .into_iter()
            .map(|section_id| decode_unsupported_section_id(key, section_id))
            .collect::<Result<Vec<_>, _>>()?,
    };

    let transparent_inputs = record
        .transparent_inputs
        .iter()
        .map(|input| decode_transparent_input_fact_record(key, input))
        .collect::<Result<Vec<_>, _>>()?;
    let transparent_outputs = record
        .transparent_outputs
        .iter()
        .map(|output| decode_transparent_output_fact_record(key, output))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TransactionFactsArtifact::new(location, public_facts)
        .with_transparent_facts(transparent_inputs, transparent_outputs))
}

fn transparent_input_fact_record(input: TransparentInputFact) -> TransparentInputFactRecord {
    TransparentInputFactRecord {
        input_index: input.input_index,
        spent_transaction_id: input.spent_outpoint.transaction_id.as_bytes().to_vec(),
        spent_output_index: input.spent_outpoint.output_index,
    }
}

fn decode_transparent_input_fact_record(
    key: &StoreKey,
    record: &TransparentInputFactRecord,
) -> Result<TransparentInputFact, StoreError> {
    Ok(TransparentInputFact::new(
        record.input_index,
        TransparentOutPoint::new(
            decode_transaction_id_for_family(
                ArtifactFamily::TransactionFacts,
                key,
                &record.spent_transaction_id,
            )?,
            record.spent_output_index,
        ),
    ))
}

fn transparent_output_fact_record(output: TransparentOutputFact) -> TransparentOutputFactRecord {
    TransparentOutputFactRecord {
        output_index: output.output_index,
        value_zat: output.value_zat,
        script_pub_key: Bytes::from(output.script_pub_key),
        address_script_hash: output.address_script_hash.as_bytes().to_vec(),
    }
}

fn decode_transparent_output_fact_record(
    key: &StoreKey,
    record: &TransparentOutputFactRecord,
) -> Result<TransparentOutputFact, StoreError> {
    Ok(TransparentOutputFact::new(
        record.output_index,
        record.value_zat,
        record.script_pub_key.to_vec(),
        decode_transparent_address_script_hash(key, &record.address_script_hash)?,
    ))
}

const TRANSACTION_VERSION_KIND_V1: u32 = 1;
const TRANSACTION_VERSION_KIND_V2: u32 = 2;
const TRANSACTION_VERSION_KIND_V3: u32 = 3;
const TRANSACTION_VERSION_KIND_V4: u32 = 4;
const TRANSACTION_VERSION_KIND_V5: u32 = 5;
const TRANSACTION_VERSION_KIND_UNSUPPORTED: u32 = 6;

const fn transaction_version_record(version: TransactionVersion) -> TransactionVersionRecord {
    match version {
        TransactionVersion::V1 => TransactionVersionRecord {
            kind: TRANSACTION_VERSION_KIND_V1,
            effective_version: 1,
            version_group_id: None,
        },
        TransactionVersion::V2 => TransactionVersionRecord {
            kind: TRANSACTION_VERSION_KIND_V2,
            effective_version: 2,
            version_group_id: None,
        },
        TransactionVersion::V3 => TransactionVersionRecord {
            kind: TRANSACTION_VERSION_KIND_V3,
            effective_version: 3,
            version_group_id: None,
        },
        TransactionVersion::V4 => TransactionVersionRecord {
            kind: TRANSACTION_VERSION_KIND_V4,
            effective_version: 4,
            version_group_id: None,
        },
        TransactionVersion::V5 => TransactionVersionRecord {
            kind: TRANSACTION_VERSION_KIND_V5,
            effective_version: 5,
            version_group_id: None,
        },
        TransactionVersion::Unsupported {
            effective_version,
            version_group_id,
        } => TransactionVersionRecord {
            kind: TRANSACTION_VERSION_KIND_UNSUPPORTED,
            effective_version,
            version_group_id,
        },
    }
}

fn decode_transaction_version_record(
    key: &StoreKey,
    record: Option<TransactionVersionRecord>,
) -> Result<TransactionVersion, StoreError> {
    let record = record.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::TransactionFacts,
        key: key.clone().into(),
        reason: "transaction facts record is missing transaction version",
    })?;
    match record.kind {
        TRANSACTION_VERSION_KIND_V1 => Ok(TransactionVersion::V1),
        TRANSACTION_VERSION_KIND_V2 => Ok(TransactionVersion::V2),
        TRANSACTION_VERSION_KIND_V3 => Ok(TransactionVersion::V3),
        TRANSACTION_VERSION_KIND_V4 => Ok(TransactionVersion::V4),
        TRANSACTION_VERSION_KIND_V5 => Ok(TransactionVersion::V5),
        TRANSACTION_VERSION_KIND_UNSUPPORTED => Ok(TransactionVersion::Unsupported {
            effective_version: record.effective_version,
            version_group_id: record.version_group_id,
        }),
        _ => Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionFacts,
            key: key.clone().into(),
            reason: "transaction version kind is unknown",
        }),
    }
}

const LOCK_TIME_KIND_UNLOCKED: u32 = 1;
const LOCK_TIME_KIND_HEIGHT: u32 = 2;
const LOCK_TIME_KIND_UNIX_SECONDS: u32 = 3;

const fn lock_time_record(lock_time: LockTime) -> LockTimeRecord {
    match lock_time {
        LockTime::Unlocked => LockTimeRecord {
            kind: LOCK_TIME_KIND_UNLOCKED,
            value: 0,
        },
        LockTime::Height(height) => LockTimeRecord {
            kind: LOCK_TIME_KIND_HEIGHT,
            value: height.value() as u64,
        },
        LockTime::UnixSeconds(seconds) => LockTimeRecord {
            kind: LOCK_TIME_KIND_UNIX_SECONDS,
            value: seconds,
        },
    }
}

fn decode_lock_time_record(
    key: &StoreKey,
    record: Option<LockTimeRecord>,
) -> Result<LockTime, StoreError> {
    let record = record.ok_or(StoreError::ArtifactCorrupt {
        family: ArtifactFamily::TransactionFacts,
        key: key.clone().into(),
        reason: "transaction facts record is missing lock time",
    })?;
    match record.kind {
        LOCK_TIME_KIND_UNLOCKED => Ok(LockTime::Unlocked),
        LOCK_TIME_KIND_HEIGHT => {
            let height = u32::try_from(record.value).map_err(|_| StoreError::ArtifactCorrupt {
                family: ArtifactFamily::TransactionFacts,
                key: key.clone().into(),
                reason: "transaction lock-time height does not fit u32",
            })?;
            Ok(LockTime::Height(BlockHeight::new(height)))
        }
        LOCK_TIME_KIND_UNIX_SECONDS => Ok(LockTime::UnixSeconds(record.value)),
        _ => Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionFacts,
            key: key.clone().into(),
            reason: "transaction lock-time kind is unknown",
        }),
    }
}

const fn transaction_component_counts_record(
    counts: TransactionComponentCounts,
) -> TransactionComponentCountsRecord {
    TransactionComponentCountsRecord {
        transparent_input_count: counts.transparent_input_count,
        transparent_output_count: counts.transparent_output_count,
        sapling_spend_count: counts.sapling_spend_count,
        sapling_output_count: counts.sapling_output_count,
        orchard_action_count: counts.orchard_action_count,
        sprout_joinsplit_count: counts.sprout_joinsplit_count,
    }
}

const fn decode_transaction_component_counts_record(
    record: TransactionComponentCountsRecord,
) -> TransactionComponentCounts {
    TransactionComponentCounts {
        transparent_input_count: record.transparent_input_count,
        transparent_output_count: record.transparent_output_count,
        sapling_spend_count: record.sapling_spend_count,
        sapling_output_count: record.sapling_output_count,
        orchard_action_count: record.orchard_action_count,
        sprout_joinsplit_count: record.sprout_joinsplit_count,
    }
}

const fn privacy_shape_id(privacy_shape: PrivacyShape) -> u32 {
    match privacy_shape {
        PrivacyShape::TransparentOnly => 1,
        PrivacyShape::Shielding => 2,
        PrivacyShape::Deshielding => 3,
        PrivacyShape::ShieldedOnly => 4,
        PrivacyShape::Mixed => 5,
        PrivacyShape::Coinbase => 6,
        PrivacyShape::ShieldedCoinbase => 7,
        PrivacyShape::Unclassified => 8,
    }
}

fn decode_privacy_shape_id(
    key: &StoreKey,
    privacy_shape_id: u32,
) -> Result<PrivacyShape, StoreError> {
    match privacy_shape_id {
        1 => Ok(PrivacyShape::TransparentOnly),
        2 => Ok(PrivacyShape::Shielding),
        3 => Ok(PrivacyShape::Deshielding),
        4 => Ok(PrivacyShape::ShieldedOnly),
        5 => Ok(PrivacyShape::Mixed),
        6 => Ok(PrivacyShape::Coinbase),
        7 => Ok(PrivacyShape::ShieldedCoinbase),
        8 => Ok(PrivacyShape::Unclassified),
        _ => Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionFacts,
            key: key.clone().into(),
            reason: "transaction privacy shape is unknown",
        }),
    }
}

const fn unsupported_section_id(section: UnsupportedSection) -> u32 {
    match section {
        UnsupportedSection::FutureVersionHeader => 1,
        UnsupportedSection::FutureShieldedProtocol => 2,
        _ => 0,
    }
}

fn decode_unsupported_section_id(
    key: &StoreKey,
    section_id: u32,
) -> Result<UnsupportedSection, StoreError> {
    match section_id {
        1 => Ok(UnsupportedSection::FutureVersionHeader),
        2 => Ok(UnsupportedSection::FutureShieldedProtocol),
        _ => Err(StoreError::ArtifactCorrupt {
            family: ArtifactFamily::TransactionFacts,
            key: key.clone().into(),
            reason: "transaction unsupported section id is unknown",
        }),
    }
}

fn decode_optional_auth_digest(
    family: ArtifactFamily,
    key: &StoreKey,
    digest_bytes: &[u8],
) -> Result<Option<AuthDigest>, StoreError> {
    if digest_bytes.is_empty() {
        return Ok(None);
    }
    let bytes = decode_fixed_32(family, key, digest_bytes, "auth digest must be 32 bytes")?;
    Ok(Some(AuthDigest::from_bytes(bytes)))
}

fn decode_optional_wtxid(key: &StoreKey, wtxid_bytes: &[u8]) -> Result<Option<Wtxid>, StoreError> {
    if wtxid_bytes.is_empty() {
        return Ok(None);
    }
    let bytes = <[u8; 64]>::try_from(wtxid_bytes).map_err(|_| StoreError::ArtifactCorrupt {
        family: ArtifactFamily::TransactionFacts,
        key: key.clone().into(),
        reason: "wtxid must be 64 bytes",
    })?;
    Ok(Some(Wtxid::from_bytes(bytes)))
}

fn chain_epoch_record(chain_epoch: &ChainEpoch) -> ChainEpochRecord {
    ChainEpochRecord {
        chain_epoch: chain_epoch.id.value(),
        network_id: chain_epoch.network.id(),
        tip_height: chain_epoch.tip_height.value(),
        tip_hash: chain_epoch.tip_hash.as_bytes().to_vec(),
        safe_tip_height: chain_epoch.safe_tip_height.value(),
        safe_tip_hash: chain_epoch.safe_tip_hash.as_bytes().to_vec(),
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
        safe_tip_height: BlockHeight::new(record.safe_tip_height),
        safe_tip_hash: decode_block_hash(family, key, &record.safe_tip_hash)?,
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
        ChainEvent::ChainCommitted { committed } => ChainEventRecord {
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

            Ok(ChainEvent::ChainCommitted { committed })
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
        PayloadFormat::ZinderBlockHeaderArtifactV1 => ArtifactFamily::BlockHeader,
        PayloadFormat::ZinderCompactBlockArtifactV1 => ArtifactFamily::CompactBlock,
        PayloadFormat::ZinderTransactionFactsArtifactV1 => ArtifactFamily::TransactionFacts,
        PayloadFormat::ZinderTreeStateArtifactV1 => ArtifactFamily::TreeState,
        PayloadFormat::ZinderSubtreeRootArtifactV1 => ArtifactFamily::SubtreeRoot,
        PayloadFormat::ZinderTransparentUnspentOutputV1 => ArtifactFamily::AddressOutputIndex,
        PayloadFormat::ZinderTransparentSpendFactV2
        | PayloadFormat::ZinderTransparentSpendFactBlockIndexV1 => {
            ArtifactFamily::TransparentSpendFact
        }
        PayloadFormat::ZinderTransparentAddressTxIndexArtifactV1 => {
            ArtifactFamily::TransparentAddressTxIndex
        }
        PayloadFormat::ZinderTransparentOutputArtifactV1
        | PayloadFormat::ZinderTransparentOutputBlockIndexV1 => ArtifactFamily::TransparentOutput,
        PayloadFormat::ZinderBlockBlobArtifactV1 => ArtifactFamily::BlockBlob,
        PayloadFormat::ZinderBlockTransactionIndexArtifactV1 => {
            ArtifactFamily::BlockTransactionIndex
        }
        PayloadFormat::ZinderTransactionLocationArtifactV1 => ArtifactFamily::TransactionLocation,
        PayloadFormat::ZinderTransactionBlobArtifactV1 => ArtifactFamily::TransactionBlob,
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

fn decode_fixed_32(
    family: ArtifactFamily,
    key: &StoreKey,
    bytes: &[u8],
    field_name: &'static str,
) -> Result<[u8; 32], StoreError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| StoreError::ArtifactCorrupt {
        family,
        key: key.clone().into(),
        reason: field_name,
    })
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
        family: ArtifactFamily::AddressOutputIndex,
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
    safe_tip_height: u32,
    #[prost(bytes, tag = "6")]
    safe_tip_hash: Vec<u8>,
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
    safe_tip_height: u32,
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
struct BlockHeaderArtifactRecord {
    #[prost(uint32, tag = "1")]
    height: u32,
    #[prost(bytes, tag = "2")]
    block_hash: Vec<u8>,
    #[prost(bytes, tag = "3")]
    parent_hash: Vec<u8>,
    #[prost(bytes, tag = "4")]
    merkle_root_hash: Vec<u8>,
    #[prost(bytes, tag = "5")]
    commitment_bytes: Vec<u8>,
    #[prost(int64, tag = "6")]
    block_time: i64,
    #[prost(uint32, tag = "7")]
    bits: u32,
    #[prost(bytes, tag = "8")]
    nonce: Vec<u8>,
    #[prost(uint32, tag = "9")]
    version: u32,
    #[prost(uint64, tag = "10")]
    block_size_bytes: u64,
}

#[derive(Clone, PartialEq, Message)]
struct BlockBlobArtifactRecord {
    #[prost(uint32, tag = "1")]
    height: u32,
    #[prost(bytes, tag = "2")]
    block_hash: Vec<u8>,
    #[prost(bytes, tag = "3")]
    parent_hash: Vec<u8>,
    #[prost(bytes = "bytes", tag = "4")]
    raw_block_bytes: Bytes,
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
struct BlockTransactionIndexArtifactRecord {
    #[prost(uint32, tag = "1")]
    block_height: u32,
    #[prost(uint32, tag = "2")]
    tx_index_in_block: u32,
    #[prost(bytes, tag = "3")]
    transaction_id: Vec<u8>,
    #[prost(bytes, tag = "4")]
    block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionLocationArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    block_height: u32,
    #[prost(bytes, tag = "3")]
    block_hash: Vec<u8>,
    #[prost(uint32, tag = "4")]
    tx_index_in_block: u32,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionFactsArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    block_height: u32,
    #[prost(bytes, tag = "3")]
    block_hash: Vec<u8>,
    #[prost(uint32, tag = "4")]
    tx_index_in_block: u32,
    #[prost(bytes, tag = "5")]
    auth_digest: Vec<u8>,
    #[prost(bytes, tag = "6")]
    wtxid: Vec<u8>,
    #[prost(message, optional, tag = "7")]
    transaction_version: Option<TransactionVersionRecord>,
    #[prost(uint32, optional, tag = "8")]
    consensus_branch_id: Option<u32>,
    #[prost(message, optional, tag = "9")]
    lock_time: Option<LockTimeRecord>,
    #[prost(uint32, optional, tag = "10")]
    expiry_height: Option<u32>,
    #[prost(uint32, tag = "11")]
    size_bytes: u32,
    #[prost(message, optional, tag = "12")]
    counts: Option<TransactionComponentCountsRecord>,
    #[prost(uint32, tag = "13")]
    privacy_shape: u32,
    #[prost(bool, tag = "14")]
    is_coinbase: bool,
    #[prost(uint32, repeated, tag = "15")]
    unsupported_sections: Vec<u32>,
    #[prost(message, repeated, tag = "16")]
    transparent_inputs: Vec<TransparentInputFactRecord>,
    #[prost(message, repeated, tag = "17")]
    transparent_outputs: Vec<TransparentOutputFactRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentInputFactRecord {
    #[prost(uint32, tag = "1")]
    input_index: u32,
    #[prost(bytes, tag = "2")]
    spent_transaction_id: Vec<u8>,
    #[prost(uint32, tag = "3")]
    spent_output_index: u32,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentOutputFactRecord {
    #[prost(uint32, tag = "1")]
    output_index: u32,
    #[prost(uint64, tag = "2")]
    value_zat: u64,
    #[prost(bytes = "bytes", tag = "3")]
    script_pub_key: Bytes,
    #[prost(bytes, tag = "4")]
    address_script_hash: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Message)]
struct TransactionVersionRecord {
    #[prost(uint32, tag = "1")]
    kind: u32,
    #[prost(uint32, tag = "2")]
    effective_version: u32,
    #[prost(uint32, optional, tag = "3")]
    version_group_id: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Message)]
struct LockTimeRecord {
    #[prost(uint32, tag = "1")]
    kind: u32,
    #[prost(uint64, tag = "2")]
    value: u64,
}

#[allow(
    clippy::struct_field_names,
    reason = "encoded protobuf field names mirror the canonical transaction facts vocabulary"
)]
#[derive(Clone, Copy, PartialEq, Message)]
struct TransactionComponentCountsRecord {
    #[prost(uint32, tag = "1")]
    transparent_input_count: u32,
    #[prost(uint32, tag = "2")]
    transparent_output_count: u32,
    #[prost(uint32, tag = "3")]
    sapling_spend_count: u32,
    #[prost(uint32, tag = "4")]
    sapling_output_count: u32,
    #[prost(uint32, tag = "5")]
    orchard_action_count: u32,
    #[prost(uint32, tag = "6")]
    sprout_joinsplit_count: u32,
}

#[derive(Clone, PartialEq, Message)]
struct TransactionBlobArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    block_height: u32,
    #[prost(bytes, tag = "3")]
    block_hash: Vec<u8>,
    #[prost(uint32, tag = "4")]
    tx_index_in_block: u32,
    #[prost(bytes = "bytes", tag = "5")]
    raw_transaction_bytes: Bytes,
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
struct TransparentUnspentOutputRecord {
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
struct TransparentSpendFactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    output_index: u32,
    #[prost(uint32, tag = "3")]
    input_index: u32,
    #[prost(bytes, tag = "4")]
    spending_transaction_id: Vec<u8>,
    #[prost(uint32, tag = "5")]
    tx_index_in_block: u32,
    #[prost(uint32, tag = "6")]
    block_height: u32,
    #[prost(bytes, tag = "7")]
    block_hash: Vec<u8>,
    #[prost(uint64, tag = "8")]
    spent_value_zat: u64,
    #[prost(bytes, tag = "9")]
    spent_address_script_hash: Vec<u8>,
    #[prost(uint32, tag = "10")]
    spent_block_height: u32,
    #[prost(bytes, tag = "11")]
    spent_block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentAddressTxIndexArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(bytes, tag = "2")]
    block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentOutputArtifactRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    output_index: u32,
    #[prost(uint64, tag = "3")]
    value_zat: u64,
    #[prost(bytes = "bytes", tag = "4")]
    script_pub_key: Bytes,
    #[prost(bytes, tag = "5")]
    address_script_hash: Vec<u8>,
    #[prost(uint32, tag = "6")]
    block_height: u32,
    #[prost(bytes, tag = "7")]
    block_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentOutputBlockIndexRecord {
    #[prost(bytes, tag = "1")]
    block_hash: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    outpoints: Vec<TransparentOutPointRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct TransparentOutPointRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
    #[prost(uint32, tag = "2")]
    output_index: u32,
}

const MEMPOOL_EVENT_KIND_ADDED: u32 = 1;
const MEMPOOL_EVENT_KIND_INVALIDATED: u32 = 2;
const MEMPOOL_EVENT_KIND_MINED: u32 = 3;
const MEMPOOL_EVENT_KIND_SUPPRESSED: u32 = 4;

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
    #[prost(message, optional, tag = "5")]
    suppressed: Option<MempoolSuppressedRecord>,
}

#[derive(Clone, PartialEq, Message)]
struct MempoolSuppressedRecord {
    #[prost(bytes, tag = "1")]
    transaction_id: Vec<u8>,
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
