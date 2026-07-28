//! `ExplorerQuery.CommitmentRootSearch` handler.
//!
//! Reads the materialized-view reverse index, then validates every candidate against one
//! pinned canonical epoch. The second check is intentional: an ingest-owned
//! historical enrichment can commit just before a reorg, so an obsolete
//! physical materialized-view row must never become a canonical match.

use std::num::NonZeroU32;

use tonic::{Request, Response, Status};
use zinder_core::{
    BlockFinalNoteCommitmentRoots, BlockHeight, CanonicalHistoryBounds, ChainEpoch,
    DisplacedRootArchiveCoverage, FinalNoteCommitmentRoot, NetworkUpgradeActivations,
    ShieldedProtocol as CoreProtocol,
};
use zinder_materialized_views::{
    COMMITMENT_ROOT_SEARCH_INDEX_COLUMN_FAMILY, CommitmentRootBackfillCoverage,
    CommitmentRootSearchConsumer, MaterializedViewStore,
};
use zinder_proto::capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1;
use zinder_proto::v1::explorer::{
    CommitmentRootMatch, CommitmentRootSearchCoverage, CommitmentRootSearchDisplacedCoverage,
    CommitmentRootSearchRequest, CommitmentRootSearchResponse, ShieldedProtocol,
};
use zinder_store::{ChainEpochReader, SecondaryChainStore, chain_epoch_message};

use super::clamp_max_entries;
use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};

const DEFAULT_MAX_MATCHES: u32 = 10;
const MAX_MATCHES: u32 = 100;
const MAX_CANDIDATES_SCANNED: usize = 1_024;
const MAX_RECENT_COVERAGE_ROWS: usize = 10_000;

/// Composed dependencies and admitted fields for one root search.
pub(crate) struct CommitmentRootSearchContext<'store> {
    pub(crate) materialized_view_store: &'store MaterializedViewStore,
    pub(crate) canonical_store: &'store SecondaryChainStore,
    pub(crate) activations: &'store NetworkUpgradeActivations,
    pub(crate) upstream_observation_cache: &'store UpstreamObservationCache,
    pub(crate) include_displaced_root_results: bool,
}

/// Executes one canonical final-root search.
pub(crate) async fn query_commitment_root_search(
    context: CommitmentRootSearchContext<'_>,
    request: Request<CommitmentRootSearchRequest>,
) -> Result<Response<CommitmentRootSearchResponse>, Status> {
    let CommitmentRootSearchContext {
        materialized_view_store,
        canonical_store,
        activations,
        upstream_observation_cache,
        include_displaced_root_results,
    } = context;
    let request = request.into_inner();
    let root_bytes: [u8; 32] = request.root.as_slice().try_into().map_err(|_| {
        ExplorerError::invalid_request("commitment root must contain exactly 32 bytes")
    })?;
    let root = FinalNoteCommitmentRoot::from_bytes(root_bytes);
    let max_matches = clamp_max_entries(request.max_matches, DEFAULT_MAX_MATCHES, MAX_MATCHES);

    materialized_view_store
        .try_catch_up()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    canonical_store
        .try_catch_up()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = canonical_store
        .current_chain_epoch_reader()
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let chain_epoch = reader.chain_epoch();
    let canonical_history_bounds = reader.canonical_history_bounds();
    let sapling_activation_height = activations
        .activation_height_by_name("Sapling")
        .ok_or_else(|| {
            ExplorerError::internal("network upgrade activations do not include Sapling")
        })?;
    let matches = canonical_root_matches(materialized_view_store, &reader, root, max_matches)?;
    let (displaced_matches, displaced_coverage) =
        read_admitted_displaced_root_results(include_displaced_root_results, || {
            let matches = displaced_root_matches(&reader, root, max_matches)?;
            let coverage = displaced_root_search_coverage(
                reader
                    .displaced_root_archive_coverage()
                    .map_err(|error| ExplorerError::internal(error.to_string()))?,
            );
            Ok((matches, coverage))
        })?;
    let coverage = commitment_root_search_coverage(
        materialized_view_store,
        chain_epoch,
        canonical_history_bounds,
        sapling_activation_height,
    )?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(materialized_view_store),
            EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
            Some(chain_epoch_message(chain_epoch)),
            0,
        )?,
    )
    .await;
    Ok(Response::new(CommitmentRootSearchResponse {
        freshness: Some(freshness),
        matches,
        coverage: Some(coverage),
        displaced_matches,
        displaced_coverage,
    }))
}

