//! Read-only verification of persisted canonical replay history.

use serde::Serialize;
use thiserror::Error;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, CanonicalBlockFactsSequenceDigestBuilder,
    CanonicalBlockFactsSequenceDigestVersion, CanonicalBlockFactsSequenceLengthOverflow,
    wire::encode_zinder_native_chain_name,
};
use zinder_store::{
    BlockReplayBatchRequest, ChainEpochReader, MAX_BLOCK_REPLAY_BATCH_BLOCKS, SecondaryChainStore,
    StoreError,
};

const REPLAY_ENVELOPE_AND_CANONICAL_HEADER_PARITY_SCOPE: &str =
    "replay_envelope_and_canonical_header_parity";

struct VerifiedReplayBatch {
    end_height: BlockHeight,
    final_hash: Option<BlockHash>,
}

/// Machine-readable evidence for one pinned canonical replay scan.
#[derive(Debug, Serialize)]
pub(crate) struct CanonicalReplayVerificationReport {
    verification_scope: &'static str,
    network: String,
    chain_epoch_id: u64,
    from_height: Option<u32>,
    to_height: Option<u32>,
    block_count: u64,
    sequence_digest_version: u16,
    sequence_digest_sha256: String,
}

impl CanonicalReplayVerificationReport {
    /// Encodes the report as one JSON object for the operator CLI.
    pub(crate) fn to_json(&self) -> Result<String, CanonicalReplayVerificationError> {
        serde_json::to_string(self)
            .map_err(|source| CanonicalReplayVerificationError::ReportEncode { source })
    }
}

/// Failure to verify one pinned canonical replay history.
#[derive(Debug, Error)]
pub(crate) enum CanonicalReplayVerificationError {
    /// Canonical storage could not serve a complete, valid pinned read.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The bounded store read returned fewer rows than its requested range.
    #[error(
        "canonical replay batch at height {start_height} returned {actual_block_count} blocks; expected {expected_block_count}"
    )]
    IncompleteBatch {
        start_height: u32,
        expected_block_count: u32,
        actual_block_count: usize,
    },

    /// The bounded canonical header read returned an unexpected number of rows.
    #[error(
        "canonical block-header batch at height {start_height} returned {actual_block_count} rows; expected {expected_block_count}"
    )]
    IncompleteCanonicalHeaderBatch {
        start_height: u32,
        expected_block_count: u32,
        actual_block_count: usize,
    },

    /// The canonical block-header family did not contain the replayed height.
    #[error("canonical block header is missing at replay height {height}")]
    CanonicalHeaderMissing { height: u32 },

    /// Replay header facts differed from the independently persisted header row.
    #[error("canonical replay header differs from the canonical block header at height {height}")]
    CanonicalHeaderMismatch { height: u32 },

    /// The replay sequence did not connect to its checkpoint or preceding block.
    #[error("canonical replay parent continuity failed at height {height}")]
    ParentContinuityMismatch { height: u32 },

    /// The final replay did not identify the pinned epoch's visible tip.
    #[error("canonical replay tip identity differs from the pinned chain epoch at height {height}")]
    TipIdentityMismatch { height: u32 },

    /// The ordered sequence exceeded its encoded item count.
    #[error(transparent)]
    SequenceLength(#[from] CanonicalBlockFactsSequenceLengthOverflow),

    /// The machine-readable report could not be encoded.
    #[error("failed to encode canonical replay verification report: {source}")]
    ReportEncode {
        #[source]
        source: serde_json::Error,
    },
}

/// Verifies every replay row retained by one secondary-visible chain epoch.
///
/// The caller may catch the secondary up before invoking this function, but
/// must not catch it up during the scan. Primary advancement is harmless: the
/// verifier reads only the secondary's pinned epoch and never refreshes it from
/// the primary.
pub(crate) fn verify_canonical_replay_store(
    secondary_store: &SecondaryChainStore,
) -> Result<CanonicalReplayVerificationReport, CanonicalReplayVerificationError> {
    verify_canonical_replay_store_at_batch_boundaries(secondary_store, |_| Ok(()))
}

