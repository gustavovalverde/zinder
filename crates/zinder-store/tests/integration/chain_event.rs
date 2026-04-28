#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::eyre;
use tempfile::tempdir;
use zinder_core::{
    ArtifactSchemaVersion, BlockArtifact, BlockHash, BlockHeight, ChainEpoch, ChainEpochId,
    ChainTipMetadata, CompactBlockArtifact, Network, UnixTimestampMillis,
};
use zinder_store::{
    ChainEpochArtifacts, ChainEvent, ChainEventEnvelope, ChainEventHistoryRequest,
    ChainEventStreamFamily, ChainStoreOptions, PrimaryChainStore, ReorgWindowChange, StoreError,
    StreamCursorTokenV1,
};

#[test]
fn chain_event_history_resumes_after_persisted_cursor() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_epoch(2, 2);

    let first_commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;
    let second_commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
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
fn chain_event_history_returns_bounded_pages() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let mut committed_envelopes = Vec::new();

    for height in 1..=3 {
        let (chain_epoch, block, compact_block) = synthetic_epoch(u64::from(height), height);
        let commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
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
        let commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
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
        StoreError::EventCursorExpired {
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
        let commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
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
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let event_history = store.chain_event_history(ChainEventHistoryRequest::new_for_family(
        None,
        ChainEventStreamFamily::Finalized,
        NonZeroU32::new(10).ok_or_else(|| eyre!("invalid max events"))?,
    ))?;
    let event_envelope = event_history
        .first()
        .ok_or_else(|| eyre!("expected finalized event"))?;

    assert_eq!(event_envelope.cursor.as_bytes()[49], 0x1);

    Ok(())
}

#[test]
fn chain_event_history_survives_store_reopen() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let committed_envelope = {
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        let commit_outcome = store.commit_chain_epoch(ChainEpochArtifacts::new(
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
    let commit_outcome = store.commit_chain_epoch(ChainEpochArtifacts::new(
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

    assert!(matches!(error, StoreError::EventCursorInvalid { .. }));

    Ok(())
}

#[test]
fn cursor_from_another_store_is_rejected() -> eyre::Result<()> {
    let first_tempdir = tempdir()?;
    let first_store =
        PrimaryChainStore::open(first_tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (first_epoch, first_block, first_compact_block) = synthetic_epoch(1, 1);
    let first_commit = first_store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block],
    ))?;

    let second_tempdir = tempdir()?;
    let second_store =
        PrimaryChainStore::open(second_tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (second_epoch, second_block, second_compact_block) = synthetic_epoch(1, 1);
    second_store.commit_chain_epoch(ChainEpochArtifacts::new(
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

    assert!(matches!(error, StoreError::EventCursorInvalid { .. }));

    Ok(())
}

#[test]
fn cursor_before_any_commits_is_rejected() -> eyre::Result<()> {
    let cursor = {
        let tempdir = tempdir()?;
        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);
        store
            .commit_chain_epoch(ChainEpochArtifacts::new(
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

    assert!(matches!(error, StoreError::EventCursorInvalid { .. }));

    Ok(())
}

#[test]
fn commit_outcome_includes_cursor_bound_chain_event() -> eyre::Result<()> {
    let tempdir = tempdir()?;
    let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
    let (chain_epoch, block, compact_block) = synthetic_epoch(1, 1);

    let commit_outcome = store.commit_chain_epoch(ChainEpochArtifacts::new(
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
        synthetic_epoch_with_finalized(1, 2, 1, block_hash(2));
    store.commit_chain_epoch(ChainEpochArtifacts::new(
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
                block_hash: initial_epoch.tip_hash,
            },
            DerivedBlockRow {
                height: BlockHeight::new(2),
                chain_epoch: initial_epoch.id,
                block_hash: initial_epoch.tip_hash,
            }
        ]
    );

    let replacement_hash = block_hash(20);
    let (replacement_epoch, replacement_block, replacement_compact_block) =
        synthetic_epoch_with_finalized(2, 2, 1, replacement_hash);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
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
                block_hash: initial_epoch.tip_hash,
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

fn assert_committed_event(event_envelope: &ChainEventEnvelope, chain_epoch: ChainEpoch) {
    assert_eq!(event_envelope.event_sequence, 1);
    assert_eq!(event_envelope.chain_epoch, chain_epoch);
    assert_eq!(
        event_envelope.finalized_height,
        chain_epoch.finalized_height
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
                block_hash: committed.chain_epoch.tip_hash,
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
) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
    synthetic_epoch_with_finalized(chain_epoch_id, height, height, block_hash(height))
}

fn synthetic_epoch_with_finalized(
    chain_epoch_id: u64,
    height: u32,
    finalized_height: u32,
    source_hash: BlockHash,
) -> (ChainEpoch, BlockArtifact, CompactBlockArtifact) {
    let parent_hash = block_hash(height.saturating_sub(1));
    let block_height = BlockHeight::new(height);

    (
        ChainEpoch {
            id: ChainEpochId::new(chain_epoch_id),
            network: Network::ZcashRegtest,
            tip_height: block_height,
            tip_hash: source_hash,
            finalized_height: BlockHeight::new(finalized_height),
            finalized_hash: block_hash(finalized_height),
            artifact_schema_version: ArtifactSchemaVersion::new(1),
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_200_000 + u64::from(height)),
        },
        BlockArtifact::new(
            block_height,
            source_hash,
            parent_hash,
            format!("raw-block-{chain_epoch_id}-{height}").into_bytes(),
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