fn read_admitted_displaced_root_results(
    include_displaced_root_results: bool,
    read: impl FnOnce() -> Result<
        (
            Vec<CommitmentRootMatch>,
            CommitmentRootSearchDisplacedCoverage,
        ),
        Status,
    >,
) -> Result<
    (
        Vec<CommitmentRootMatch>,
        Option<CommitmentRootSearchDisplacedCoverage>,
    ),
    Status,
> {
    if include_displaced_root_results {
        let (matches, coverage) = read()?;
        Ok((matches, Some(coverage)))
    } else {
        Ok((Vec::new(), None))
    }
}

fn canonical_root_matches(
    materialized_view_store: &MaterializedViewStore,
    reader: &ChainEpochReader<'_>,
    root: FinalNoteCommitmentRoot,
    max_matches: u32,
) -> Result<Vec<CommitmentRootMatch>, Status> {
    let candidates =
        CommitmentRootSearchConsumer::search(materialized_view_store, root, MAX_CANDIDATES_SCANNED)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let mut matches = Vec::with_capacity(max_matches as usize);
    for candidate in candidates {
        let Some(header) = reader
            .block_header_at(candidate.block_height)
            .map_err(|error| ExplorerError::internal(error.to_string()))?
        else {
            continue;
        };
        if header.block_hash != candidate.block_hash {
            continue;
        }
        let Some(roots) = reader
            .final_note_commitment_roots_at(candidate.block_height)
            .map_err(|error| ExplorerError::internal(error.to_string()))?
        else {
            continue;
        };
        if roots.block_hash != candidate.block_hash
            || !root_matches_protocol(roots, candidate.protocol, root)
        {
            continue;
        }
        matches.push(CommitmentRootMatch {
            block_height: candidate.block_height.value(),
            block_hash: zinder_core::wire::encode_rpc_block_hash_hex(candidate.block_hash),
            block_time_unix_seconds: candidate.block_time_unix_seconds,
            protocol: wire_protocol(candidate.protocol)? as i32,
        });
        if matches.len() >= max_matches as usize {
            break;
        }
    }
    Ok(matches)
}

fn displaced_root_matches(
    reader: &ChainEpochReader<'_>,
    root: FinalNoteCommitmentRoot,
    max_matches: u32,
) -> Result<Vec<CommitmentRootMatch>, Status> {
    let candidate_limit = NonZeroU32::new(
        u32::try_from(MAX_CANDIDATES_SCANNED)
            .map_err(|_| ExplorerError::internal("displaced root candidate limit is too large"))?,
    )
    .ok_or_else(|| ExplorerError::internal("displaced root candidate limit is zero"))?;
    let mut candidates = Vec::new();
    for protocol in [
        CoreProtocol::Sapling,
        CoreProtocol::Orchard,
        CoreProtocol::Ironwood,
    ] {
        candidates.extend(
            reader
                .displaced_root_candidates(protocol, root, candidate_limit)
                .map_err(|error| ExplorerError::internal(error.to_string()))?,
        );
    }
    candidates.sort_unstable_by(|left, right| {
        right
            .displacement_event_sequence
            .cmp(&left.displacement_event_sequence)
            .then_with(|| {
                right
                    .block_id
                    .height
                    .value()
                    .cmp(&left.block_id.height.value())
            })
            .then_with(|| {
                right
                    .block_id
                    .hash
                    .as_bytes()
                    .cmp(&left.block_id.hash.as_bytes())
            })
            .then_with(|| right.protocol.id().cmp(&left.protocol.id()))
    });

    let mut matches = Vec::with_capacity(max_matches as usize);
    let mut matched_identities = Vec::new();
    for candidate in candidates {
        let canonical_hash = reader
            .block_header_at(candidate.block_id.height)
            .map_err(|error| ExplorerError::internal(error.to_string()))?
            .map(|header| header.block_hash);
        if canonical_hash == Some(candidate.block_id.hash)
            || matched_identities.iter().any(|(height, hash, protocol)| {
                *height == candidate.block_id.height
                    && *hash == candidate.block_id.hash
                    && *protocol == candidate.protocol
            })
        {
            continue;
        }
        matched_identities.push((
            candidate.block_id.height,
            candidate.block_id.hash,
            candidate.protocol,
        ));
        matches.push(CommitmentRootMatch {
            block_height: candidate.block_id.height.value(),
            block_hash: zinder_core::wire::encode_rpc_block_hash_hex(candidate.block_id.hash),
            block_time_unix_seconds: candidate.block_time_unix_seconds,
            protocol: wire_protocol(candidate.protocol)? as i32,
        });
        if matches.len() >= max_matches as usize {
            break;
        }
    }
    Ok(matches)
}