fn verify_canonical_replay_store_at_batch_boundaries(
    secondary_store: &SecondaryChainStore,
    mut observe_batch_boundary: impl FnMut(BlockHeight) -> Result<(), CanonicalReplayVerificationError>,
) -> Result<CanonicalReplayVerificationReport, CanonicalReplayVerificationError> {
    let reader = secondary_store.current_chain_epoch_reader()?;
    let chain_epoch = reader.chain_epoch();
    let history_bounds = reader.canonical_history_bounds();
    let first_available_height = history_bounds.first_available_height();
    let visible_tip_height = chain_epoch.visible_tip_height;
    let has_retained_replay = first_available_height <= visible_tip_height;
    let mut next_height = first_available_height;
    let mut preceding_block_hash = history_bounds
        .preceding_checkpoint()
        .map(|checkpoint| checkpoint.hash);
    let mut final_replay_hash = None;
    let mut sequence_digest_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
        CanonicalBlockFactsSequenceDigestVersion::CURRENT,
    );

    while next_height <= visible_tip_height {
        let remaining_blocks = visible_tip_height
            .value()
            .saturating_sub(next_height.value())
            .saturating_add(1);
        let batch_blocks =
            std::num::NonZeroU32::new(remaining_blocks.min(MAX_BLOCK_REPLAY_BATCH_BLOCKS.get()))
                .unwrap_or(std::num::NonZeroU32::MIN);
        let verified_batch = verify_replay_batch(
            &reader,
            next_height,
            batch_blocks,
            preceding_block_hash,
            &mut sequence_digest_builder,
        )?;
        preceding_block_hash = verified_batch.final_hash;
        final_replay_hash = verified_batch.final_hash;

        if verified_batch.end_height >= visible_tip_height {
            break;
        }
        observe_batch_boundary(verified_batch.end_height)?;
        next_height = BlockHeight::new(verified_batch.end_height.value().saturating_add(1));
    }

    if final_replay_hash.is_some_and(|hash| hash != chain_epoch.visible_tip_hash) {
        return Err(CanonicalReplayVerificationError::TipIdentityMismatch {
            height: visible_tip_height.value(),
        });
    }

    let observed_epoch = secondary_store
        .current_chain_epoch()?
        .ok_or(StoreError::NoVisibleChainEpoch)?;
    if observed_epoch.id != chain_epoch.id {
        return Err(StoreError::ChainEpochConflict {
            current: observed_epoch.id,
            attempted: chain_epoch.id,
        }
        .into());
    }

    let sequence_digest = sequence_digest_builder.finish();
    Ok(CanonicalReplayVerificationReport {
        verification_scope: REPLAY_ENVELOPE_AND_CANONICAL_HEADER_PARITY_SCOPE,
        network: encode_zinder_native_chain_name(chain_epoch.network).to_owned(),
        chain_epoch_id: chain_epoch.id.value(),
        from_height: has_retained_replay.then_some(first_available_height.value()),
        to_height: has_retained_replay.then_some(visible_tip_height.value()),
        block_count: sequence_digest.block_count(),
        sequence_digest_version: sequence_digest.version().value(),
        sequence_digest_sha256: hex::encode(sequence_digest.as_bytes()),
    })
}

