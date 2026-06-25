#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    MempoolEntry, MempoolEvictionReason, Network, RawTransactionBytes, TransactionId,
    TransparentAddressScriptHash, TransparentMempoolOutput, TransparentMempoolSpend,
    TransparentOutPoint, UnixTimestampMillis,
};
use zinder_store::{
    ChainStoreOptions, MempoolEvent, MempoolEventHistoryRequest, MempoolEventRetentionConfig,
    PrimaryChainStore, StoreError, StreamCursorTokenV1,
};

/// Round-trip: appending three mempool events and reading them back without
/// a cursor returns them in append order.
#[test]
fn mempool_event_history_round_trip() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = open_store(tempdir.path())?;

    let chain_epoch = synthetic_chain_epoch(1);
    let added = synthetic_entry(0xA0, chain_epoch);
    let added_envelope = store.append_mempool_event(
        MempoolEvent::Added {
            entry: added.clone(),
        },
        UnixTimestampMillis::new(1_000),
    )?;
    let mined_envelope = store.append_mempool_event(
        MempoolEvent::Mined {
            transaction_id: added.transaction_id,
            mined_height: BlockHeight::new(7),
            block_hash: BlockHash::from_bytes([0xC7; 32]),
        },
        UnixTimestampMillis::new(2_000),
    )?;
    let invalidated_envelope = store.append_mempool_event(
        MempoolEvent::Invalidated {
            transaction_id: TransactionId::from_bytes([0xB0; 32]),
            reason: MempoolEvictionReason::Conflict,
        },
        UnixTimestampMillis::new(3_000),
    )?;

    let history =
        store.mempool_event_history(MempoolEventHistoryRequest::with_default_limit(None))?;
    let sequences: Vec<u64> = history
        .iter()
        .map(|envelope| envelope.event_sequence)
        .collect();
    assert_eq!(
        sequences,
        vec![
            added_envelope.event_sequence,
            mined_envelope.event_sequence,
            invalidated_envelope.event_sequence,
        ],
    );
    Ok(())
}

/// Resume from cursor: the second `mempool_event_history` request strictly
/// after the cursor returns only the events past it.
#[test]
fn mempool_event_history_resumes_strictly_after_cursor() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = open_store(tempdir.path())?;
    let chain_epoch = synthetic_chain_epoch(1);

    let first = store.append_mempool_event(
        MempoolEvent::Added {
            entry: synthetic_entry(0xA0, chain_epoch),
        },
        UnixTimestampMillis::new(1_000),
    )?;
    let _second = store.append_mempool_event(
        MempoolEvent::Added {
            entry: synthetic_entry(0xA1, chain_epoch),
        },
        UnixTimestampMillis::new(2_000),
    )?;

    let resumed = store.mempool_event_history(MempoolEventHistoryRequest::with_default_limit(
        Some(&first.cursor),
    ))?;
    let sequences: Vec<u64> = resumed
        .iter()
        .map(|envelope| envelope.event_sequence)
        .collect();
    assert_eq!(sequences, vec![first.event_sequence + 1]);
    Ok(())
}

/// A mempool resume cursor that fails authentication is rejected, the same
/// integrity discipline the chain-event resume applies.
#[test]
fn tampered_mempool_event_cursor_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = open_store(tempdir.path())?;
    let chain_epoch = synthetic_chain_epoch(1);

    let appended = store.append_mempool_event(
        MempoolEvent::Added {
            entry: synthetic_entry(0xA0, chain_epoch),
        },
        UnixTimestampMillis::new(1_000),
    )?;

    let mut cursor_bytes = appended.cursor.as_bytes().to_vec();
    *cursor_bytes
        .last_mut()
        .ok_or_else(|| eyre!("expected non-empty cursor bytes"))? ^= 1;
    let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

    let error = match store.mempool_event_history(MempoolEventHistoryRequest::with_default_limit(
        Some(&cursor),
    )) {
        Ok(history) => return Err(eyre!("expected invalid cursor, got {history:?}")),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StoreError::MempoolEventCursorInvalid { .. }
    ));
    Ok(())
}

