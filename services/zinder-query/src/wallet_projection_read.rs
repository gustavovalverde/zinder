//! Typed query-side boundary for wallet-critical projections.

use std::{collections::HashMap, error::Error, fmt, sync::Arc};

use thiserror::Error;
use zinder_core::{
    BlockHeight, TransparentAddressTxIndexArtifact, TransparentOutPoint, TransparentSpendEntry,
};
use zinder_derive::{
    DeriveStore, DeriveStoreError, DeriveStoreReadSnapshot,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
    TransparentAddressTransactionHistoryConsumer, TransparentAddressTransactionHistoryPageRequest,
    TransparentOutpointSpendConsumer,
};
use zinder_store::StreamCursorTokenV1;

use crate::TransparentAddressTxIdsInRangeRequest;

/// One typed projection value with the durable height that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionRead<Value> {
    /// Authenticated canonical chain-event position reflected by this read.
    pub chain_event_cursor: Option<StreamCursorTokenV1>,
    /// Last block height materialized by this projection.
    pub materialized_height: Option<BlockHeight>,
    /// Typed value returned by the projection.
    pub value: Value,
}

impl<Value> ProjectionRead<Value> {
    /// Returns whether this read reflects the required canonical position.
    #[must_use]
    pub fn covers(
        &self,
        required_height: BlockHeight,
        required_cursor: Option<&StreamCursorTokenV1>,
    ) -> bool {
        projection_covers(
            self.materialized_height,
            self.chain_event_cursor.as_ref(),
            required_height,
            required_cursor,
        )
    }
}

/// Durable position reported by one wallet projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionPosition {
    /// Whether the selected workload includes the projection identity.
    pub available: bool,
    /// Authenticated canonical chain-event position committed with the projection.
    pub chain_event_cursor: Option<StreamCursorTokenV1>,
    /// Last block height materialized by the projection.
    pub materialized_height: Option<BlockHeight>,
}

impl WalletProjectionPosition {
    /// Returns whether this projection is complete through `required_height`.
    #[must_use]
    pub fn covers(
        &self,
        required_height: BlockHeight,
        required_cursor: Option<&StreamCursorTokenV1>,
    ) -> bool {
        self.available
            && projection_covers(
                self.materialized_height,
                self.chain_event_cursor.as_ref(),
                required_height,
                required_cursor,
            )
    }
}

/// Independent readiness positions for the two wallet-critical projections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletProjectionReadiness {
    /// Transparent-address transaction-history readiness.
    pub transparent_address_history: WalletProjectionPosition,
    /// Durable transparent outpoint-spend readiness.
    pub transparent_outpoint_spend: WalletProjectionPosition,
}

/// Transparent-address transaction-history page without a canonical epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransparentAddressHistoryPage {
    /// Transaction-history artifacts in requested order.
    pub artifacts: Vec<TransparentAddressTxIndexArtifact>,
    /// Resume cursor when more entries may be available.
    pub next_cursor: Option<StreamCursorTokenV1>,
}

