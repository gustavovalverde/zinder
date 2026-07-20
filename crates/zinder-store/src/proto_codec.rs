//! Protobuf encoders and decoders for store-owned chain-event values.

use thiserror::Error;
use zinder_core::wire::{
    decode_rpc_auth_digest_hex, decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex,
    decode_zinder_native_chain_name, encode_rpc_auth_digest_hex, encode_rpc_block_hash_hex,
    encode_rpc_transaction_id_hex, encode_zinder_native_chain_name,
};
use zinder_core::{
    ArtifactSchemaVersion, AuthDigest, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, MempoolEntry, MempoolEvictionReason, RawTransactionBytes, TransactionId,
    TransparentAddressScriptHash, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, TransparentOutput, TransparentOutputEntry, TransparentSpendEntry,
    UnixTimestampMillis,
};
use zinder_proto::v1::wallet;

use crate::{
    ChainEpochCommitted, ChainEvent, ChainEventEnvelope, ChainEventStreamFamily,
    ChainRangeReverted, EventStreamStartPosition, MempoolEvent, MempoolEventEnvelope,
    MempoolEventStreamFamily, StreamCursorTokenV1,
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
            wallet::chain_event_envelope::Event::ChainCommitted(wallet::ChainCommitted {
                committed: Some(chain_epoch_committed_message(*committed)),
            })
        }
        ChainEvent::ChainReorged {
            reverted,
            committed,
        } => wallet::chain_event_envelope::Event::ChainReorged(wallet::ChainReorged {
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
        chain_view: Some(chain_view_message(event_envelope.chain_epoch)),
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
        transaction_id: encode_rpc_transaction_id_hex(entry.transaction_id),
        auth_digest: entry
            .auth_digest
            .map(encode_rpc_auth_digest_hex)
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
            transaction_id: encode_rpc_transaction_id_hex(*transaction_id),
            reason: mempool_eviction_reason_message(*reason).into(),
        }),
        MempoolEvent::Mined {
            transaction_id,
            mined_height,
            block_hash,
        } => wallet::mempool_event_envelope::Event::Mined(wallet::MempoolMinedEvent {
            transaction_id: encode_rpc_transaction_id_hex(*transaction_id),
            mined_height: mined_height.value(),
            block_hash: encode_rpc_block_hash_hex(*block_hash),
        }),
        MempoolEvent::Suppressed { transaction_id } => {
            wallet::mempool_event_envelope::Event::Suppressed(wallet::MempoolSuppressedEvent {
                transaction_id: encode_rpc_transaction_id_hex(*transaction_id),
            })
        }
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

/// Encodes a [`TransparentOutPoint`] into the wallet protocol message.
///
/// Public so every wallet-plane outpoint-keyed RPC encodes through one
/// canonical helper instead of re-deriving `(transaction_id, output_index)`.
#[must_use]
pub fn outpoint_message(outpoint: &TransparentOutPoint) -> wallet::OutPoint {
    wallet::OutPoint {
        transaction_id: encode_rpc_transaction_id_hex(outpoint.transaction_id),
        output_index: outpoint.output_index,
    }
}

/// Encodes a [`TransparentOutput`] into the wallet protocol message.
#[must_use]
pub fn transparent_output_message(output: TransparentOutput) -> wallet::TransparentOutput {
    wallet::TransparentOutput {
        value_zat: output.value_zat,
        script_pub_key: output.script_pub_key,
    }
}

/// Encodes a [`TransparentOutputEntry`] into the wallet protocol message.
///
/// Used by both the canonical `WalletQuery.TransparentOutputsByOutpoint` surface and
/// the live-mempool `WalletQuery.TransparentMempoolOutputsByOutpoint` surface, so the
/// two sides share one wire shape and one encoder.
#[must_use]
pub fn transparent_output_entry_message(
    entry: TransparentOutputEntry,
) -> wallet::TransparentOutputEntry {
    wallet::TransparentOutputEntry {
        outpoint: Some(outpoint_message(&entry.outpoint)),
        output: entry.output.map(transparent_output_message),
    }
}

/// Encodes a [`TransparentSpendEntry`] into the wallet protocol message.
///
/// The canonical reverse-spend resolver `WalletQuery.TransparentSpendsByOutpoint`
/// projects each found spend through this one encoder so the outpoint, spending
/// transaction id, input index, and spending-block hash all route through the
/// shared wire helpers.
#[must_use]
pub fn transparent_spend_message(entry: &TransparentSpendEntry) -> wallet::TransparentSpend {
    wallet::TransparentSpend {
        spent_outpoint: Some(outpoint_message(&entry.spent_outpoint)),
        spending_transaction_id: encode_rpc_transaction_id_hex(entry.spending_transaction_id),
        input_index: entry.input_index,
        spending_block: Some(block_tip_message(
            entry.spending_block_height,
            entry.spending_block_hash,
        )),
    }
}