/// Atomicity: idempotent floor advancement on resumed prune.
///
/// When a prune pass observes that the floor must advance but no physical
/// deletes remain (a previous prune crashed mid-batch and the column-family
/// delete has already happened), the floor still advances so readers do not
/// observe a partially pruned tail.
#[test]
fn prune_advances_floor_when_deletes_already_applied() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = open_store(tempdir.path())?;
    let chain_epoch = synthetic_chain_epoch(1);

    let stale_one = store.append_mempool_event(
        MempoolEvent::Mined {
            transaction_id: TransactionId::from_bytes([0xA0; 32]),
            mined_height: BlockHeight::new(1),
            block_hash: BlockHash::from_bytes([0xB0; 32]),
        },
        UnixTimestampMillis::new(0),
    )?;
    let _stale_two = store.append_mempool_event(
        MempoolEvent::Mined {
            transaction_id: TransactionId::from_bytes([0xA1; 32]),
            mined_height: BlockHeight::new(1),
            block_hash: BlockHash::from_bytes([0xB1; 32]),
        },
        UnixTimestampMillis::new(0),
    )?;
    let _recent = store.append_mempool_event(
        MempoolEvent::Added {
            entry: synthetic_entry(0xA2, chain_epoch),
        },
        UnixTimestampMillis::new(1_000_000_000),
    )?;

    // First prune: physically deletes the two stale Mined envelopes and
    // advances the floor.
    let now = UnixTimestampMillis::new(1_000_000_000);
    let retention = MempoolEventRetentionConfig::new(Some(Duration::from_secs(1)), None);
    let first_report = store.prune_mempool_events_before(now, retention)?;
    assert!(first_report.pruned_mined_count >= 2);
    let floor_after_first_pass = first_report
        .oldest_retained_sequence
        .ok_or_else(|| eyre!("expected oldest retained after first prune"))?;
    assert!(floor_after_first_pass > stale_one.event_sequence);

    // Second prune over the same window: nothing left to delete, floor must
    // not regress (and if anything advances on a no-op call, the call is a
    // no-op).
    let second_report = store.prune_mempool_events_before(now, retention)?;
    assert_eq!(second_report.pruned_mined_count, 0);
    assert_eq!(
        second_report.oldest_retained_sequence,
        Some(floor_after_first_pass),
    );
    Ok(())
}

/// `prune_when_only_current_event_exists`: with a single retained event, a
/// retention pass with an aggressive window does not corrupt the floor and
/// leaves the most recent event untouched.
#[test]
fn prune_when_only_current_event_exists() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = open_store(tempdir.path())?;
    let chain_epoch = synthetic_chain_epoch(1);

    let only_envelope = store.append_mempool_event(
        MempoolEvent::Added {
            entry: synthetic_entry(0xA0, chain_epoch),
        },
        UnixTimestampMillis::new(1_000_000_000),
    )?;

    let retention = MempoolEventRetentionConfig::new(Some(Duration::from_secs(1)), None);
    let report =
        store.prune_mempool_events_before(UnixTimestampMillis::new(1_000_000_000), retention)?;
    assert_eq!(report.pruned_added_count, 0);
    assert_eq!(report.pruned_mined_count, 0);
    assert_eq!(report.pruned_invalidated_count, 0);
    assert_eq!(
        report.oldest_retained_sequence,
        Some(only_envelope.event_sequence),
    );
    assert_eq!(report.retained_event_count, 1);

    // The current event is still readable.
    let history =
        store.mempool_event_history(MempoolEventHistoryRequest::with_default_limit(None))?;
    assert_eq!(history.len(), 1);
    Ok(())
}

/// Cursor expiration: a cursor pointing at a pruned mempool event surfaces
/// `MempoolEventCursorExpired` carrying the new floor.
#[test]
fn cursor_below_pruned_floor_returns_mempool_cursor_expired() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = open_store(tempdir.path())?;
    let chain_epoch = synthetic_chain_epoch(1);

    let stale = store.append_mempool_event(
        MempoolEvent::Mined {
            transaction_id: TransactionId::from_bytes([0xA0; 32]),
            mined_height: BlockHeight::new(1),
            block_hash: BlockHash::from_bytes([0xC2; 32]),
        },
        UnixTimestampMillis::new(0),
    )?;
    let _recent = store.append_mempool_event(
        MempoolEvent::Added {
            entry: synthetic_entry(0xA1, chain_epoch),
        },
        UnixTimestampMillis::new(1_000_000_000),
    )?;

    let now = UnixTimestampMillis::new(1_000_000_000);
    let retention = MempoolEventRetentionConfig::new(Some(Duration::from_secs(1)), None);
    let report = store.prune_mempool_events_before(now, retention)?;
    assert!(report.pruned_mined_count >= 1);

    let outcome = store.mempool_event_history(MempoolEventHistoryRequest::with_default_limit(
        Some(&stale.cursor),
    ));
    let error = outcome
        .err()
        .ok_or_else(|| eyre!("expected MempoolEventCursorExpired"))?;
    let StoreError::MempoolEventCursorExpired {
        oldest_retained_sequence,
        ..
    } = &error
    else {
        return Err(eyre!("unexpected error variant: {error:?}"));
    };
    assert!(*oldest_retained_sequence > stale.event_sequence);
    Ok(())
}

