//! Node-sourced transaction values.
//!
//! Helpers that translate raw serialized Zcash transaction bytes (as carried
//! in `TransactionArtifact.payload_bytes` or live mempool entries) into
//! Zinder vocabulary. Mirrors the [`crate::source_block`] pattern: the
//! upstream Zebra type is parsed once at the boundary and the typed
//! Zinder shape is the public return.

use zebra_chain::{
    serialization::ZcashDeserializeInto, transaction::Transaction as ZebraTransaction,
};
use zinder_core::TransparentPrevout;

use crate::SourceError;

/// Resolves one transparent output of a serialized Zcash transaction by its
/// `output_index`.
///
/// Returns `Ok(Some(_))` when the transaction parses cleanly and contains an
/// output at the requested index. Returns `Ok(None)` when the index is out of
/// bounds for the transaction's vout list (the transaction itself parsed,
/// just the index is wrong). Returns `Err(_)` when the bytes do not parse.
///
/// Shielded-only transactions return `Ok(None)` for every index because they
/// have no transparent outputs.
pub fn transparent_prevout_from_raw_transaction_bytes(
    raw_transaction_bytes: &[u8],
    output_index: u32,
) -> Result<Option<TransparentPrevout>, SourceError> {
    let transaction: ZebraTransaction =
        raw_transaction_bytes
            .zcash_deserialize_into()
            .map_err(|source| SourceError::RawTransactionParseFailed {
                reason: source.to_string(),
            })?;
    let outputs = transaction.outputs();
    let Some(output) = usize::try_from(output_index)
        .ok()
        .and_then(|index| outputs.get(index))
    else {
        return Ok(None);
    };
    Ok(Some(TransparentPrevout {
        value_zat: u64::from(output.value()),
        script_pub_key: output.lock_script.as_raw_bytes().to_vec(),
    }))
}