/// Decodes a stream cursor from the bytes carried by a request message.
///
/// Empty bytes encode "no cursor" (start at the beginning of the stream);
/// every other value materializes a [`StreamCursorTokenV1`] without
/// validating its envelope. Validation happens at the seek site.
#[must_use]
pub fn stream_cursor_from_message_bytes(cursor_bytes: Vec<u8>) -> Option<StreamCursorTokenV1> {
    if cursor_bytes.is_empty() {
        None
    } else {
        Some(StreamCursorTokenV1::from_bytes(cursor_bytes))
    }
}

/// Decodes the chain-event stream family carried by a request message.
///
/// The wire field is the integer encoding of `wallet::ChainEventStreamFamily`.
/// Returns `None` for any unknown integer; callers map that to an
/// `INVALID_ARGUMENT` diagnostic at the transport boundary.
#[must_use]
pub fn chain_event_stream_family_from_message(family: i32) -> Option<ChainEventStreamFamily> {
    match wallet::ChainEventStreamFamily::try_from(family) {
        Ok(wallet::ChainEventStreamFamily::Tip) => Some(ChainEventStreamFamily::Tip),
        Ok(wallet::ChainEventStreamFamily::Safe) => Some(ChainEventStreamFamily::Safe),
        Err(_) => None,
    }
}

/// Decodes the mempool-event stream family carried by a request message.
///
/// `Unspecified` resolves to the single `Mempool` family; unknown integers
/// return `None` and map to `INVALID_ARGUMENT` at the transport boundary.
#[must_use]
pub fn mempool_event_stream_family_from_message(family: i32) -> Option<MempoolEventStreamFamily> {
    match wallet::MempoolEventStreamFamily::try_from(family) {
        Ok(
            wallet::MempoolEventStreamFamily::Unspecified
            | wallet::MempoolEventStreamFamily::Mempool,
        ) => Some(MempoolEventStreamFamily::Mempool),
        Err(_) => None,
    }
}

/// Decodes the wire `EventStreamStart` into the typed start position.
///
/// Returns `None` when the message or its `position` oneof is unset, and
/// when `after_cursor` carries empty bytes; callers map `None` to
/// `INVALID_ARGUMENT` at the transport boundary.
#[must_use]
pub fn event_stream_start_from_message(
    start: Option<wallet::EventStreamStart>,
) -> Option<EventStreamStartPosition> {
    match start?.position? {
        wallet::event_stream_start::Position::AfterCursor(cursor_bytes) => {
            stream_cursor_from_message_bytes(cursor_bytes)
                .map(EventStreamStartPosition::AfterCursor)
        }
        wallet::event_stream_start::Position::EarliestRetained(_) => {
            Some(EventStreamStartPosition::EarliestRetained)
        }
        wallet::event_stream_start::Position::LiveTail(_) => {
            Some(EventStreamStartPosition::LiveTail)
        }
    }
}

/// Encodes a typed start position into the wire `EventStreamStart` message.
#[must_use]
pub fn event_stream_start_message(start: &EventStreamStartPosition) -> wallet::EventStreamStart {
    let position = match start {
        EventStreamStartPosition::AfterCursor(cursor) => {
            wallet::event_stream_start::Position::AfterCursor(cursor.as_bytes().to_vec())
        }
        EventStreamStartPosition::EarliestRetained => {
            wallet::event_stream_start::Position::EarliestRetained(wallet::EarliestRetained {})
        }
        EventStreamStartPosition::LiveTail => {
            wallet::event_stream_start::Position::LiveTail(wallet::LiveTail {})
        }
    };
    wallet::EventStreamStart {
        position: Some(position),
    }
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
        outpoint: Some(outpoint_message(&transparent_output.outpoint)),
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
        spent_outpoint: Some(outpoint_message(&transparent_spend.spent_outpoint)),
        spending_transaction_id: encode_rpc_transaction_id_hex(
            transparent_spend.spending_transaction_id,
        ),
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

/// Encodes one named chain tip (height plus RPC-form block hash) into the
/// wallet protocol message.
#[must_use]
pub fn block_tip_message(height: BlockHeight, hash: BlockHash) -> wallet::BlockTip {
    wallet::BlockTip {
        height: height.value(),
        hash: encode_rpc_block_hash_hex(hash),
    }
}

/// Encodes chain-epoch metadata into the wallet protocol message.
#[must_use]
pub fn chain_epoch_message(chain_epoch: ChainEpoch) -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: chain_epoch.id.value(),
        network_name: encode_zinder_native_chain_name(chain_epoch.network).to_owned(),
        artifact_schema_version: u32::from(chain_epoch.artifact_schema_version.value()),
        created_at_millis: chain_epoch.created_at.value(),
        visible_tip: Some(block_tip_message(
            chain_epoch.visible_tip_height,
            chain_epoch.visible_tip_hash,
        )),
        settled_tip: Some(block_tip_message(
            chain_epoch.settled_tip_height,
            chain_epoch.settled_tip_hash,
        )),
        sapling_commitment_tree_size: chain_epoch.tip_metadata.sapling_commitment_tree_size,
        orchard_commitment_tree_size: chain_epoch.tip_metadata.orchard_commitment_tree_size,
        ironwood_commitment_tree_size: chain_epoch.tip_metadata.ironwood_commitment_tree_size,
    }
}

