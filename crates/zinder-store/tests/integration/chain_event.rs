#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
    CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_store::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest,
    ChainEventStreamFamily, ChainStoreOptions, EventStreamStartPosition, PrimaryChainStore,
    ReorgWindowChange, StoreError, StreamCursorTokenV1,
};

#[test]
fn chain_event_history_resumes_after_persisted_cursor() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);

    let first_commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;
    let second_commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        second_epoch,
        vec![second_block],
        vec![second_compact_block],
    ))?;

    let event_history = store.chain_event_history(ChainEventHistoryRequest::new(
        None,
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(
        event_history,
        vec![
            first_commit.event_envelope.clone(),
            second_commit.event_envelope.clone()
        ]
    );

    let resumed_history = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&first_commit.event_envelope.cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(resumed_history, vec![second_commit.event_envelope]);

    Ok(())
}

#[test]
fn checkpoint_bootstrap_cursor_resumes_after_artifactless_anchor() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let checkpoint_height = BlockHeight::new(10);
    let checkpoint_hash = block_hash(checkpoint_height.value());
    let checkpoint_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        visible_tip_height: checkpoint_height,
        visible_tip_hash: checkpoint_hash,
        settled_tip_height: checkpoint_height,
        settled_tip_hash: checkpoint_hash,
        artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_200_010),
    };
    let checkpoint_commit = store.commit_artifactless_checkpoint(checkpoint_epoch)?;
    let (next_epoch, next_block, next_compact_block) = synthetic_epoch(2, 11);
    let next_commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        next_epoch,
        vec![next_block],
        vec![next_compact_block],
    ))?;

    let resumed_history = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&checkpoint_commit.event_envelope.cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;

    assert_eq!(resumed_history, vec![next_commit.event_envelope]);

    Ok(())
}

