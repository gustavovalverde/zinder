//! `ExplorerQuery.ChainReorgHistory` handler.
//!
//! Reads the durable reorg-incidents materialized view. The view
//! backfills from the earliest retained chain-event row when the consumer first
//! appears, then preserves future incidents independently of chain-event
//! retention.

use prost::Message as _;
use tonic::{Request, Response, Status};
use zinder_materialized_views::{
    MaterializedViewStore, MaterializedViewStoreReadSnapshot, REORG_INCIDENTS_COLUMN_FAMILY,
    REORG_INCIDENTS_CONSUMER_NAME, REORG_INCIDENTS_KEY_LEN, ReorgIncidentsConsumer,
};
use zinder_proto::capabilities::EXPLORER_CHAIN_REORG_HISTORY_V1;
use zinder_proto::v1::explorer::{
    ChainReorgHistoryEvent, ChainReorgHistoryRequest, ChainReorgHistoryResponse,
};

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, read_materialized_view_status_snapshot,
};
use zinder_proto::v1::explorer::ExplorerFreshness;
use zinder_proto::v1::wallet::ChainView;
use zinder_store::CanonicalStoreConstructionIdentity;

/// Server-side maximum retained chain events scanned per request.
const MAX_CHAIN_REORG_HISTORY_EVENTS_PER_REQUEST: u32 = 1024;

/// Default retained chain-event scan size when the caller passes zero.
const DEFAULT_CHAIN_REORG_HISTORY_EVENTS: u32 = 64;

/// Version byte for a Reorg History cursor bound to one construction lineage.
const REORG_CURSOR_VERSION: u8 = 1;