/// Wraps chain-epoch metadata in the cross-plane [`wallet::ChainView`] envelope.
///
/// Leaves the materialized-view axes (`indexed_tip`, `upstream_tip`, `materialized_views`)
/// absent. The wallet plane owns the epoch; the explorer and ingest planes fill
/// the remaining axes from their own state.
#[must_use]
pub fn chain_view_message(chain_epoch: ChainEpoch) -> wallet::ChainView {
    wallet::ChainView {
        chain_epoch: Some(chain_epoch_message(chain_epoch)),
        indexed_tip: None,
        upstream_tip: None,
        materialized_views: None,
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
    /// An RPC-form hex hash field failed to decode.
    #[error("{field}: invalid RPC-form hex hash: {reason}")]
    InvalidRpcHashHex {
        /// Static field path that carried the bad hex.
        field: &'static str,
        /// Human-readable description of the decode failure.
        reason: String,
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
            | Self::InvalidRpcHashHex { field, .. }
            | Self::Overflow { field, .. }
            | Self::UnknownNetwork { field, .. }
            | Self::UnknownEvictionReason { field, .. } => field,
        }
    }
}

/// Decodes a wallet-protocol [`wallet::ChainEpoch`] into the canonical type.
#[allow(
    clippy::needless_pass_by_value,
    reason = "ChainEpoch fields are all small POD; taking by value keeps caller ergonomics symmetric with the message constructor and avoids a borrow-vs-clone choice at every callsite."
)]
pub fn chain_epoch_from_message(
    message: wallet::ChainEpoch,
) -> Result<ChainEpoch, MempoolDecodeError> {
    let network = decode_zinder_native_chain_name(&message.network_name)
        .ok()
        .ok_or_else(|| MempoolDecodeError::UnknownNetwork {
            field: "chain_epoch.network_name",
            network_name: message.network_name.clone(),
        })?;
    let artifact_schema_version = u16::try_from(message.artifact_schema_version).map_err(|_| {
        MempoolDecodeError::Overflow {
            field: "chain_epoch.artifact_schema_version",
            target: "u16",
        }
    })?;
    let visible_tip = message
        .visible_tip
        .ok_or(MempoolDecodeError::MissingField {
            field: "chain_epoch.visible_tip",
        })?;
    let settled_tip = message
        .settled_tip
        .ok_or(MempoolDecodeError::MissingField {
            field: "chain_epoch.settled_tip",
        })?;
    Ok(ChainEpoch {
        id: ChainEpochId::new(message.chain_epoch_id),
        network,
        visible_tip_height: BlockHeight::new(visible_tip.height),
        visible_tip_hash: block_hash_from_rpc_hex(
            "chain_epoch.visible_tip.hash",
            &visible_tip.hash,
        )?,
        settled_tip_height: BlockHeight::new(settled_tip.height),
        settled_tip_hash: block_hash_from_rpc_hex(
            "chain_epoch.settled_tip.hash",
            &settled_tip.hash,
        )?,
        artifact_schema_version: ArtifactSchemaVersion::new(artifact_schema_version),
        tip_metadata: ChainTipMetadata::new(
            message.sapling_commitment_tree_size,
            message.orchard_commitment_tree_size,
            message.ironwood_commitment_tree_size,
        ),
        created_at: UnixTimestampMillis::new(message.created_at_millis),
    })
}

