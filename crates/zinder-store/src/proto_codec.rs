//! Protobuf encoders for store-owned chain-event values.

use thiserror::Error;
use zinder_core::ChainEpoch;
use zinder_proto::v1::wallet;

use crate::{ChainEpochCommitted, ChainEvent, ChainEventEnvelope, ChainRangeReverted};

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