#[test]
fn chain_event_history_returns_bounded_pages() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let mut committed_envelopes = Vec::new();

    for height in 1..=3 {
        let (chain_epoch, block, compact_block) = synthetic_epoch(u64::from(height), height);
        let commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
        committed_envelopes.push(commit.event_envelope);
    }

    let first_page = store.chain_event_history(ChainEventHistoryRequest::new(
        None,
        NonZeroU32::new(2).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(first_page, committed_envelopes[..2]);

    let second_page = store.chain_event_history(ChainEventHistoryRequest::new(
        first_page
            .last()
            .map(|event_envelope| &event_envelope.cursor),
        NonZeroU32::new(2).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(second_page, committed_envelopes[2..]);

    Ok(())
}

#[test]
fn chain_event_retention_prunes_prefix_and_expires_stale_cursors() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let mut committed_envelopes = Vec::new();

    for height in 1..=3 {
        let (chain_epoch, block, compact_block) = synthetic_epoch(u64::from(height), height);
        let commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
        committed_envelopes.push(commit.event_envelope);
    }

    let report = store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_200_003))?;
    assert_eq!(report.current_event_sequence, 3);
    assert_eq!(report.oldest_retained_sequence, Some(3));
    assert_eq!(report.retained_event_count, 1);
    assert_eq!(report.pruned_event_count, 2);

    let event_history = store.chain_event_history(ChainEventHistoryRequest::new(
        None,
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(event_history, vec![committed_envelopes[2].clone()]);

    let error = match store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&committed_envelopes[0].cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    )) {
        Ok(event_history) => {
            return Err(eyre!("expected expired cursor, got {event_history:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        StoreError::ChainEventCursorExpired {
            event_sequence: 1,
            oldest_retained_sequence: 3,
        }
    ));

    Ok(())
}

#[test]
fn chain_event_retention_never_prunes_the_newest_event() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let mut committed_envelopes = Vec::new();

    for height in 1..=2 {
        let (chain_epoch, block, compact_block) = synthetic_epoch(u64::from(height), height);
        let commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
        committed_envelopes.push(commit.event_envelope);
    }

    let report = store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_201_000))?;
    assert_eq!(report.oldest_retained_sequence, Some(2));
    assert_eq!(report.retained_event_count, 1);

    let event_history = store.chain_event_history(ChainEventHistoryRequest::new(
        None,
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(event_history, vec![committed_envelopes[1].clone()]);

    Ok(())
}

#[test]
fn chain_event_history_encodes_requested_stream_family() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let event_history = store.chain_event_history(ChainEventHistoryRequest::new_for_family(
        None,
        ChainEventStreamFamily::Safe,
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    let event_envelope = event_history
        .first()
        .ok_or_else(|| eyre!("expected safe-tip event"))?;

    assert_eq!(event_envelope.cursor.as_bytes()[49], 0x1);

    Ok(())
}

#[test]
fn chain_event_history_survives_store_reopen() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let committed_envelope = {
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        let commit_outcome = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
        commit_outcome.event_envelope
    };

    let reopened = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    assert_eq!(
        reopened.chain_event_history(ChainEventHistoryRequest::new(
            None,
            NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
        ))?,
        vec![committed_envelope]
    );

    Ok(())
}

#[test]
fn tampered_chain_event_cursor_is_rejected() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let commit_outcome = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    let mut cursor_bytes = commit_outcome.event_envelope.cursor.as_bytes().to_vec();
    let last_byte = cursor_bytes
        .last_mut()
        .ok_or_else(|| eyre!("expected non-empty cursor bytes"))?;
    *last_byte ^= 1;
    let cursor = StreamCursorTokenV1::from_bytes(cursor_bytes);

    let error = match store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    )) {
        Ok(event_history) => {
            return Err(eyre!("expected invalid cursor, got {event_history:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::ChainEventCursorInvalid { .. }));

    Ok(())
}

#[test]
fn cursor_from_another_store_is_rejected() -> eyre::Result<()> {
    let first_tempdir = tempdir()?;
    let first_store =
        PrimaryChainStore::open(first_tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    let first_commit = first_store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let second_tempdir = tempdir()?;
    let second_store =
        PrimaryChainStore::open(second_tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (second_epoch, second_block, second_compact_block) = synthetic_epoch(1, 1);
    second_store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        second_epoch,
        vec![second_block],
        vec![second_compact_block],
    ))?;

    let error = match second_store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&first_commit.event_envelope.cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    )) {
        Ok(event_history) => {
            return Err(eyre!("expected invalid cursor, got {event_history:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::ChainEventCursorInvalid { .. }));

    Ok(())
}

#[test]
fn cursor_before_any_commits_is_rejected() -> eyre::Result<()> {
    let cursor = {
        let tempdir = tempdir()?;
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        store
            .commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
                chain_epoch,
                vec![block],
                vec![compact_block],
            ))?
            .event_envelope
            .cursor
    };

    let empty_tempdir = tempdir()?;
    let empty_store =
        PrimaryChainStore::open(empty_tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let error = match empty_store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    )) {
        Ok(event_history) => {
            return Err(eyre!("expected invalid cursor, got {event_history:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::ChainEventCursorInvalid { .. }));

    Ok(())
}

#[test]
fn commit_outcome_includes_cursor_bound_chain_event() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);

    let commit_outcome = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    assert_committed_event(&commit_outcome.event_envelope, chain_epoch);

    Ok(())
}

#[test]
fn test_derived_consumer_resumes_and_replays_reorgs() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (_, initial_block_1, initial_compact_block_1) = synthetic_epoch(1, 1);
    let (initial_epoch, initial_block, initial_compact_block) =
        synthetic_epoch_with_safe_tip(1, 2, 1, block_hash(2));
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        initial_epoch,
        vec![initial_block_1, initial_block],
        vec![initial_compact_block_1, initial_compact_block],
    ))?;

    let mut consumer = TestDerivedConsumer::default();
    consumer.apply_available_events(&store)?;
    assert_eq!(
        consumer.rows,
        vec![
            DerivedBlockRow {
                height: BlockHeight::new(1),
                chain_epoch: initial_epoch.id,
                block_hash: initial_epoch.visible_tip_hash,
            },
            DerivedBlockRow {
                height: BlockHeight::new(2),
                chain_epoch: initial_epoch.id,
                block_hash: initial_epoch.visible_tip_hash,
            }
        ]
    );

    let replacement_hash = block_hash(20);
    let (replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch_with_safe_tip(2, 2, 1, replacement_hash);
    store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;

    let mut restarted_consumer = consumer.clone();
    restarted_consumer.apply_available_events(&store)?;
    assert_eq!(
        restarted_consumer.rows,
        vec![
            DerivedBlockRow {
                height: BlockHeight::new(1),
                chain_epoch: initial_epoch.id,
                block_hash: initial_epoch.visible_tip_hash,
            },
            DerivedBlockRow {
                height: BlockHeight::new(2),
                chain_epoch: replacement_epoch.id,
                block_hash: replacement_hash,
            }
        ]
    );

    let rows_after_replay = restarted_consumer.rows.clone();
    restarted_consumer.apply_available_events(&store)?;
    assert_eq!(restarted_consumer.rows, rows_after_replay);

    Ok(())
}

/// Commits a two-block initial chain as separate epochs and returns the
/// height-2 cursor.
///
/// Heights 1 and 2 commit under safe tip 1. The returned read-path cursor at
/// height 2 carries an enriched locator over heights 2 and 1, so a reorg of
/// height 2 resolves the fork at height 1.
fn commit_reorgable_chain_and_cursor(
    store: &PrimaryChainStore,
) -> eyre::Result<StreamCursorTokenV1> {
    let (first_epoch, first_block, first_compact_block) =
        synthetic_epoch_with_safe_tip(1, 1, 1, block_hash(1));
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;
    let (second_epoch, second_block, second_compact_block) =
        synthetic_epoch_with_safe_tip(2, 2, 1, block_hash(2));
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        second_epoch,
        vec![second_block],
        vec![second_compact_block],
    ))?;

    let page = store.chain_event_history(ChainEventHistoryRequest::new(
        None,
        NonZeroU32::new(2).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    Ok(page
        .get(1)
        .ok_or_else(|| eyre!("expected a height-2 event"))?
        .cursor
        .clone())
}

/// Commits the reorg that replaces height 2, optionally stamping the
/// replacement epoch with a distinct creation time for retention separation.
fn commit_height_two_reorg(
    store: &PrimaryChainStore,
    created_at: UnixTimestampMillis,
) -> eyre::Result<()> {
    let (mut replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch_with_safe_tip(3, 2, 1, block_hash(20));
    replacement_epoch.created_at = created_at;
    store.commit_chain_epoch(
        super::synthetic_chain_epoch_artifacts(
            replacement_epoch,
            vec![replacement_block],
            vec![replacement_compact_block],
        )
        .with_reorg_window_change(ReorgWindowChange::Replace {
            from_height: BlockHeight::new(2),
        }),
    )?;
    Ok(())
}

#[test]
fn within_retention_reorg_reconnect_replays_the_real_reorg() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let cursor = commit_reorgable_chain_and_cursor(&store)?;
    commit_height_two_reorg(&store, UnixTimestampMillis::new(1_774_668_300_000))?;

    // Resume from the pre-reorg cursor while its event row is still retained:
    // the real ChainReorged at the next sequence replays, with no synthesis.
    let resumed = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(resumed.len(), 1);
    let reorg = resumed.first().ok_or_else(|| eyre!("expected one event"))?;
    assert_eq!(reorg.event_sequence, 3);
    assert!(matches!(reorg.event, ChainEvent::ChainReorged { .. }));

    Ok(())
}

#[test]
fn past_retention_reorg_reconnect_synthesizes_a_reorg() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let cursor = commit_reorgable_chain_and_cursor(&store)?;
    commit_height_two_reorg(&store, UnixTimestampMillis::new(1_774_668_300_000))?;

    // Prune every event older than the reorg so the pre-reorg events are gone
    // and only the reorg event at sequence 3 remains retained.
    let report = store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_300_000))?;
    assert_eq!(report.oldest_retained_sequence, Some(3));

    // The pre-reorg cursor's branch is reorged out and its event row is gone.
    // The locator resolves the fork point at height 1, so the server injects a
    // synthetic ChainReorged ahead of the retained reorg event.
    let resumed = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;

    let synthetic = resumed.first().ok_or_else(|| eyre!("expected events"))?;
    let ChainEvent::ChainReorged { reverted, .. } = &synthetic.event else {
        return Err(eyre!(
            "expected a synthetic ChainReorged first, got {synthetic:?}"
        ));
    };
    assert_eq!(reverted.block_range.start, BlockHeight::new(2));
    assert_eq!(reverted.block_range.end, BlockHeight::new(2));

    // The synthetic envelope carries the consumer's own cursor, so a reconnect
    // that has not yet applied the reorg recomputes the identical recovery.
    let replayed = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&synthetic.cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    let replayed_first = replayed.first().ok_or_else(|| eyre!("expected events"))?;
    assert!(matches!(
        replayed_first.event,
        ChainEvent::ChainReorged { .. }
    ));

    Ok(())
}

