//! Protobuf encoders and decoders for store-owned chain-event values.

use thiserror::Error;
use zinder_core::{
    ArtifactSchemaVersion, AuthDigest, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, MempoolEntry, MempoolEvictionReason, Network, RawTransactionBytes,
    TransactionId, TransparentAddressScriptHash, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, UnixTimestampMillis,
};
use zinder_proto::v1::wallet;

use crate::{
    ChainEpochCommitted, ChainEvent, ChainEventEnvelope, ChainRangeReverted, MempoolEvent,
    MempoolEventEnvelope, StreamCursorTokenV1,
};

/// Error returned when a store chain event cannot be represented on the v1 wallet proto.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainEventEncodeError {
    /// Store returned a chain-event variant unsupported by the wallet protocol.
    #[error("unsupported chain event: {event}")]
    UnsupportedChainEvent {
        /// Unsupported event description.
        event: &'static str,
    },
}

/// Encodes a store chain-event envelope into the wallet protocol message.
#[allow(
    unreachable_patterns,
    reason = "ChainEvent is expected to grow; this encoder must fail closed for future variants."
)]
pub fn chain_event_envelope_message(
    event_envelope: &ChainEventEnvelope,
) -> Result<wallet::ChainEventEnvelope, ChainEventEncodeError> {
    let event = match &event_envelope.event {
        ChainEvent::ChainCommitted { committed } => {
            wallet::chain_event_envelope::Event::Committed(wallet::ChainCommitted {
                committed: Some(chain_epoch_committed_message(*committed)),
            })
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => wallet::chain_event_envelope::Event::Reorged(wallet::ChainReorged {
            reverted: Some(chain_range_reverted_message(*reverted)),
            committed: Some(chain_epoch_committed_message(*committed)),
        }),
        _ => {
            return Err(ChainEventEncodeError::UnsupportedChainEvent {
                event: "unknown chain event variant",
            });
        }
    };

    Ok(wallet::ChainEventEnvelope {
        cursor: event_envelope.cursor.as_bytes().into(),
        event_sequence: event_envelope.event_sequence,
        chain_epoch: Some(chain_epoch_message(event_envelope.chain_epoch)),
        finalized_height: event_envelope.finalized_height.value(),
        event: Some(event),
    })
}

fn chain_epoch_committed_message(committed: ChainEpochCommitted) -> wallet::ChainEpochCommitted {
    wallet::ChainEpochCommitted {
        chain_epoch: Some(chain_epoch_message(committed.chain_epoch)),
        start_height: committed.block_range.start.value(),
        end_height: committed.block_range.end.value(),
    }
}

fn chain_range_reverted_message(reverted: ChainRangeReverted) -> wallet::ChainRangeReverted {
    wallet::ChainRangeReverted {
        chain_epoch: Some(chain_epoch_message(reverted.chain_epoch)),
        start_height: reverted.block_range.start.value(),
        end_height: reverted.block_range.end.value(),
    }
}

/// Encodes a mempool entry into the wallet protocol message.
#[must_use]
pub fn mempool_entry_message(entry: &MempoolEntry) -> wallet::MempoolEntry {
    wallet::MempoolEntry {
        transaction_id: entry.transaction_id.as_bytes().into(),
        auth_digest: entry
            .auth_digest
            .map(|digest| digest.as_bytes().into())
            .unwrap_or_default(),
        raw_transaction_bytes: entry.raw_transaction_bytes.as_slice().into(),
        compact_transaction_bytes: entry.compact_transaction_bytes.clone(),
        first_seen_unix_millis: entry.first_seen_unix_millis.value(),
        first_seen_chain_epoch: Some(chain_epoch_message(entry.first_seen_chain_epoch)),
        transparent_outputs: entry
            .transparent_outputs
            .iter()
            .map(transparent_mempool_output_message)
            .collect(),
        transparent_spends: entry
            .transparent_spends
            .iter()
            .map(transparent_mempool_spend_message)
            .collect(),
    }
}

/// Encodes a mempool-event envelope into the wallet protocol message.
#[allow(
    unreachable_patterns,
    reason = "MempoolEvent is expected to grow; this encoder must fail closed for future variants."
)]
pub fn mempool_event_envelope_message(
    event_envelope: &MempoolEventEnvelope,
) -> Result<wallet::MempoolEventEnvelope, ChainEventEncodeError> {
    let event = match &event_envelope.event {
        MempoolEvent::Added { entry } => {
            wallet::mempool_event_envelope::Event::Added(wallet::MempoolAddedEvent {
                entry: Some(mempool_entry_message(entry)),
            })
        }
        MempoolEvent::Invalidated {
            transaction_id,
            reason,
        } => wallet::mempool_event_envelope::Event::Invalidated(wallet::MempoolInvalidatedEvent {
            transaction_id: transaction_id.as_bytes().into(),
            reason: mempool_eviction_reason_message(*reason).into(),
        }),
        MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        } => wallet::mempool_event_envelope::Event::Mined(wallet::MempoolMinedEvent {
            transaction_id: transaction_id.as_bytes().into(),
            mined_height: mined_height.value(),
            block_hash: block_hash.as_bytes().into(),
        }),
        _ => {
            return Err(ChainEventEncodeError::UnsupportedChainEvent {
                event: "unknown mempool event variant",
            });
        }
    };

    Ok(wallet::MempoolEventEnvelope {
        cursor: event_envelope.cursor.as_bytes().into(),
        event_sequence: event_envelope.event_sequence,
        source_observed_unix_millis: event_envelope.source_observed_unix_millis,
        event: Some(event),
    })
}

