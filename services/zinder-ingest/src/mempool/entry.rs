//! Mempool entry hydration from source observations.
//!
//! Translates a source-observed [`MempoolSourceEntry`] into the canonical
//! public [`MempoolEntry`] consumed by the live `MempoolIndex` and by the
//! mempool surface RPCs. The translation:
//!
//! 1. Parses the raw transaction bytes via `zebra-chain` so the same
//!    consensus parser used by chain ingestion drives mempool hydration.
//! 2. Builds the lightwalletd-compatible compact transaction bytes once,
//!    on the ingest write side, so the lightwalletd compatibility adapter
//!    can serve `GetMempoolTx` without re-parsing on the read path.
//! 3. Extracts the transparent outputs and outpoint spends needed for the
//!    `transparent_mempool_outputs_by_address` and
//!    `transparent_mempool_spend_by_outpoint` lookups.
//! 4. Stamps the visible [`ChainEpoch`] supplied by ingest at observation
//!    time. The chain epoch is the one stored alongside the source entry's
//!    wall-clock observation time, not the chain epoch at the moment a
//!    consumer reads the entry.

use prost::Message;
use thiserror::Error;
use zebra_chain::serialization::ZcashDeserializeInto;
use zebra_chain::transaction::Transaction as ZebraTransaction;
use zebra_chain::transparent::Input as ZebraTransparentInput;
use zinder_core::{
    ChainEpoch, MempoolEntry, TransactionId, TransparentAddressScriptHash,
    TransparentMempoolOutput, TransparentMempoolSpend, TransparentOutPoint,
};
use zinder_source::MempoolSourceEntry;

use crate::artifact_builder::compact_transaction;

/// Error returned while finalizing a [`MempoolEntry`] from a source entry.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MempoolEntryBuildError {
    /// Raw transaction bytes did not parse as a Zcash transaction.
    #[error("mempool transaction parse failed: {source}")]
    TransactionParseFailed {
        /// Underlying parse error.
        #[source]
        source: zebra_chain::serialization::SerializationError,
    },

    /// Building the lightwalletd-compatible compact transaction failed.
    #[error("mempool compact-transaction build failed: {source}")]
    CompactTransactionBuildFailed {
        /// Underlying derive error from `artifact_builder`.
        #[source]
        source: crate::ArtifactDeriveError,
    },

    /// The transaction reports more transparent outputs than `u32::MAX`.
    #[error("mempool transparent output index overflowed u32")]
    TransparentOutputIndexOverflow,
}

/// Finalizes a source-observed mempool entry into the canonical public
/// [`MempoolEntry`].
pub fn build_mempool_entry(
    source_entry: MempoolSourceEntry,
    visible_chain_epoch: ChainEpoch,
) -> Result<MempoolEntry, MempoolEntryBuildError> {
    let parsed_transaction: ZebraTransaction = source_entry
        .raw_transaction_bytes
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|source| MempoolEntryBuildError::TransactionParseFailed { source })?;

    let compact_tx = compact_transaction(0, &parsed_transaction)
        .map_err(|source| MempoolEntryBuildError::CompactTransactionBuildFailed { source })?;
    let compact_transaction_bytes = compact_tx.encode_to_vec();

    let transparent_outputs =
        build_transparent_mempool_outputs(&parsed_transaction, source_entry.transaction_id)?;
    let transparent_spends =
        build_transparent_mempool_spends(&parsed_transaction, source_entry.transaction_id);

    Ok(MempoolEntry {
        transaction_id: source_entry.transaction_id,
        auth_digest: source_entry.auth_digest,
        raw_transaction_bytes: source_entry.raw_transaction_bytes,
        compact_transaction_bytes,
        first_seen_unix_millis: source_entry.observed_at_unix_millis,
        first_seen_chain_epoch: visible_chain_epoch,
        transparent_outputs,
        transparent_spends,
    })
}

fn build_transparent_mempool_outputs(
    parsed_transaction: &ZebraTransaction,
    transaction_id: TransactionId,
) -> Result<Vec<TransparentMempoolOutput>, MempoolEntryBuildError> {
    let mut transparent_outputs = Vec::with_capacity(parsed_transaction.outputs().len());
    for (output_index, transparent_output) in parsed_transaction.outputs().iter().enumerate() {
        let output_index = u32::try_from(output_index)
            .map_err(|_| MempoolEntryBuildError::TransparentOutputIndexOverflow)?;
        let script_pub_key = transparent_output.lock_script.as_raw_bytes().to_vec();
        transparent_outputs.push(TransparentMempoolOutput {
            address_script_hash: TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
            script_pub_key,
            outpoint: TransparentOutPoint::new(transaction_id, output_index),
            value_zat: u64::from(transparent_output.value()),
        });
    }
    Ok(transparent_outputs)
}

fn build_transparent_mempool_spends(
    parsed_transaction: &ZebraTransaction,
    spending_transaction_id: TransactionId,
) -> Vec<TransparentMempoolSpend> {
    parsed_transaction
        .inputs()
        .iter()
        .filter_map(|transparent_input| match transparent_input {
            ZebraTransparentInput::PrevOut { outpoint, .. } => Some(TransparentMempoolSpend {
                spent_outpoint: TransparentOutPoint::new(
                    TransactionId::from_bytes(outpoint.hash.0),
                    outpoint.index,
                ),
                spending_transaction_id,
            }),
            ZebraTransparentInput::Coinbase { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::{MempoolEntryBuildError, build_mempool_entry};
    use zinder_core::{
        AuthDigest, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, Network,
        RawTransactionBytes, TransactionId, UnixTimestampMillis,
    };
    use zinder_source::MempoolSourceEntry;
    use zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION;

    fn synthetic_chain_epoch() -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(1),
            network: Network::ZcashRegtest,
            tip_height: BlockHeight::new(100),
            tip_hash: BlockHash::from_bytes([0x42; 32]),
            safe_tip_height: BlockHeight::new(100),
            safe_tip_hash: BlockHash::from_bytes([0x42; 32]),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_700_000_000_000),
        }
    }

    #[test]
    fn build_mempool_entry_rejects_unparseable_bytes() {
        let source_entry = MempoolSourceEntry {
            transaction_id: TransactionId::from_bytes([0xFF; 32]),
            auth_digest: Some(AuthDigest::from_bytes([0xAB; 32])),
            raw_transaction_bytes: RawTransactionBytes::new(vec![0u8; 4]),
            observed_at_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        };

        let outcome = build_mempool_entry(source_entry, synthetic_chain_epoch());

        assert!(matches!(
            outcome,
            Err(MempoolEntryBuildError::TransactionParseFailed { .. })
        ));
    }

    #[test]
    fn build_mempool_entry_rejects_short_payload() {
        let source_entry = MempoolSourceEntry {
            transaction_id: TransactionId::from_bytes([0xCD; 32]),
            auth_digest: None,
            raw_transaction_bytes: RawTransactionBytes::new(vec![0u8; 1]),
            observed_at_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        };

        let outcome = build_mempool_entry(source_entry, synthetic_chain_epoch());

        assert!(matches!(
            outcome,
            Err(MempoolEntryBuildError::TransactionParseFailed { .. })
        ));
    }
}