/// Executes one `ExplorerQuery.ChainReorgHistory` request.
#[allow(
    clippy::significant_drop_tightening,
    clippy::too_many_lines,
    reason = "one event-only snapshot spans checkpoint validation, pagination, decoding, and the response freshness fence"
)]
pub(crate) async fn query_chain_reorg_history(
    materialized_view_store: &MaterializedViewStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<ChainReorgHistoryRequest>,
) -> Result<Response<ChainReorgHistoryResponse>, Status> {
    let inner = request.into_inner();
    let max_events = clamp_max_entries(
        inner.max_events,
        DEFAULT_CHAIN_REORG_HISTORY_EVENTS,
        MAX_CHAIN_REORG_HISTORY_EVENTS_PER_REQUEST,
    );
    materialized_view_store
        .try_catch_up()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let (events, next_cursor, freshness) = {
        let snapshot = materialized_view_store
            .read_snapshot()
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        require_reorg_incidents_checkpoint(&snapshot)?;
        read_reorg_history_snapshot(
            &snapshot,
            materialized_view_store.construction_identity(),
            &inner.from_cursor,
            max_events,
        )?
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;

    Ok(Response::new(ChainReorgHistoryResponse {
        freshness: Some(freshness),
        events,
        next_cursor,
    }))
}

/// Reads one bounded Reorg History page and its local freshness from one snapshot.
fn read_reorg_history_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    construction_identity: CanonicalStoreConstructionIdentity,
    from_cursor: &[u8],
    max_events: u32,
) -> Result<(Vec<ChainReorgHistoryEvent>, Vec<u8>, ExplorerFreshness), Status> {
    let from_event_sequence = decode_reorg_cursor(from_cursor, construction_identity)?;
    let rows = match from_event_sequence {
        Some(event_sequence) if event_sequence < u64::MAX => snapshot
            .range_iterate_consumer(
                REORG_INCIDENTS_COLUMN_FAMILY,
                &ReorgIncidentsConsumer::key_for_event_sequence(event_sequence + 1),
                &[0xFFu8; REORG_INCIDENTS_KEY_LEN],
                (max_events as usize).saturating_add(1),
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?,
        None => snapshot
            .range_iterate_consumer(
                REORG_INCIDENTS_COLUMN_FAMILY,
                &[0u8; REORG_INCIDENTS_KEY_LEN],
                &[0xFFu8; REORG_INCIDENTS_KEY_LEN],
                (max_events as usize).saturating_add(1),
            )
            .map_err(|error| ExplorerError::internal(error.to_string()))?,
        Some(_) => Vec::new(),
    };
    let has_more = rows.len() > max_events as usize;
    let visible_rows = rows
        .into_iter()
        .take(max_events as usize)
        .collect::<Vec<_>>();
    let mut events = Vec::with_capacity(visible_rows.len());
    let mut last_event_sequence = None;
    for (key, payload) in visible_rows {
        let key_array: [u8; REORG_INCIDENTS_KEY_LEN] = key
            .as_slice()
            .try_into()
            .map_err(|_| ExplorerError::internal("reorg_incidents row key is not 8 bytes"))?;
        let event = ChainReorgHistoryEvent::decode(payload.as_slice())
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        let event_sequence = ReorgIncidentsConsumer::decode_event_sequence_cursor(&key_array)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if event.event_sequence != event_sequence {
            return Err(ExplorerError::internal(
                "reorg_incidents row event sequence does not match its key",
            )
            .into());
        }
        last_event_sequence = Some(event_sequence);
        events.push(event);
    }
    let next_cursor = if has_more {
        last_event_sequence.map_or_else(Vec::new, |event_sequence| {
            encode_reorg_cursor(construction_identity, event_sequence)
        })
    } else {
        Vec::new()
    };
    let freshness = build_reorg_history_freshness(snapshot)?;
    Ok((events, next_cursor, freshness))
}

fn require_reorg_incidents_checkpoint(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<(), Status> {
    snapshot
        .chain_event_checkpoint(REORG_INCIDENTS_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized("Reorg Incidents chain-event checkpoint is unavailable")
        })?;
    Ok(())
}

fn build_reorg_history_freshness(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
) -> Result<ExplorerFreshness, Status> {
    let materialized_views = read_materialized_view_status_snapshot(snapshot)?;
    Ok(ExplorerFreshness {
        chain_view: materialized_views.map(|materialized_views| ChainView {
            chain_epoch: None,
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: Some(materialized_views),
        }),
        snapshot_age_millis: 0,
        capability_version: EXPLORER_CHAIN_REORG_HISTORY_V1.to_owned(),
        unavailable: Vec::new(),
    })
}

fn encode_reorg_cursor(
    construction_identity: CanonicalStoreConstructionIdentity,
    event_sequence: u64,
) -> Vec<u8> {
    let identity = construction_identity.encode_persisted();
    let mut cursor = Vec::with_capacity(1 + identity.len() + REORG_INCIDENTS_KEY_LEN);
    cursor.push(REORG_CURSOR_VERSION);
    cursor.extend_from_slice(&identity);
    cursor.extend_from_slice(&event_sequence.to_be_bytes());
    cursor
}

fn decode_reorg_cursor(
    encoded: &[u8],
    construction_identity: CanonicalStoreConstructionIdentity,
) -> Result<Option<u64>, Status> {
    if encoded.is_empty() {
        return Ok(None);
    }
    let identity = construction_identity.encode_persisted();
    let expected_length = 1 + identity.len() + REORG_INCIDENTS_KEY_LEN;
    if encoded.len() != expected_length || encoded.first() != Some(&REORG_CURSOR_VERSION) {
        return Err(ExplorerError::invalid_request("Reorg History cursor is malformed").into());
    }
    let identity_end = 1 + identity.len();
    if encoded[1..identity_end] != identity {
        return Err(ExplorerError::unsatisfied_precondition(
            "Reorg History cursor belongs to a different admitted chain lineage",
        )
        .into());
    }
    let event_sequence = u64::from_be_bytes(encoded[identity_end..].try_into().map_err(|_| {
        ExplorerError::invalid_request("Reorg History cursor event sequence malformed")
    })?);
    Ok(Some(event_sequence))
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockHeight, ChainEpochId, Network, NetworkUpgradeActivationsFingerprintVersion,
    };
    use zinder_materialized_views::{
        MaterializedViewState, MaterializedViewStoreOptions, REORG_INCIDENTS_SCHEMA,
    };
    use zinder_proto::v1::wallet::{MaterializedViewHealth, MaterializedViewStatus};
    use zinder_store::{
        CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION, CanonicalEventCursor, RocksDbResourceBudget,
    };

    fn construction_identity(seed: u8) -> Result<CanonicalStoreConstructionIdentity, &'static str> {
        let mut encoded = vec![0u8; 1 + 4 + 2 + 32 + 2 + 32];
        encoded[0] = 1;
        encoded[1..5].copy_from_slice(&Network::ZcashRegtest.id().to_be_bytes());
        encoded[5..7].copy_from_slice(
            &NetworkUpgradeActivationsFingerprintVersion::CURRENT
                .value()
                .to_be_bytes(),
        );
        encoded[7..39].fill(seed);
        encoded[39..41]
            .copy_from_slice(&CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION.to_be_bytes());
        CanonicalStoreConstructionIdentity::decode_persisted(&encoded)
            .map_err(|_| "test construction identity must decode")
    }

    #[test]
    fn reorg_cursor_round_trips_only_with_its_construction_identity() -> Result<(), &'static str> {
        let identity = construction_identity(1)?;
        let encoded = encode_reorg_cursor(identity, 64);

        let decoded = decode_reorg_cursor(&encoded, identity)
            .map_err(|_| "cursor must decode with its construction identity")?;
        assert_eq!(decoded, Some(64));

        let error = decode_reorg_cursor(&encoded, construction_identity(2)?)
            .err()
            .ok_or("other construction identity must fail")?;
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        Ok(())
    }

    #[test]
    #[allow(
        clippy::significant_drop_tightening,
        clippy::too_many_lines,
        reason = "the E1 event-only snapshot intentionally spans the complete E2 replacement and every asserted response axis"
    )]
    fn reorg_history_snapshot_retains_e1_rows_cursor_and_freshness_after_e2_write()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let activations = zinder_testkit::sample_regtest_upgrade_activations();
        let chain = zinder_testkit::ChainFixture::new(activations.network()).extend_blocks(2);
        let mut canonical_fixture =
            zinder_testkit::WalletServingStoreFixture::from_chain_after_live_append(
                &chain,
                &activations,
            )?;
        let identity = canonical_fixture.canonical_construction_identity()?;
        let (canonical_reader, _) = canonical_fixture.take_readers()?;
        let e1_checkpoint =
            zinder_materialized_views::MaterializedViewChainEventCheckpoint::from_retained_event(
                canonical_reader.retained_event_at_cursor(CanonicalEventCursor::at(1)?)?,
            );
        let e2_checkpoint =
            zinder_materialized_views::MaterializedViewChainEventCheckpoint::from_retained_event(
                canonical_reader.retained_event_at_cursor(CanonicalEventCursor::at(2)?)?,
            );
        let store = MaterializedViewStore::open(
            directory.path(),
            identity,
            MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: &[REORG_INCIDENTS_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        let e1_state = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(41),
            tip_height: BlockHeight::new(100),
            tip_hash: BlockHash::from_bytes([0x41; 32]),
            revision: 1,
            coverage: None,
        };
        let e1_status = MaterializedViewStatus {
            health: MaterializedViewHealth::Live as i32,
            indexed_height: 100,
            lag_blocks: 0,
            observed_at_millis: 1_000,
        };
        for event_sequence in [41_u64, 42] {
            let event = ChainReorgHistoryEvent {
                event_sequence,
                cursor: ReorgIncidentsConsumer::cursor_for_event_sequence(event_sequence).to_vec(),
                ..Default::default()
            };
            store.put_consumer(
                REORG_INCIDENTS_COLUMN_FAMILY,
                &ReorgIncidentsConsumer::key_for_event_sequence(event_sequence),
                &event.encode_to_vec(),
            )?;
        }
        store.put_consumer_state(REORG_INCIDENTS_CONSUMER_NAME, e1_state)?;
        store.put_chain_event_checkpoint(REORG_INCIDENTS_CONSUMER_NAME, e1_checkpoint)?;
        store.put_materialized_view_status(&e1_status.encode_to_vec())?;

        let e1_snapshot = store.read_snapshot()?;

        let e2_event = ChainReorgHistoryEvent {
            event_sequence: 43,
            cursor: ReorgIncidentsConsumer::cursor_for_event_sequence(43).to_vec(),
            ..Default::default()
        };
        store.put_consumer(
            REORG_INCIDENTS_COLUMN_FAMILY,
            &ReorgIncidentsConsumer::key_for_event_sequence(43),
            &e2_event.encode_to_vec(),
        )?;
        store.put_consumer_state(
            REORG_INCIDENTS_CONSUMER_NAME,
            MaterializedViewState {
                chain_epoch_id: ChainEpochId::new(42),
                tip_height: BlockHeight::new(101),
                tip_hash: BlockHash::from_bytes([0x42; 32]),
                revision: 2,
                coverage: None,
            },
        )?;
        store.put_chain_event_checkpoint(REORG_INCIDENTS_CONSUMER_NAME, e2_checkpoint)?;
        store.put_materialized_view_status(
            &MaterializedViewStatus {
                health: MaterializedViewHealth::Live as i32,
                indexed_height: 101,
                lag_blocks: 0,
                observed_at_millis: 1_001,
            }
            .encode_to_vec(),
        )?;

        let (events, cursor, freshness) =
            read_reorg_history_snapshot(&e1_snapshot, identity, &[], 1)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_sequence, 41);
        assert_eq!(decode_reorg_cursor(&cursor, identity)?, Some(41));
        assert_eq!(
            e1_snapshot.chain_event_checkpoint(REORG_INCIDENTS_CONSUMER_NAME)?,
            Some(e1_checkpoint)
        );
        assert_eq!(
            freshness
                .chain_view
                .and_then(|chain_view| chain_view.materialized_views)
                .map(|status| status.observed_at_millis),
            Some(e1_status.observed_at_millis)
        );
        Ok(())
    }
}