/// Crash-resume durability of the prune floor.
///
/// After a `prune_mempool_events_before` call, the store is closed and
/// reopened. The `oldest_retained_mempool_event_sequence` floor is preserved
/// durably (not just held in process memory).
#[test]
fn pruned_floor_persists_across_store_reopen() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let chain_epoch = synthetic_chain_epoch(1);

    let pruned_floor = {
        let store = open_store(tempdir.path())?;
        let _stale = store.append_mempool_event(
            MempoolEvent::Mined {
                transaction_id: TransactionId::from_bytes([0xA0; 32]),
                mined_height: BlockHeight::new(1),
                block_hash: BlockHash::from_bytes([0xC3; 32]),
            },
            UnixTimestampMillis::new(0),
        )?;
        let _recent = store.append_mempool_event(
            MempoolEvent::Added {
                entry: synthetic_entry(0xA1, chain_epoch),
            },
            UnixTimestampMillis::new(1_000_000_000),
        )?;
        let report = store.prune_mempool_events_before(
            UnixTimestampMillis::new(1_000_000_000),
            MempoolEventRetentionConfig::new(Some(Duration::from_secs(1)), None),
        )?;
        report
            .oldest_retained_sequence
            .ok_or_else(|| eyre!("expected non-empty floor after prune"))?
    };

    // Reopen the store and confirm the retention report still observes the
    // pruned floor.
    let reopened = open_store(tempdir.path())?;
    let report = reopened.mempool_event_retention_report()?;
    assert_eq!(report.oldest_retained_sequence, Some(pruned_floor));
    Ok(())
}

fn open_store(path: &std::path::Path) -> Result<PrimaryChainStore, StoreError> {
    PrimaryChainStore::open(path, ChainStoreOptions::for_network(Network::ZcashRegtest))
}

fn synthetic_chain_epoch(chain_epoch_id: u64) -> ChainEpoch {
    ChainEpoch {
        id: ChainEpochId::new(chain_epoch_id),
        network: Network::ZcashRegtest,
        visible_tip_height: BlockHeight::new(1),
        visible_tip_hash: BlockHash::from_bytes([0x42; 32]),
        settled_tip_height: BlockHeight::new(1),
        settled_tip_hash: BlockHash::from_bytes([0x42; 32]),
        artifact_schema_version: ArtifactSchemaVersion::new(11),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_700_000_000_000),
    }
}

fn synthetic_entry(transaction_id_byte: u8, chain_epoch: ChainEpoch) -> MempoolEntry {
    let transaction_id = TransactionId::from_bytes([transaction_id_byte; 32]);
    MempoolEntry {
        transaction_id,
        auth_digest: None,
        raw_transaction_bytes: RawTransactionBytes::new(vec![transaction_id_byte; 8]),
        compact_transaction_bytes: vec![transaction_id_byte; 4],
        first_seen_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
        first_seen_chain_epoch: chain_epoch,
        transparent_outputs: vec![TransparentMempoolOutput {
            address_script_hash: TransparentAddressScriptHash::from_bytes(
                [transaction_id_byte; 32],
            ),
            script_pub_key: vec![transaction_id_byte; 25],
            outpoint: TransparentOutPoint::new(transaction_id, 0),
            value_zat: 1_000,
        }],
        transparent_spends: vec![TransparentMempoolSpend {
            spent_outpoint: TransparentOutPoint::new(
                TransactionId::from_bytes([transaction_id_byte.wrapping_add(1); 32]),
                0,
            ),
            spending_transaction_id: transaction_id,
        }],
    }
}