fn verify_replay_batch(
    reader: &ChainEpochReader<'_>,
    start_height: BlockHeight,
    block_count: std::num::NonZeroU32,
    mut preceding_block_hash: Option<BlockHash>,
    sequence_digest_builder: &mut CanonicalBlockFactsSequenceDigestBuilder,
) -> Result<VerifiedReplayBatch, CanonicalReplayVerificationError> {
    let replay_batch =
        reader.block_replay_batch(BlockReplayBatchRequest::new(start_height, block_count))?;
    if replay_batch.len() != block_count.get() as usize {
        return Err(CanonicalReplayVerificationError::IncompleteBatch {
            start_height: start_height.value(),
            expected_block_count: block_count.get(),
            actual_block_count: replay_batch.len(),
        });
    }
    let end_height = BlockHeight::new(
        start_height
            .value()
            .saturating_add(block_count.get().saturating_sub(1)),
    );
    let canonical_headers =
        reader.block_headers_in_range(BlockHeightRange::inclusive(start_height, end_height))?;
    if canonical_headers.len() != block_count.get() as usize {
        return Err(
            CanonicalReplayVerificationError::IncompleteCanonicalHeaderBatch {
                start_height: start_height.value(),
                expected_block_count: block_count.get(),
                actual_block_count: canonical_headers.len(),
            },
        );
    }
    for (replay, canonical_header) in replay_batch.into_iter().zip(canonical_headers) {
        let replay_header = &replay.facts().block_header;
        let canonical_header =
            canonical_header.ok_or(CanonicalReplayVerificationError::CanonicalHeaderMissing {
                height: replay_header.height.value(),
            })?;
        if *replay_header != canonical_header {
            return Err(CanonicalReplayVerificationError::CanonicalHeaderMismatch {
                height: replay_header.height.value(),
            });
        }
        if preceding_block_hash
            .is_some_and(|expected_parent_hash| replay_header.parent_hash != expected_parent_hash)
        {
            return Err(CanonicalReplayVerificationError::ParentContinuityMismatch {
                height: replay_header.height.value(),
            });
        }
        sequence_digest_builder.try_append(replay.reference_digest())?;
        preceding_block_hash = Some(replay_header.block_hash);
    }

    Ok(VerifiedReplayBatch {
        end_height,
        final_hash: preceding_block_hash,
    })
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::Path};

    use rust_rocksdb::{DB, Options};
    use tempfile::tempdir;
    use zinder_core::{
        BlockHeight, CanonicalBlockFactsSequenceDigestBuilder,
        CanonicalBlockFactsSequenceDigestVersion, ChainEpochId, Network,
        decode_canonical_block_replay,
    };
    use zinder_store::{
        ArtifactFamily, ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore,
        SecondaryChainStore, StoreError,
    };
    use zinder_testkit::ChainFixture;

    use super::{
        CanonicalReplayVerificationError, verify_canonical_replay_store,
        verify_canonical_replay_store_at_batch_boundaries,
    };

    const BLOCK_REPLAY_COLUMN_FAMILY: &str = "block_replay";

    #[test]
    fn verification_scans_multiple_bounded_batches() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let primary_path = tempdir.path().join("canonical");
        let secondary_path = tempdir.path().join("verification-secondary");
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(257);
        let artifacts = chain_fixture
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("chain fixture unexpectedly empty")?;
        let mut expected_digest_builder = CanonicalBlockFactsSequenceDigestBuilder::new(
            CanonicalBlockFactsSequenceDigestVersion::CURRENT,
        );
        for replay_envelope in &artifacts.block_replay_envelopes {
            expected_digest_builder.try_append(
                decode_canonical_block_replay(replay_envelope.as_bytes())?.reference_digest(),
            )?;
        }
        let expected_digest = expected_digest_builder.finish();
        let primary = PrimaryChainStore::open(
            &primary_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        primary.commit_chain_epoch(artifacts)?;
        let secondary = SecondaryChainStore::open(
            &primary_path,
            &secondary_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;

        let report = verify_canonical_replay_store(&secondary)?;
        let report_json: serde_json::Value = serde_json::from_str(&report.to_json()?)?;
        assert_eq!(
            report_json["verification_scope"],
            "replay_envelope_and_canonical_header_parity"
        );
        assert_eq!(report_json["from_height"], 1);
        assert_eq!(report_json["to_height"], 257);
        assert_eq!(report_json["block_count"], 257);
        assert_eq!(
            report_json["sequence_digest_sha256"],
            hex::encode(expected_digest.as_bytes())
        );

        Ok(())
    }

    #[test]
    fn verification_keeps_its_epoch_when_primary_advances_between_batches()
    -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let primary_path = tempdir.path().join("canonical");
        let secondary_path = tempdir.path().join("verification-secondary");
        let initial_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(257);
        let initial_artifacts = initial_fixture
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("initial fixture unexpectedly empty")?;
        let primary = PrimaryChainStore::open(
            &primary_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        primary.commit_chain_epoch(initial_artifacts)?;
        let secondary = SecondaryChainStore::open(
            &primary_path,
            &secondary_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;

        let appended_artifacts = initial_fixture
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(2))
            .ok_or("appended fixture unexpectedly empty")?;
        let mut appended_tail = Some(appended_tail(appended_artifacts, 257));

        let report =
            verify_canonical_replay_store_at_batch_boundaries(&secondary, |completed_through| {
                assert_eq!(completed_through, BlockHeight::new(256));
                if let Some(artifacts) = appended_tail.take() {
                    primary.commit_chain_epoch(artifacts)?;
                }
                Ok(())
            })?;
        let report_json: serde_json::Value = serde_json::from_str(&report.to_json()?)?;
        assert_eq!(report_json["chain_epoch_id"], 1);
        assert_eq!(report_json["to_height"], 257);
        assert!(appended_tail.is_none());

        Ok(())
    }

    #[test]
    fn verification_rejects_secondary_catch_up_between_batches() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let primary_path = tempdir.path().join("canonical");
        let secondary_path = tempdir.path().join("verification-secondary");
        let initial_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(257);
        let initial_artifacts = initial_fixture
            .chain_epoch_artifacts(ChainEpochId::new(1))
            .ok_or("initial fixture unexpectedly empty")?;
        let primary = PrimaryChainStore::open(
            &primary_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        primary.commit_chain_epoch(initial_artifacts)?;
        let secondary = SecondaryChainStore::open(
            &primary_path,
            &secondary_path,
            ChainStoreOptions::for_network(Network::ZcashRegtest),
        )?;
        let appended_artifacts = initial_fixture
            .extend_blocks(1)
            .chain_epoch_artifacts(ChainEpochId::new(2))
            .ok_or("appended fixture unexpectedly empty")?;
        let mut appended_tail = Some(appended_tail(appended_artifacts, 257));

        let verification_result =
            verify_canonical_replay_store_at_batch_boundaries(&secondary, |completed_through| {
                assert_eq!(completed_through, BlockHeight::new(256));
                if let Some(artifacts) = appended_tail.take() {
                    primary.commit_chain_epoch(artifacts)?;
                }
                secondary.try_catch_up()?;
                Ok(())
            });
        let error = match verification_result {
            Ok(report) => {
                return Err(format!(
                    "refreshing the secondary unexpectedly preserved the pinned scan: {report:?}"
                )
                .into());
            }
            Err(error) => error,
        };
        assert!(matches!(
            error,
            CanonicalReplayVerificationError::Store(StoreError::ChainEpochConflict {
                current,
                attempted,
            }) if current == ChainEpochId::new(2) && attempted == ChainEpochId::new(1)
        ));

        Ok(())
    }

    #[test]
    fn verification_rejects_missing_and_corrupt_replay_rows() -> Result<(), Box<dyn Error>> {
        for mutation in [StoredReplayMutation::Delete, StoredReplayMutation::Corrupt] {
            let tempdir = tempdir()?;
            let primary_path = tempdir.path().join("canonical");
            let secondary_path = tempdir.path().join("verification-secondary");
            let artifacts = ChainFixture::new(Network::ZcashRegtest)
                .extend_blocks(1)
                .chain_epoch_artifacts(ChainEpochId::new(1))
                .ok_or("chain fixture unexpectedly empty")?;
            {
                let primary = PrimaryChainStore::open(
                    &primary_path,
                    ChainStoreOptions::for_network(Network::ZcashRegtest),
                )?;
                primary.commit_chain_epoch(artifacts)?;
            }
            mutate_first_replay_row(&primary_path, mutation)?;
            let secondary = SecondaryChainStore::open(
                &primary_path,
                &secondary_path,
                ChainStoreOptions::for_network(Network::ZcashRegtest),
            )?;

            let error = match verify_canonical_replay_store(&secondary) {
                Ok(report) => {
                    return Err(
                        format!("verification accepted a mutated replay row: {report:?}").into(),
                    );
                }
                Err(error) => error,
            };
            match mutation {
                StoredReplayMutation::Delete => assert!(matches!(
                    error,
                    CanonicalReplayVerificationError::Store(StoreError::ArtifactMissing {
                        family: ArtifactFamily::BlockReplay,
                        ..
                    })
                )),
                StoredReplayMutation::Corrupt => assert!(matches!(
                    error,
                    CanonicalReplayVerificationError::Store(StoreError::ArtifactCorrupt {
                        family: ArtifactFamily::BlockReplay,
                        ..
                    })
                )),
            }
        }

        Ok(())
    }

    fn appended_tail(
        mut artifacts: ChainEpochArtifacts,
        retained_prefix_blocks: usize,
    ) -> ChainEpochArtifacts {
        ChainEpochArtifacts::new(
            artifacts.chain_epoch,
            artifacts.block_headers.split_off(retained_prefix_blocks),
            artifacts
                .block_replay_envelopes
                .split_off(retained_prefix_blocks),
            artifacts.compact_blocks.split_off(retained_prefix_blocks),
        )
    }

    #[derive(Clone, Copy)]
    enum StoredReplayMutation {
        Delete,
        Corrupt,
    }

    fn mutate_first_replay_row(
        primary_path: &Path,
        mutation: StoredReplayMutation,
    ) -> Result<(), Box<dyn Error>> {
        let column_families = DB::list_cf(&Options::default(), primary_path)?;
        let database = DB::open_cf(&Options::default(), primary_path, column_families)?;
        let column_family = database
            .cf_handle(BLOCK_REPLAY_COLUMN_FAMILY)
            .ok_or("block replay column family is missing")?;
        let mut iterator = database.raw_iterator_cf(&column_family);
        iterator.seek_to_first();
        let first_key = iterator
            .key()
            .ok_or("block replay column family is empty")?
            .to_vec();
        drop(iterator);
        match mutation {
            StoredReplayMutation::Delete => database.delete_cf(&column_family, first_key)?,
            StoredReplayMutation::Corrupt => {
                database.put_cf(&column_family, first_key, [0xff])?;
            }
        }
        database.flush_cf(&column_family)?;

        Ok(())
    }
}