/// Encodes a [`TransparentMempoolOutput`] into the wallet protocol message.
///
/// Public so the writer-side mempool point-lookup adapter can encode
/// responses without re-deriving the field layout.
#[must_use]
pub fn transparent_mempool_output_message(
    transparent_output: &TransparentMempoolOutput,
) -> wallet::TransparentMempoolOutput {
    wallet::TransparentMempoolOutput {
        address_script_hash: transparent_output.address_script_hash.as_bytes().into(),
        script_pub_key: transparent_output.script_pub_key.clone(),
        spending_transaction_id: transparent_output.outpoint.transaction_id.as_bytes().into(),
        output_index: transparent_output.outpoint.output_index,
        value_zat: transparent_output.value_zat,
    }
}

/// Encodes a [`TransparentMempoolSpend`] into the wallet protocol message.
///
/// Public for the same reason as [`transparent_mempool_output_message`].
#[must_use]
pub fn transparent_mempool_spend_message(
    transparent_spend: &TransparentMempoolSpend,
) -> wallet::TransparentMempoolSpend {
    wallet::TransparentMempoolSpend {
        spent_transaction_id: transparent_spend
            .spent_outpoint
            .transaction_id
            .as_bytes()
            .into(),
        spent_output_index: transparent_spend.spent_outpoint.output_index,
        spending_transaction_id: transparent_spend.spending_transaction_id.as_bytes().into(),
    }
}

const fn mempool_eviction_reason_message(
    reason: MempoolEvictionReason,
) -> wallet::MempoolEvictionReason {
    match reason {
        MempoolEvictionReason::Conflict => wallet::MempoolEvictionReason::Conflict,
        MempoolEvictionReason::Expired => wallet::MempoolEvictionReason::Expired,
        MempoolEvictionReason::LowFee => wallet::MempoolEvictionReason::LowFee,
        MempoolEvictionReason::NodeRejected => wallet::MempoolEvictionReason::NodeRejected,
        MempoolEvictionReason::Unknown => wallet::MempoolEvictionReason::Unknown,
        // The Rust enum is non_exhaustive but the wire enum is closed; the
        // store side projects to the closest known reason.
        _ => wallet::MempoolEvictionReason::Unspecified,
    }
}

/// Encodes chain-epoch metadata into the wallet protocol message.
#[must_use]
pub fn chain_epoch_message(chain_epoch: ChainEpoch) -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: chain_epoch.id.value(),
        network_name: chain_epoch.network.name().to_owned(),
        tip_height: chain_epoch.tip_height.value(),
        tip_hash: chain_epoch.tip_hash.as_bytes().into(),
        finalized_height: chain_epoch.finalized_height.value(),
        finalized_hash: chain_epoch.finalized_hash.as_bytes().into(),
        artifact_schema_version: u32::from(chain_epoch.artifact_schema_version.value()),
        created_at_millis: chain_epoch.created_at.value(),
        sapling_commitment_tree_size: chain_epoch.tip_metadata.sapling_commitment_tree_size,
        orchard_commitment_tree_size: chain_epoch.tip_metadata.orchard_commitment_tree_size,
    }
}

/// Failure decoding a wallet-protocol message into canonical types.
///
/// Each variant carries a static `field` path so consumers can map the error
/// to their own domain (e.g. `IndexerError::MalformedResponse`,
/// `MempoolSurfaceError::Unavailable`, `tonic::Status::invalid_argument`)
/// without losing the diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[non_exhaustive]
pub enum MempoolDecodeError {
    /// A required protobuf field was absent.
    #[error("{field} is missing from the wallet protocol message")]
    MissingField {
        /// Static field path that was missing.
        field: &'static str,
    },
    /// A 32-byte hash field carried the wrong length.
    #[error("{field} expected 32 bytes, got {actual}")]
    WrongHashLength {
        /// Static field path that carried the wrong length.
        field: &'static str,
        /// Observed length on the wire.
        actual: usize,
    },
    /// An integer field overflowed the canonical type.
    #[error("{field} does not fit a {target}")]
    Overflow {
        /// Static field path that overflowed.
        field: &'static str,
        /// Target canonical type the value would not fit into.
        target: &'static str,
    },
    /// A network name was unknown.
    #[error("{field}: unknown network name {network_name}")]
    UnknownNetwork {
        /// Static field path that carried the unknown network name.
        field: &'static str,
        /// Observed network name.
        network_name: String,
    },
    /// A mempool eviction reason was unknown.
    #[error("{field}: unknown mempool eviction reason {encoded}")]
    UnknownEvictionReason {
        /// Static field path that carried the unknown encoded reason.
        field: &'static str,
        /// Encoded reason value.
        encoded: i32,
    },
}