fn displaced_root_search_coverage(
    coverage: Option<DisplacedRootArchiveCoverage>,
) -> CommitmentRootSearchDisplacedCoverage {
    let Some(coverage) = coverage else {
        return CommitmentRootSearchDisplacedCoverage {
            activation_event_sequence: None,
            activation_epoch_id: None,
            activated_at_millis: None,
            captured_block_count: 0,
            root_artifact_unavailable_count: 0,
            captured_range_complete: false,
        };
    };
    CommitmentRootSearchDisplacedCoverage {
        activation_event_sequence: Some(coverage.activation_event_sequence),
        activation_epoch_id: Some(coverage.activation_epoch.value()),
        activated_at_millis: Some(coverage.activated_at.value()),
        captured_block_count: coverage.captured_block_count,
        root_artifact_unavailable_count: coverage.root_artifact_unavailable_count,
        captured_range_complete: coverage.root_artifact_unavailable_count == 0,
    }
}

fn commitment_root_search_coverage(
    materialized_view_store: &MaterializedViewStore,
    chain_epoch: ChainEpoch,
    canonical_history_bounds: CanonicalHistoryBounds,
    sapling_activation_height: BlockHeight,
) -> Result<CommitmentRootSearchCoverage, Status> {
    let backfill_coverage =
        CommitmentRootSearchConsumer::backfill_coverage(materialized_view_store)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let latest_indexed_height = materialized_view_store
        .last_materialized_height_ascending(COMMITMENT_ROOT_SEARCH_INDEX_COLUMN_FAMILY)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let recent_index_complete = match backfill_coverage {
        Some(coverage) => recent_index_is_contiguous(
            materialized_view_store,
            coverage.complete_through_height,
            chain_epoch.visible_tip_height,
        )?,
        None => false,
    };
    let canonical_history_complete = CommitmentRootCompleteness {
        canonical_history_bounds,
        sapling_activation_height,
        backfill_coverage,
        latest_indexed_height,
        settled_tip_height: chain_epoch.settled_tip_height,
        visible_tip_height: chain_epoch.visible_tip_height,
        recent_index_complete,
    }
    .is_complete();
    Ok(CommitmentRootSearchCoverage {
        complete_from_height: backfill_coverage
            .map(|coverage| coverage.complete_from_height.value()),
        complete_through_height: backfill_coverage
            .map(|coverage| coverage.complete_through_height.value()),
        latest_indexed_height: latest_indexed_height.map(zinder_core::BlockHeight::value),
        canonical_history_complete,
    })
}

struct CommitmentRootCompleteness {
    canonical_history_bounds: CanonicalHistoryBounds,
    sapling_activation_height: BlockHeight,
    backfill_coverage: Option<CommitmentRootBackfillCoverage>,
    latest_indexed_height: Option<BlockHeight>,
    settled_tip_height: BlockHeight,
    visible_tip_height: BlockHeight,
    recent_index_complete: bool,
}

impl CommitmentRootCompleteness {
    fn is_complete(&self) -> bool {
        let expected_backfill_floor = self
            .sapling_activation_height
            .max(self.canonical_history_bounds.first_available_height());
        !self
            .canonical_history_bounds
            .intentionally_excludes(self.sapling_activation_height)
            && self.backfill_coverage.is_some_and(|coverage| {
                coverage.complete_from_height == expected_backfill_floor
                    && coverage.complete_through_height >= self.settled_tip_height
                    && self
                        .latest_indexed_height
                        .is_some_and(|height| height >= self.visible_tip_height)
                    && self.recent_index_complete
            })
    }
}