/// `LiveTail` resolves once at subscribe time: events already in the log are
/// skipped and a later commit is the first event delivered after the resolved
/// cursor.
#[test]
fn live_tail_start_skips_prior_events_and_delivers_later_commits() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    for height in 1..=2 {
        let (chain_epoch, block, compact_block) = synthetic_epoch(u64::from(height), height);
        store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
            chain_epoch,
            vec![block],
            vec![compact_block],
        ))?;
    }

    let resume = store.resolve_chain_event_stream_start(
        &EventStreamStartPosition::LiveTail,
        ChainEventStreamFamily::Tip,
    )?;
    assert_eq!(resume.family, ChainEventStreamFamily::Tip);
    let head_cursor = resume
        .cursor
        .ok_or_else(|| eyre!("live tail on a non-empty log must mint a head cursor"))?;

    let quiet_page = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&head_cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert!(quiet_page.is_empty());

    let (third_epoch, third_block, third_compact_block) = synthetic_epoch(3, 3);
    let third_commit = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        third_epoch,
        vec![third_block],
        vec![third_compact_block],
    ))?;

    let delivered = store.chain_event_history(ChainEventHistoryRequest::new(
        Some(&head_cursor),
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    assert_eq!(delivered, vec![third_commit.event_envelope]);

    Ok(())
}

/// A `LiveTail` head cursor carries the requested family so later pages stay
/// on the safe-tip stream.
#[test]
fn live_tail_start_mints_a_cursor_in_the_requested_family() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let resume = store.resolve_chain_event_stream_start(
        &EventStreamStartPosition::LiveTail,
        ChainEventStreamFamily::Safe,
    )?;
    assert_eq!(resume.family, ChainEventStreamFamily::Safe);
    let head_cursor = resume
        .cursor
        .ok_or_else(|| eyre!("live tail on a non-empty log must mint a head cursor"))?;
    assert_eq!(head_cursor.as_bytes()[49], 0x1);

    Ok(())
}