/// Failure vocabulary for typed wallet-projection reads.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WalletProjectionReadError {
    /// Required projection is deliberately absent from this deployment.
    #[error("wallet projection {projection} is unavailable")]
    ProjectionUnavailable {
        /// Stable projection identity required by the read.
        projection: &'static str,
    },
    /// Transparent-address history cursor is invalid for the requested page.
    #[error("transparent-address history cursor is invalid")]
    TransparentAddressHistoryCursorInvalid,
    /// Projection backend could not complete a storage read.
    #[error("wallet projection storage read failed: {source}")]
    Storage {
        /// Backend-specific failure retained only as an error source.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

/// Typed reader for the two projections required by wallet deployments.
pub trait WalletProjectionReadApi: fmt::Debug + Send + Sync + 'static {
    /// Reads each wallet-critical projection's durable position.
    fn readiness(&self) -> Result<WalletProjectionReadiness, WalletProjectionReadError>;

    /// Reads transparent-address transaction history and its materialized head.
    fn transparent_address_history_page(
        &self,
        request: &TransparentAddressTxIdsInRangeRequest,
    ) -> Result<ProjectionRead<TransparentAddressHistoryPage>, WalletProjectionReadError>;

    /// Resolves durable transparent spenders and the projection's materialized head.
    fn transparent_outpoint_spenders(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<
        ProjectionRead<HashMap<TransparentOutPoint, TransparentSpendEntry>>,
        WalletProjectionReadError,
    >;
}

/// Current `RocksDB` adapter for the typed wallet-projection read boundary.
#[derive(Clone, Debug)]
pub(crate) struct DeriveStoreWalletProjectionReader {
    store: DeriveStore,
}

impl DeriveStoreWalletProjectionReader {
    pub(crate) const fn new(store: DeriveStore) -> Self {
        Self { store }
    }

    fn require_projection(
        &self,
        projection: zinder_derive::DeriveConsumerName,
    ) -> Result<(), WalletProjectionReadError> {
        if self.store.has_consumer(projection) {
            Ok(())
        } else {
            Err(WalletProjectionReadError::ProjectionUnavailable {
                projection: projection.as_str(),
            })
        }
    }

    fn refresh(&self) -> Result<(), WalletProjectionReadError> {
        self.store
            .try_catch_up()
            .map(|_outcome| ())
            .map_err(storage_error)
    }
}

impl WalletProjectionReadApi for DeriveStoreWalletProjectionReader {
    fn readiness(&self) -> Result<WalletProjectionReadiness, WalletProjectionReadError> {
        self.refresh()?;
        let snapshot = self.store.read_snapshot();
        Ok(WalletProjectionReadiness {
            transparent_address_history: self.projection_position(
                &snapshot,
                TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
                TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
            )?,
            transparent_outpoint_spend: self.projection_position(
                &snapshot,
                TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
                TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY,
            )?,
        })
    }

    fn transparent_address_history_page(
        &self,
        request: &TransparentAddressTxIdsInRangeRequest,
    ) -> Result<ProjectionRead<TransparentAddressHistoryPage>, WalletProjectionReadError> {
        self.require_projection(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME)?;
        self.refresh()?;
        let snapshot = self.store.read_snapshot();
        let chain_event_cursor = snapshot
            .get_chain_event_cursor(TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME)
            .map_err(storage_error)?
            .map(StreamCursorTokenV1::from_bytes);
        let materialized_height = snapshot
            .last_materialized_height_ascending(
                TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_INDEX_COLUMN_FAMILY,
            )
            .map_err(storage_error)?;
        let page = TransparentAddressTransactionHistoryConsumer::read_page_snapshot(
            &snapshot,
            TransparentAddressTransactionHistoryPageRequest {
                address_script_hash: request.address_script_hash,
                start_height: request.start_height,
                end_height: request.end_height,
                max_entries: request.max_entries,
                descending: request.descending,
                from_cursor: request.from_cursor.as_ref(),
            },
        )
        .map_err(history_error)?;
        drop(snapshot);
        Ok(ProjectionRead {
            chain_event_cursor,
            materialized_height,
            value: TransparentAddressHistoryPage {
                artifacts: page.artifacts,
                next_cursor: page.next_cursor,
            },
        })
    }

    fn transparent_outpoint_spenders(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Result<
        ProjectionRead<HashMap<TransparentOutPoint, TransparentSpendEntry>>,
        WalletProjectionReadError,
    > {
        self.require_projection(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)?;
        self.refresh()?;
        let snapshot = self.store.read_snapshot();
        let chain_event_cursor = snapshot
            .get_chain_event_cursor(TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME)
            .map_err(storage_error)?
            .map(StreamCursorTokenV1::from_bytes);
        let materialized_height = snapshot
            .last_materialized_height_ascending(TRANSPARENT_OUTPOINT_SPEND_INDEX_COLUMN_FAMILY)
            .map_err(storage_error)?;
        let spenders = TransparentOutpointSpendConsumer::read_spends_by_outpoints_snapshot(
            &snapshot, outpoints,
        )
        .map_err(storage_error)?;
        drop(snapshot);
        Ok(ProjectionRead {
            chain_event_cursor,
            materialized_height,
            value: spenders,
        })
    }
}

impl DeriveStoreWalletProjectionReader {
    fn projection_position(
        &self,
        snapshot: &DeriveStoreReadSnapshot<'_>,
        projection: zinder_derive::DeriveConsumerName,
        index_column_family: &'static str,
    ) -> Result<WalletProjectionPosition, WalletProjectionReadError> {
        if !self.store.has_consumer(projection) {
            return Ok(WalletProjectionPosition {
                available: false,
                chain_event_cursor: None,
                materialized_height: None,
            });
        }
        let chain_event_cursor = snapshot
            .get_chain_event_cursor(projection)
            .map_err(storage_error)?
            .map(StreamCursorTokenV1::from_bytes);
        let materialized_height = snapshot
            .last_materialized_height_ascending(index_column_family)
            .map_err(storage_error)?;
        Ok(WalletProjectionPosition {
            available: true,
            chain_event_cursor,
            materialized_height,
        })
    }
}

fn projection_covers(
    materialized_height: Option<BlockHeight>,
    projection_cursor: Option<&StreamCursorTokenV1>,
    required_height: BlockHeight,
    required_cursor: Option<&StreamCursorTokenV1>,
) -> bool {
    materialized_height.is_some_and(|height| height >= required_height)
        && required_cursor.is_some_and(|cursor| projection_cursor == Some(cursor))
}

/// Builds the current `RocksDB` adapter for wallet projection reads and readiness.
#[must_use]
pub fn derive_store_wallet_projection_reader(
    store: DeriveStore,
) -> Arc<dyn WalletProjectionReadApi> {
    Arc::new(DeriveStoreWalletProjectionReader::new(store))
}

fn history_error(error: DeriveStoreError) -> WalletProjectionReadError {
    if matches!(error, DeriveStoreError::ProjectionCursorInvalid { .. }) {
        WalletProjectionReadError::TransparentAddressHistoryCursorInvalid
    } else {
        storage_error(error)
    }
}

fn storage_error(error: DeriveStoreError) -> WalletProjectionReadError {
    WalletProjectionReadError::Storage {
        source: Box::new(error),
    }
}