/// Decodes a wallet-protocol [`wallet::MempoolEntry`] into the canonical type.
pub fn mempool_entry_from_message(
    message: wallet::MempoolEntry,
) -> Result<MempoolEntry, MempoolDecodeError> {
    let transaction_id =
        transaction_id_from_rpc_hex("mempool_entry.transaction_id", &message.transaction_id)?;
    let auth_digest = if message.auth_digest.is_empty() {
        None
    } else {
        Some(auth_digest_from_rpc_hex(
            "mempool_entry.auth_digest",
            &message.auth_digest,
        )?)
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
                transaction_id: transaction_id_from_rpc_hex(
                    "mempool_event_envelope.invalidated.transaction_id",
                    &invalidated.transaction_id,
                )?,
                reason: mempool_eviction_reason_from_message(
                    "mempool_event_envelope.invalidated.reason",
                    invalidated.reason,
                )?,
            }
        }
        wallet::mempool_event_envelope::Event::Mined(mined) => MempoolEvent::Mined {
            transaction_id: transaction_id_from_rpc_hex(
                "mempool_event_envelope.mined.transaction_id",
                &mined.transaction_id,
            )?,
            mined_height: BlockHeight::new(mined.mined_height),
            block_hash: block_hash_from_rpc_hex(
                "mempool_event_envelope.mined.block_hash",
                &mined.block_hash,
            )?,
        },
        wallet::mempool_event_envelope::Event::Suppressed(suppressed) => MempoolEvent::Suppressed {
            transaction_id: transaction_id_from_rpc_hex(
                "mempool_event_envelope.suppressed.transaction_id",
                &suppressed.transaction_id,
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

/// Decodes a wallet-protocol [`wallet::OutPoint`] into the canonical type.
///
/// `field` is the field path of the outpoint message itself (used in
/// diagnostics if `transaction_id` carries the wrong length). Callers
/// pre-unwrap the `Option<wallet::OutPoint>` and emit
/// [`MempoolDecodeError::MissingField`] with their own precise path when
/// the message is absent.
#[allow(
    clippy::needless_pass_by_value,
    reason = "OutPoint is a tiny POD; taking by value keeps caller ergonomics symmetric with the message constructor."
)]
pub fn outpoint_from_message(
    field: &'static str,
    message: wallet::OutPoint,
) -> Result<TransparentOutPoint, MempoolDecodeError> {
    Ok(TransparentOutPoint::new(
        transaction_id_from_rpc_hex(field, &message.transaction_id)?,
        message.output_index,
    ))
}

/// Decodes a wallet-protocol [`wallet::TransparentMempoolOutput`] into the canonical type.
///
/// Public for the same reason as [`transparent_mempool_output_message`]:
/// writer-side and reader-side adapters share one decoder.
pub fn transparent_mempool_output_from_message(
    message: wallet::TransparentMempoolOutput,
) -> Result<TransparentMempoolOutput, MempoolDecodeError> {
    let outpoint_message = message.outpoint.ok_or(MempoolDecodeError::MissingField {
        field: "transparent_output.outpoint",
    })?;
    Ok(TransparentMempoolOutput {
        address_script_hash: TransparentAddressScriptHash::from_bytes(fixed_32_bytes(
            "transparent_output.address_script_hash",
            message.address_script_hash,
        )?),
        script_pub_key: message.script_pub_key,
        outpoint: outpoint_from_message("transparent_output.outpoint", outpoint_message)?,
        value_zat: message.value_zat,
    })
}

/// Decodes a wallet-protocol [`wallet::TransparentMempoolSpend`] into the canonical type.
///
/// Public for the same reason as [`transparent_mempool_spend_message`].
pub fn transparent_mempool_spend_from_message(
    message: wallet::TransparentMempoolSpend,
) -> Result<TransparentMempoolSpend, MempoolDecodeError> {
    let spent_outpoint_message =
        message
            .spent_outpoint
            .ok_or(MempoolDecodeError::MissingField {
                field: "transparent_spend.spent_outpoint",
            })?;
    Ok(TransparentMempoolSpend {
        spent_outpoint: outpoint_from_message(
            "transparent_spend.spent_outpoint",
            spent_outpoint_message,
        )?,
        spending_transaction_id: transaction_id_from_rpc_hex(
            "transparent_spend.spending_transaction_id",
            &message.spending_transaction_id,
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

fn transaction_id_from_rpc_hex(
    field: &'static str,
    rpc_hex: &str,
) -> Result<TransactionId, MempoolDecodeError> {
    decode_rpc_transaction_id_hex(rpc_hex).map_err(|error| MempoolDecodeError::InvalidRpcHashHex {
        field,
        reason: error.to_string(),
    })
}

fn block_hash_from_rpc_hex(
    field: &'static str,
    rpc_hex: &str,
) -> Result<BlockHash, MempoolDecodeError> {
    decode_rpc_block_hash_hex(rpc_hex).map_err(|error| MempoolDecodeError::InvalidRpcHashHex {
        field,
        reason: error.to_string(),
    })
}

fn auth_digest_from_rpc_hex(
    field: &'static str,
    rpc_hex: &str,
) -> Result<AuthDigest, MempoolDecodeError> {
    decode_rpc_auth_digest_hex(rpc_hex).map_err(|error| MempoolDecodeError::InvalidRpcHashHex {
        field,
        reason: error.to_string(),
    })
}

fn fixed_32_bytes(field: &'static str, bytes: Vec<u8>) -> Result<[u8; 32], MempoolDecodeError> {
    let actual = bytes.len();
    bytes
        .try_into()
        .map_err(|_| MempoolDecodeError::WrongHashLength { field, actual })
}