/// `LiveTail` on an empty event log degrades to the retention floor, which
/// only widens delivery.
#[test]
fn live_tail_start_on_empty_log_starts_at_earliest_retained() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;

    let resume = store.resolve_chain_event_stream_start(
        &EventStreamStartPosition::LiveTail,
        ChainEventStreamFamily::Tip,
    )?;

    assert_eq!(resume.cursor, None);
    assert_eq!(resume.family, ChainEventStreamFamily::Tip);

    Ok(())
}

/// With `after_cursor` the cursor's encoded family is authoritative when the
/// request family is left at its default.
#[test]
fn after_cursor_start_takes_the_cursor_family_as_authoritative() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    let safe_page = store.chain_event_history(ChainEventHistoryRequest::new_for_family(
        None,
        ChainEventStreamFamily::Safe,
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    let safe_cursor = safe_page
        .first()
        .ok_or_else(|| eyre!("expected a safe-family event"))?
        .cursor
        .clone();

    let resume = store.resolve_chain_event_stream_start(
        &EventStreamStartPosition::AfterCursor(safe_cursor.clone()),
        ChainEventStreamFamily::Tip,
    )?;

    assert_eq!(resume.family, ChainEventStreamFamily::Safe);
    assert_eq!(resume.cursor, Some(safe_cursor));

    Ok(())
}

/// A non-default request family that disagrees with the cursor's encoded
/// family is rejected as an invalid cursor.
#[test]
fn after_cursor_start_rejects_request_family_mismatch() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
    let commit_outcome = store.commit_chain_epoch(super::synthetic_chain_epoch_artifacts(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    let tip_cursor = commit_outcome.event_envelope.cursor;

    let error = match store.resolve_chain_event_stream_start(
        &EventStreamStartPosition::AfterCursor(tip_cursor),
        ChainEventStreamFamily::Safe,
    ) {
        Ok(resume) => return Err(eyre!("expected family mismatch, got {resume:?}")),
        Err(error) => error,
    };

    assert!(matches!(error, StoreError::ChainEventCursorInvalid { .. }));

    Ok(())
}

fn assert_committed_event(event_envelope: &ChainEventEnvelope, chain_epoch: ChainEpoch) {
    assert_eq!(event_envelope.event_sequence, 1);
    assert_eq!(event_envelope.chain_epoch, chain_epoch);
    assert_eq!(
        event_envelope.safe_tip_height,
        chain_epoch.settled_tip_height
    );
    assert!(matches!(
        &event_envelope.event,
        ChainEvent::ChainCommitted { committed } if committed.chain_epoch == chain_epoch
    ));
}

#[derive(Clone, Debug, Default)]
struct TestDerivedConsumer {
    cursor: Option<StreamCursorTokenV1>,
    rows: Vec<DerivedBlockRow>,
}

impl TestDerivedConsumer {
    fn apply_available_events(&mut self, store: &PrimaryChainStore) -> Result<(), StoreError> {
        for event_envelope in store.chain_event_history(ChainEventHistoryRequest::new(
            self.cursor.as_ref(),
            NonZeroU32::new(10).ok_or(StoreError::InvalidChainStoreOptions {
                reason: "test max events must be non-zero",
            })?,
        ))? {
            self.apply_event(&event_envelope.event);
            self.cursor = Some(event_envelope.cursor);
        }

        Ok(())
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "test consumer ignores future chain event variants it does not model"
    )]
    fn apply_event(&mut self, chain_event: &ChainEvent) {
        match chain_event {
            ChainEvent::ChainCommitted { committed } => self.apply_committed(committed),
            ChainEvent::ChainReorged {
                reverted,
                committed,
            } => {
                self.apply_reverted(reverted.block_range);
                self.apply_committed(committed);
            }
            _ => {}
        }
    }

    fn apply_committed(&mut self, committed: &zinder_store::ChainEpochCommitted) {
        let block_range = committed.block_range;

        self.apply_reverted(block_range);
        for height in block_range.start.value()..=block_range.end.value() {
            self.rows.push(DerivedBlockRow {
                height: BlockHeight::new(height),
                chain_epoch: committed.chain_epoch.id,
                block_hash: committed.chain_epoch.visible_tip_hash,
            });
        }
        self.rows.sort_by_key(|row| row.height.value());
    }

    fn apply_reverted(&mut self, block_range: zinder_core::BlockHeightRange) {
        self.rows
            .retain(|row| row.height < block_range.start || row.height > block_range.end);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DerivedBlockRow {
    height: BlockHeight,
    chain_epoch: ChainEpochId,
    block_hash: BlockHash,
}

fn synthetic_epoch(
    chain_epoch_id: u64,
    height: u32,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    synthetic_epoch_with_safe_tip(chain_epoch_id, height, height, block_hash(height))
}

fn synthetic_epoch_with_safe_tip(
    chain_epoch_id: u64,
    height: u32,
    safe_tip_height: u32,
    source_hash: BlockHash,
) -> (ChainEpoch, BlockHeaderArtifact, CompactBlockArtifact) {
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            visible_tip_height: block_height,
            visible_tip_hash: source_hash,
            settled_tip_height: BlockHeight::new(safe_tip_height),
            settled_tip_hash: block_hash(safe_tip_height),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_200_000 + u64::from(height)),
        },
        super::synthetic_block_header(
            block_height,
            source_hash,
            parent_hash,
            format!("raw-block-{chain_epoch_id}-{height}").as_bytes(),
        ),
        CompactBlockArtifact::new(
            block_height,
            source_hash,
            format!("compact-block-{chain_epoch_id}-{height}").into_bytes(),
        ),
    )
}

fn block_hash(seed: u32) -> BlockHash {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&seed.to_be_bytes());
    }
    BlockHash::from_bytes(bytes)
}