fn recent_index_is_contiguous(
    materialized_view_store: &MaterializedViewStore,
    complete_through_height: zinder_core::BlockHeight,
    visible_tip_height: zinder_core::BlockHeight,
) -> Result<bool, Status> {
    if complete_through_height >= visible_tip_height {
        return Ok(true);
    }
    let Some(start_height) = complete_through_height.next() else {
        return Ok(false);
    };
    let expected_rows = usize::try_from(
        visible_tip_height
            .value()
            .saturating_sub(start_height.value())
            .saturating_add(1),
    )
    .unwrap_or(usize::MAX);
    if expected_rows > MAX_RECENT_COVERAGE_ROWS {
        return Ok(false);
    }
    let entries = materialized_view_store
        .range_iterate_consumer(
            COMMITMENT_ROOT_SEARCH_INDEX_COLUMN_FAMILY,
            &zinder_core::wire::encode_height_key_ascending(start_height),
            &zinder_core::wire::encode_height_key_ascending(visible_tip_height),
            expected_rows.saturating_add(1),
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    if entries.len() != expected_rows {
        return Ok(false);
    }
    for (offset, (key, _)) in entries.into_iter().enumerate() {
        let height = zinder_core::wire::decode_height_key_ascending(&key)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        let expected_height = start_height
            .value()
            .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        if height.value() != expected_height {
            return Ok(false);
        }
    }
    Ok(true)
}

fn root_matches_protocol(
    roots: BlockFinalNoteCommitmentRoots,
    protocol: CoreProtocol,
    requested_root: FinalNoteCommitmentRoot,
) -> bool {
    match protocol {
        CoreProtocol::Sapling => roots.sapling == Some(requested_root),
        CoreProtocol::Orchard => roots.orchard == Some(requested_root),
        CoreProtocol::Ironwood => roots.ironwood == Some(requested_root),
        _ => false,
    }
}

fn wire_protocol(protocol: CoreProtocol) -> Result<ShieldedProtocol, Status> {
    Ok(match protocol {
        CoreProtocol::Sapling => ShieldedProtocol::Sapling,
        CoreProtocol::Orchard => ShieldedProtocol::Orchard,
        CoreProtocol::Ironwood => ShieldedProtocol::Ironwood,
        _ => {
            return Err(ExplorerError::internal(
                "commitment-root materialized view contains an unsupported shielded protocol",
            )
            .into());
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zinder_core::{BlockHash, BlockId, CanonicalHistoryBoundsError};

    #[test]
    fn unadvertised_displaced_results_skip_storage_and_remain_absent()
    -> Result<(), Box<dyn std::error::Error>> {
        let storage_read = std::cell::Cell::new(false);
        let (matches, coverage) = read_admitted_displaced_root_results(false, || {
            storage_read.set(true);
            Ok((Vec::new(), CommitmentRootSearchDisplacedCoverage::default()))
        })?;

        assert!(matches.is_empty());
        assert_eq!(coverage, None);
        assert!(!storage_read.get());
        Ok(())
    }

    #[test]
    fn checkpoint_before_sapling_preserves_complete_commitment_root_domain()
    -> Result<(), CanonicalHistoryBoundsError> {
        let bounds = CanonicalHistoryBounds::checkpointed(BlockId::new(
            BlockHeight::new(99),
            BlockHash::from_bytes([1; 32]),
        ))?;

        assert!(
            CommitmentRootCompleteness {
                canonical_history_bounds: bounds,
                sapling_activation_height: BlockHeight::new(100),
                backfill_coverage: Some(CommitmentRootBackfillCoverage::new(
                    BlockHeight::new(100),
                    BlockHeight::new(699),
                )),
                latest_indexed_height: Some(BlockHeight::new(700)),
                settled_tip_height: BlockHeight::new(699),
                visible_tip_height: BlockHeight::new(700),
                recent_index_complete: true,
            }
            .is_complete()
        );
        Ok(())
    }

    #[test]
    fn checkpoint_after_sapling_cannot_claim_complete_commitment_root_domain()
    -> Result<(), CanonicalHistoryBoundsError> {
        let bounds = CanonicalHistoryBounds::checkpointed(BlockId::new(
            BlockHeight::new(500),
            BlockHash::from_bytes([2; 32]),
        ))?;

        assert!(
            !CommitmentRootCompleteness {
                canonical_history_bounds: bounds,
                sapling_activation_height: BlockHeight::new(100),
                backfill_coverage: Some(CommitmentRootBackfillCoverage::new(
                    BlockHeight::new(501),
                    BlockHeight::new(699),
                )),
                latest_indexed_height: Some(BlockHeight::new(700)),
                settled_tip_height: BlockHeight::new(699),
                visible_tip_height: BlockHeight::new(700),
                recent_index_complete: true,
            }
            .is_complete()
        );
        Ok(())
    }
}
