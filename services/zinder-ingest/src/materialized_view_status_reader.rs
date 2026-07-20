//! Backend-neutral access to the materialized-view plane's latest status.

use prost::Message as _;
use thiserror::Error;
use zinder_materialized_views::MaterializedViewStore;
use zinder_proto::v1::wallet::{MaterializedViewHealth, MaterializedViewStatus};

/// Reads the latest typed materialized-view status for an operational surface.
///
/// Implementations own backend-specific decoding and validation. Control-plane
/// adapters depend only on this interface, so adding another materialized-view storage
/// backend does not change their API.
pub trait MaterializedViewStatusReader: Send + Sync {
    /// Returns the latest status, or `None` before the materialized-view plane has
    /// persisted its first observation.
    fn read_materialized_view_status(
        &self,
    ) -> Result<Option<MaterializedViewStatus>, MaterializedViewStatusReadError>;
}

/// Backend-neutral failure returned by a [`MaterializedViewStatusReader`].
#[derive(Debug, Error)]
#[error("materialized-view status read failed: {reason}")]
pub struct MaterializedViewStatusReadError {
    reason: String,
}

impl MaterializedViewStatusReadError {
    /// Creates a backend-neutral failure with an operator-facing reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Reads and validates materialized-view status persisted in the embedded materialized-view store.
#[derive(Clone)]
pub struct RocksDbMaterializedViewStatusReader {
    store: MaterializedViewStore,
}

impl RocksDbMaterializedViewStatusReader {
    /// Creates a reader over the caller-owned materialized-view handle.
    #[must_use]
    pub const fn new(store: MaterializedViewStore) -> Self {
        Self { store }
    }
}

impl MaterializedViewStatusReader for RocksDbMaterializedViewStatusReader {
    fn read_materialized_view_status(
        &self,
    ) -> Result<Option<MaterializedViewStatus>, MaterializedViewStatusReadError> {
        let Some(bytes) = self.store.get_materialized_view_status().map_err(|error| {
            MaterializedViewStatusReadError::new(format!("storage operation: {error}"))
        })?
        else {
            return Ok(None);
        };
        let status = MaterializedViewStatus::decode(bytes.as_slice()).map_err(|error| {
            MaterializedViewStatusReadError::new(format!(
                "persisted protobuf is malformed: {error}"
            ))
        })?;
        validate_materialized_view_status(&status)?;
        Ok(Some(status))
    }
}

fn validate_materialized_view_status(
    status: &MaterializedViewStatus,
) -> Result<(), MaterializedViewStatusReadError> {
    let health = MaterializedViewHealth::try_from(status.health).map_err(|_| {
        MaterializedViewStatusReadError::new(format!(
            "persisted health value {} is unknown",
            status.health
        ))
    })?;
    if health == MaterializedViewHealth::Unspecified {
        return Err(MaterializedViewStatusReadError::new(
            "persisted health must identify a materialized-view lifecycle state",
        ));
    }
    if health == MaterializedViewHealth::Live && status.lag_blocks != 0 {
        return Err(MaterializedViewStatusReadError::new(format!(
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
    fn validation_rejects_unknown_health() -> Result<(), MaterializedViewStatusReadError> {
        let outcome = validate_materialized_view_status(&MaterializedViewStatus {
            health: 99,
            indexed_height: 10,
            lag_blocks: 2,
            observed_at_millis: 1,
        });
        let Err(error) = outcome else {
            return Err(MaterializedViewStatusReadError::new(
                "unknown health was unexpectedly accepted",
            ));
        };

        assert!(error.to_string().contains("health value 99 is unknown"));
        Ok(())
    }

    #[test]
    fn validation_rejects_live_status_with_canonical_lag()
    -> Result<(), MaterializedViewStatusReadError> {
        let outcome = validate_materialized_view_status(&MaterializedViewStatus {
            health: MaterializedViewHealth::Live.into(),
            indexed_height: 10,
            lag_blocks: 1,
            observed_at_millis: 1,
        });
        let Err(error) = outcome else {
            return Err(MaterializedViewStatusReadError::new(
                "live status with lag was unexpectedly accepted",
            ));
        };

        assert!(error.to_string().contains("nonzero canonical lag 1"));
        Ok(())
    }
}
