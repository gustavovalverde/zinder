//! Shared mined transparent-input projection for explorer responses.

use std::collections::{HashMap, HashSet};

use zinder_core::wire::encode_rpc_transaction_id_hex;
use zinder_core::{TransactionFactsArtifact, TransactionId, TransparentInputFact};
use zinder_proto::v1::explorer::{TransactionFeesRecord, TransparentInput};
use zinder_proto::v1::wallet;

/// Collects unique retained parent transaction ids in first-seen order.
pub(super) fn parent_transaction_ids<'transaction>(
    transactions: impl IntoIterator<Item = &'transaction TransactionFactsArtifact>,
) -> Vec<TransactionId> {
    let mut seen = HashSet::new();
    transactions
        .into_iter()
        .flat_map(|transaction| &transaction.transparent_inputs)
        .filter(|input| !input.spent_outpoint.is_coinbase_sentinel())
        .filter_map(|input| {
            seen.insert(input.spent_outpoint.transaction_id)
                .then_some(input.spent_outpoint.transaction_id)
        })
        .collect()
}

/// Encodes ordered mined inputs with independently optional parent value/script.
pub(super) fn encode_mined_transparent_inputs(
    transaction: &TransactionFactsArtifact,
    parent_transactions: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    fees: Option<&TransactionFeesRecord>,
) -> Vec<TransparentInput> {
    if transaction.public_facts.is_coinbase {
        return Vec::new();
    }
    let projected_values: HashMap<u32, u64> = fees
        .into_iter()
        .flat_map(|record| record.transparent_inputs.iter())
        .filter_map(|input| {
            input
                .value_zat
                .map(|value_zat| (input.input_index, value_zat))
        })
        .collect();
    transaction
        .transparent_inputs
        .iter()
        .filter(|input| !input.spent_outpoint.is_coinbase_sentinel())
        .map(|input| {
            let prevout = parent_transactions
                .get(&input.spent_outpoint.transaction_id)
                .and_then(Option::as_ref)
                .and_then(|parent| {
                    parent
                        .transparent_outputs
                        .iter()
                        .find(|output| output.output_index == input.spent_outpoint.output_index)
                });
            TransparentInput {
                input_index: input.input_index,
                spent_outpoint: Some(wallet::OutPoint {
                    transaction_id: encode_rpc_transaction_id_hex(
                        input.spent_outpoint.transaction_id,
                    ),
                    output_index: input.spent_outpoint.output_index,
                }),
                value_zat: prevout
                    .map(|output| output.value_zat)
                    .or_else(|| projected_values.get(&input.input_index).copied()),
                script_pub_key: prevout.map(|output| output.script_pub_key.clone()),
            }
        })
        .collect()
}

/// Encodes transient inputs when only their transaction-local outpoints exist.
pub(super) fn encode_unresolved_transparent_inputs(
    inputs: &[TransparentInputFact],
    fees: Option<&TransactionFeesRecord>,
) -> Vec<TransparentInput> {
    let projected_values: HashMap<u32, u64> = fees
        .into_iter()
        .flat_map(|record| record.transparent_inputs.iter())
        .filter_map(|input| {
            input
                .value_zat
                .map(|value_zat| (input.input_index, value_zat))
        })
        .collect();
    inputs
        .iter()
        .filter(|input| !input.spent_outpoint.is_coinbase_sentinel())
        .map(|input| TransparentInput {
            input_index: input.input_index,
            spent_outpoint: Some(wallet::OutPoint {
                transaction_id: encode_rpc_transaction_id_hex(input.spent_outpoint.transaction_id),
                output_index: input.spent_outpoint.output_index,
            }),
            value_zat: projected_values.get(&input.input_index).copied(),
            script_pub_key: None,
        })
        .collect()
}