impl MempoolDecodeError {
    /// Returns the static field path associated with this decode failure.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        match self {
            Self::MissingField { field }
            | Self::WrongHashLength { field, .. }
            | Self::Overflow { field, .. }
            | Self::UnknownNetwork { field, .. }
            | Self::UnknownEvictionReason { field, .. } => field,
        }
    }
}

/// Decodes a wallet-protocol [`wallet::ChainEpoch`] into the canonical type.
pub fn chain_epoch_from_message(
    message: wallet::ChainEpoch,
) -> Result<ChainEpoch, MempoolDecodeError> {
    let network = Network::from_name(&message.network_name).ok_or_else(|| {
        MempoolDecodeError::UnknownNetwork {
            field: "chain_epoch.network_name",
            network_name: message.network_name.clone(),
        }
    })?;
    let artifact_schema_version = u16::try_from(message.artifact_schema_version).map_err(|_| {
        MempoolDecodeError::Overflow {
            field: "chain_epoch.artifact_schema_version",
            target: "u16",
        }
    })?;
    Ok(ChainEpoch {
        id: ChainEpochId::new(message.chain_epoch_id),
        network,
        tip_height: BlockHeight::new(message.tip_height),
        tip_hash: block_hash_from_bytes("chain_epoch.tip_hash", message.tip_hash)?,
        finalized_height: BlockHeight::new(message.finalized_height),
        finalized_hash: block_hash_from_bytes(
            "chain_epoch.finalized_hash",
            message.finalized_hash,
        )?,
        artifact_schema_version: ArtifactSchemaVersion::new(artifact_schema_version),
        tip_metadata: ChainTipMetadata::new(
            message.sapling_commitment_tree_size,
            message.orchard_commitment_tree_size,
        ),
        created_at: UnixTimestampMillis::new(message.created_at_millis),
    })
}

/// Decodes a wallet-protocol [`wallet::MempoolEntry`] into the canonical type.
pub fn mempool_entry_from_message(
    message: wallet::MempoolEntry,
) -> Result<MempoolEntry, MempoolDecodeError> {
    let transaction_id =
        transaction_id_from_bytes("mempool_entry.transaction_id", message.transaction_id)?;
    let auth_digest = if message.auth_digest.is_empty() {
        None
    } else {
        Some(AuthDigest::from_bytes(fixed_32_bytes(
            "mempool_entry.auth_digest",
            message.auth_digest,
        )?))
    };
    let chain_epoch_message =
        message
            .first_seen_chain_epoch
            .ok_or(MempoolDecodeError::MissingField {
                field: "mempool_entry.first_seen_chain_epoch",
            })?;
    let first_seen_chain_epoch = chain_epoch_from_message(chain_epoch_message)?;
    let transparent_outputs = message
        .transparent_outputs
        .into_iter()
        .map(transparent_mempool_output_from_message)
        .collect::<Result<Vec<_>, _>>()?;
    let transparent_spends = message
        .transparent_spends
        .into_iter()
        .map(transparent_mempool_spend_from_message)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MempoolEntry {
        transaction_id,
        auth_digest,
        raw_transaction_bytes: RawTransactionBytes::new(message.raw_transaction_bytes),
        compact_transaction_bytes: message.compact_transaction_bytes,
        first_seen_unix_millis: UnixTimestampMillis::new(message.first_seen_unix_millis),
        first_seen_chain_epoch,
        transparent_outputs,
        transparent_spends,
    })
}

