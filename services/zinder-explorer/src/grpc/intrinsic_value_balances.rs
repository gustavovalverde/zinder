//! Canonical transaction-intrinsic value-balance lookup shared by explorer reads.

use std::collections::HashMap;

use tonic::Status;
use zinder_core::{Network, NetworkUpgradeActivations, TransactionId, TransactionLocation};
use zinder_proto::v1::explorer::TransactionIntrinsicValueBalances;
use zinder_store::{ChainEpochReader, status_from_store_error};

use super::error::ExplorerError;

/// Resolves signed shielded balances from canonical artifacts or retained blobs.
///
/// Every resolved source must match the supplied canonical transaction location.
/// Missing artifacts and blobs remain absent so callers never mistake unavailable
/// historical data for an all-zero value balance.
pub(crate) fn resolve_transaction_intrinsic_value_balances(
    reader: &ChainEpochReader<'_>,
    network: Network,
    locations: &[(TransactionId, TransactionLocation)],
) -> Result<HashMap<TransactionId, TransactionIntrinsicValueBalances>, Status> {
    if locations.is_empty() {
        return Ok(HashMap::new());
    }

    let transaction_ids = locations
        .iter()
        .map(|(transaction_id, _)| *transaction_id)
        .collect::<Vec<_>>();
    let artifacts = reader
        .transaction_intrinsic_value_balances_by_ids(&transaction_ids)
        .map_err(|error| status_from_store_error(&error))?;
    let activations = NetworkUpgradeActivations::empty(network);
    let mut balances = HashMap::new();

    for (transaction_id, expected_location) in locations {
        let value_balances =
            if let Some(artifact) = artifacts.get(transaction_id).copied().flatten() {
                validate_intrinsic_value_balance_location(
                    *transaction_id,
                    *expected_location,
                    artifact.location,
                )?;
                artifact.value_balances
            } else {
                let Some(blob) = reader
                    .transaction_blob_by_id(*transaction_id)
                    .map_err(|error| status_from_store_error(&error))?
                else {
                    continue;
                };
                validate_intrinsic_value_balance_location(
                    *transaction_id,
                    *expected_location,
                    blob.location,
                )?;
                let fact_set = zinder_source::parse_transaction_public_fact_set(
                    &blob.raw_transaction_bytes,
                    Some(blob.location.block_height),
                    &activations,
                )
                .map_err(|error| ExplorerError::internal(error.to_string()))?;
                if fact_set.public_facts.transaction_id != *transaction_id {
                    return Err(ExplorerError::internal(
                    "retained transaction blob id does not match canonical transaction location",
                )
                .into());
                }
                fact_set.intrinsic_value_balances
            };
        balances.insert(
            *transaction_id,
            TransactionIntrinsicValueBalances {
                sprout_zat: value_balances.sprout_zat,
                sapling_zat: value_balances.sapling_zat,
                orchard_zat: value_balances.orchard_zat,
                ironwood_zat: value_balances.ironwood_zat,
            },
        );
    }

    Ok(balances)
}

fn validate_intrinsic_value_balance_location(
    transaction_id: TransactionId,
    expected_location: TransactionLocation,
    actual_location: TransactionLocation,
) -> Result<(), Status> {
    if actual_location != expected_location || actual_location.transaction_id != transaction_id {
        return Err(ExplorerError::internal(
            "transaction intrinsic value-balance location does not match canonical transaction location",
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{BlockHash, BlockHeight};

    #[test]
    fn intrinsic_value_balance_location_must_match_the_canonical_location() -> Result<(), Status> {
        let transaction_id = TransactionId::from_bytes([0x11; 32]);
        let expected = TransactionLocation::new(
            transaction_id,
            BlockHeight::new(42),
            BlockHash::from_bytes([0x22; 32]),
            7,
        );
        validate_intrinsic_value_balance_location(transaction_id, expected, expected)?;

        let actual = TransactionLocation::new(
            transaction_id,
            BlockHeight::new(42),
            BlockHash::from_bytes([0x22; 32]),
            8,
        );
        let error = validate_intrinsic_value_balance_location(transaction_id, expected, actual)
            .err()
            .ok_or_else(|| Status::internal("mismatched transaction index was accepted"))?;
        assert_eq!(error.code(), tonic::Code::Internal);
        assert_eq!(
            error.message(),
            "transaction intrinsic value-balance location does not match canonical transaction location"
        );
        Ok(())
    }
}
