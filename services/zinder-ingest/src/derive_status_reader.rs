//! Backend-neutral access to the ingest plane's latest derive status.

use prost::Message as _;
use thiserror::Error;
use zinder_derive::DeriveStore;
use zinder_proto::v1::wallet::{DeriveHealth, DeriveStatus};

/// Reads the latest typed derive-plane status for an operational surface.
///
/// Implementations own backend-specific decoding and validation. Control-plane
/// adapters depend only on this interface, so adding another derive storage
/// backend does not change their API.
pub trait DeriveStatusReader: Send + Sync {
    /// Returns the latest status, or `None` before the derive plane has
    /// persisted its first observation.
    fn read_derive_status(&self) -> Result<Option<DeriveStatus>, DeriveStatusReadError>;
}

/// Backend-neutral failure returned by a [`DeriveStatusReader`].
#[derive(Debug, Error)]
#[error("derive status read failed: {reason}")]
pub struct DeriveStatusReadError {
    reason: String,
}

impl DeriveStatusReadError {
    /// Creates a backend-neutral failure with an operator-facing reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Reads and validates derive status persisted in the embedded derive store.
#[derive(Clone)]
pub struct RocksDbDeriveStatusReader {
    store: DeriveStore,
}

impl RocksDbDeriveStatusReader {
    /// Creates a reader over the caller-owned derive-store handle.
    #[must_use]
    pub const fn new(store: DeriveStore) -> Self {
        Self { store }
    }
}

impl DeriveStatusReader for RocksDbDeriveStatusReader {
    fn read_derive_status(&self) -> Result<Option<DeriveStatus>, DeriveStatusReadError> {
        let Some(bytes) = self
            .store
            .get_derive_status()
            .map_err(|error| DeriveStatusReadError::new(format!("storage operation: {error}")))?
        else {
            return Ok(None);
        };
        let status = DeriveStatus::decode(bytes.as_slice()).map_err(|error| {
            DeriveStatusReadError::new(format!("persisted protobuf is malformed: {error}"))
        })?;
        validate_derive_status(&status)?;
        Ok(Some(status))
    }
}

fn validate_derive_status(status: &DeriveStatus) -> Result<(), DeriveStatusReadError> {
    let health = DeriveHealth::try_from(status.health).map_err(|_| {
        DeriveStatusReadError::new(format!(
            "persisted health value {} is unknown",
            status.health
        ))
    })?;
    if health == DeriveHealth::Unspecified {
        return Err(DeriveStatusReadError::new(
            "persisted health must identify a derive lifecycle state",
        ));
    }
    if health == DeriveHealth::Live && status.lag_blocks != 0 {
        return Err(DeriveStatusReadError::new(format!(
            "live status has nonzero canonical lag {}",
            status.lag_blocks
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_unknown_health() -> Result<(), DeriveStatusReadError> {
        let outcome = validate_derive_status(&DeriveStatus {
            health: 99,
            indexed_height: 10,
            lag_blocks: 2,
            observed_at_millis: 1,
        });
        let Err(error) = outcome else {
            return Err(DeriveStatusReadError::new(
                "unknown health was unexpectedly accepted",
            ));
        };

        assert!(error.to_string().contains("health value 99 is unknown"));
        Ok(())
    }

    #[test]
    fn validation_rejects_live_status_with_canonical_lag() -> Result<(), DeriveStatusReadError> {
        let outcome = validate_derive_status(&DeriveStatus {
            health: DeriveHealth::Live.into(),
            indexed_height: 10,
            lag_blocks: 1,
            observed_at_millis: 1,
        });
        let Err(error) = outcome else {
            return Err(DeriveStatusReadError::new(
                "live status with lag was unexpectedly accepted",
            ));
        };

        assert!(error.to_string().contains("nonzero canonical lag 1"));
        Ok(())
    }
}