/// Decodes a wallet-protocol [`wallet::MempoolEventEnvelope`] into the canonical envelope.
pub fn mempool_event_envelope_from_message(
    message: wallet::MempoolEventEnvelope,
) -> Result<MempoolEventEnvelope, MempoolDecodeError> {
    let event_message = message.event.ok_or(MempoolDecodeError::MissingField {
        field: "mempool_event_envelope.event",
    })?;
    let event = match event_message {
        wallet::mempool_event_envelope::Event::Added(added) => {
            let entry_message = added.entry.ok_or(MempoolDecodeError::MissingField {
                field: "mempool_event_envelope.added.entry",
            })?;
            MempoolEvent::Added {
                entry: mempool_entry_from_message(entry_message)?,
            }
        }
        wallet::mempool_event_envelope::Event::Invalidated(invalidated) => {
            MempoolEvent::Invalidated {
                transaction_id: transaction_id_from_bytes(
                    "mempool_event_envelope.invalidated.transaction_id",
                    invalidated.transaction_id,
                )?,
                reason: mempool_eviction_reason_from_message(
                    "mempool_event_envelope.invalidated.reason",
                    invalidated.reason,
                )?,
            }
        }
        wallet::mempool_event_envelope::Event::Mined(mined) => MempoolEvent::Mined {
            transaction_id: transaction_id_from_bytes(
                "mempool_event_envelope.mined.transaction_id",
                mined.transaction_id,
            )?,
            mined_height: BlockHeight::new(mined.mined_height),
            block_hash: block_hash_from_bytes(
                "mempool_event_envelope.mined.block_hash",
                mined.block_hash,
            )?,
        },
    };
    Ok(MempoolEventEnvelope {
        cursor: StreamCursorTokenV1::from_bytes(message.cursor),
        event_sequence: message.event_sequence,
        source_observed_unix_millis: message.source_observed_unix_millis,
        event,
    })
}

/// Decodes a wallet-protocol [`wallet::TransparentMempoolOutput`] into the canonical type.
///
/// Public for the same reason as [`transparent_mempool_output_message`]:
/// writer-side and reader-side adapters share one decoder.
pub fn transparent_mempool_output_from_message(
    message: wallet::TransparentMempoolOutput,
) -> Result<TransparentMempoolOutput, MempoolDecodeError> {
    Ok(TransparentMempoolOutput {
        address_script_hash: TransparentAddressScriptHash::from_bytes(fixed_32_bytes(
            "transparent_output.address_script_hash",
            message.address_script_hash,
        )?),
        script_pub_key: message.script_pub_key,
        outpoint: TransparentOutPoint::new(
            transaction_id_from_bytes(
                "transparent_output.spending_transaction_id",
                message.spending_transaction_id,
            )?,
            message.output_index,
        ),
        value_zat: message.value_zat,
    })
}

/// Decodes a wallet-protocol [`wallet::TransparentMempoolSpend`] into the canonical type.
///
/// Public for the same reason as [`transparent_mempool_spend_message`].
pub fn transparent_mempool_spend_from_message(
    message: wallet::TransparentMempoolSpend,
) -> Result<TransparentMempoolSpend, MempoolDecodeError> {
    Ok(TransparentMempoolSpend {
        spent_outpoint: TransparentOutPoint::new(
            transaction_id_from_bytes(
                "transparent_spend.spent_transaction_id",
                message.spent_transaction_id,
            )?,
            message.spent_output_index,
        ),
        spending_transaction_id: transaction_id_from_bytes(
            "transparent_spend.spending_transaction_id",
            message.spending_transaction_id,
        )?,
    })
}

fn mempool_eviction_reason_from_message(
    field: &'static str,
    encoded: i32,
) -> Result<MempoolEvictionReason, MempoolDecodeError> {
    match wallet::MempoolEvictionReason::try_from(encoded) {
        Ok(wallet::MempoolEvictionReason::Conflict) => Ok(MempoolEvictionReason::Conflict),
        Ok(wallet::MempoolEvictionReason::Expired) => Ok(MempoolEvictionReason::Expired),
        Ok(wallet::MempoolEvictionReason::LowFee) => Ok(MempoolEvictionReason::LowFee),
        Ok(wallet::MempoolEvictionReason::NodeRejected) => Ok(MempoolEvictionReason::NodeRejected),
        Ok(wallet::MempoolEvictionReason::Unknown | wallet::MempoolEvictionReason::Unspecified) => {
            Ok(MempoolEvictionReason::Unknown)
        }
        Err(_) => Err(MempoolDecodeError::UnknownEvictionReason { field, encoded }),
    }
}

fn transaction_id_from_bytes(
    field: &'static str,
    bytes: Vec<u8>,
) -> Result<TransactionId, MempoolDecodeError> {
    Ok(TransactionId::from_bytes(fixed_32_bytes(field, bytes)?))
}

fn block_hash_from_bytes(
    field: &'static str,
    bytes: Vec<u8>,
) -> Result<BlockHash, MempoolDecodeError> {
    Ok(BlockHash::from_bytes(fixed_32_bytes(field, bytes)?))
}

fn fixed_32_bytes(field: &'static str, bytes: Vec<u8>) -> Result<[u8; 32], MempoolDecodeError> {
    let actual = bytes.len();
    bytes
        .try_into()
        .map_err(|_| MempoolDecodeError::WrongHashLength { field, actual })
}
