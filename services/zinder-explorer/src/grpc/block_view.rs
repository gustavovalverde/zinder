//! `ExplorerQuery` block-view handlers.
//!
//! Both reads project the materialized `BlockSummaryRecord` payloads written
//! by [`zinder_derive::BlockSummaryConsumer`] into the
//! public wire shapes. The handlers wrap reads in the cross-cutting
//! [`ExplorerFreshness`] envelope per
//! [ADR-0011](../../../docs/adrs/0011-explorer-freshness-envelope.md) and
//! compute `derive_cursor_lag_blocks` against the wallet plane's visible
//! tip.

use std::collections::{HashMap, HashSet};

use prost::Message as _;
use tonic::{Request, Response, Status};
use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpochId, TransactionFactsArtifact, TransactionId,
    wire::{
        decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex, encode_height_key_ascending,
        encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex,
    },
};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_PRODUCTION_SERIES_V2, EXPLORER_BLOCK_SUMMARY_V1,
    EXPLORER_BLOCK_TRANSACTIONS_V2,
};
use zinder_proto::v1::explorer::{
    BlockDetailRequest, BlockDetailResponse, BlockFinalNoteCommitmentRoots, BlockProductionPoint,
    BlockProductionSeriesRequest, BlockProductionSeriesResponse, BlockSummariesInRangeRequest,
    BlockSummariesInRangeResponse, BlockSummary, BlockSummaryRecord, BlockTransaction,
    BlockTransactionsResponse, CoinbaseTransactionSummary, block_detail_request,
};
use zinder_proto::v1::wallet::{
    self, BlockSelector, LatestBlockRequest, block_selector, wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, attach_upstream_observation, build_explorer_freshness,
};
use super::require_matching_chain_epoch;
use super::transaction_detail::encode_public_facts;
use super::transparent_input::{encode_mined_transparent_inputs, parent_transaction_ids};
use zinder_derive::{BLOCK_SUMMARY_COLUMN_FAMILY, DeriveStore};
use zinder_store::{
    ChainEpochReader, SecondaryChainStore, chain_epoch_from_message, chain_epoch_message,
    status_from_store_error,
};

/// Hard cap on the number of block summaries one range request returns.
///
/// The wire response is a single repeated field; a multi-million-row request
/// would blow up the gRPC buffer. The cap mirrors the bounded-page rule used
/// across other explorer reads.
const MAX_BLOCK_SUMMARIES_PER_REQUEST: u32 = 1024;

struct MaterializedBlockView {
    summary: zinder_proto::v1::explorer::BlockSummary,
    transaction_ids: Vec<String>,
    chain_epoch: wallet::ChainEpoch,
}

pub(crate) async fn handle_block_summaries_in_range(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockSummariesInRangeRequest>,
) -> Result<Response<BlockSummariesInRangeResponse>, Status> {
    let inner = request.into_inner();
    let start_height = inner.start_height;
    let end_height = inner.end_height;
    validate_block_view_range(start_height, end_height)?;

    let (chain_epoch, canonical_tip) = read_canonical_tip(wallet_client).await?;
    let mut summaries = read_materialized_block_summaries(derive_store, start_height, end_height)?;
    for summary in &mut summaries {
        annotate_request_time_fields(summary, canonical_tip);
    }

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_BLOCK_SUMMARY_V1,
            Some(chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockSummariesInRangeResponse {
        freshness: Some(freshness),
        summaries,
    }))
}

pub(crate) async fn handle_block_production_series(
    derive_store: &DeriveStore,
    chain_store: &SecondaryChainStore,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockProductionSeriesRequest>,
) -> Result<Response<BlockProductionSeriesResponse>, Status> {
    let request = request.into_inner();
    let requested_block_count =
        validate_block_view_range(request.start_height, request.end_height)?;
    let records =
        read_materialized_block_records(derive_store, request.start_height, request.end_height)?;
    let summaries = records
        .iter()
        .map(|record| {
            record
                .summary
                .clone()
                .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing").into())
        })
        .collect::<Result<Vec<_>, Status>>()?;

    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let (chain_epoch, points) = {
        let reader = match request.at_epoch_id {
            Some(chain_epoch_id) => chain_store
                .chain_epoch_reader_at(ChainEpochId::new(chain_epoch_id))
                .map_err(|error| status_from_store_error(&error))?,
            None => chain_store
                .current_chain_epoch_reader()
                .map_err(|error| status_from_store_error(&error))?,
        };
        let chain_epoch = reader.chain_epoch();
        let headers = reader
            .block_headers_in_range(BlockHeightRange::inclusive(
                BlockHeight::new(request.start_height),
                BlockHeight::new(request.end_height),
            ))
            .map_err(|error| status_from_store_error(&error))?;
        let mut points = join_block_production_points(
            summaries,
            headers,
            request.start_height,
            chain_epoch.visible_tip_height.value(),
        );
        let coinbase_artifacts = read_coinbase_artifacts(&reader, &records)?;
        attach_coinbase_summaries(&mut points, &records, &coinbase_artifacts)?;
        (chain_epoch, points)
    };
    let covered_block_count = u32::try_from(points.len()).unwrap_or(u32::MAX);
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
            Some(chain_epoch_message(chain_epoch)),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockProductionSeriesResponse {
        freshness: Some(freshness),
        start_height: request.start_height,
        end_height: request.end_height,
        covered_block_count,
        missing_block_count: requested_block_count.saturating_sub(covered_block_count),
        points,
    }))
}

fn validate_block_view_range(start_height: u32, end_height: u32) -> Result<u32, Status> {
    if end_height < start_height {
        return Err(ExplorerError::invalid_request("end_height must be >= start_height").into());
    }
    let span = u64::from(end_height) - u64::from(start_height) + 1;
    if span > u64::from(MAX_BLOCK_SUMMARIES_PER_REQUEST) {
        return Err(ExplorerError::invalid_request(format!(
            "requested span {span} blocks exceeds the per-request cap of \
             {MAX_BLOCK_SUMMARIES_PER_REQUEST}",
        ))
        .into());
    }
    Ok(u32::try_from(span).unwrap_or(MAX_BLOCK_SUMMARIES_PER_REQUEST))
}

fn read_materialized_block_summaries(
    derive_store: &DeriveStore,
    start_height: u32,
    end_height: u32,
) -> Result<Vec<BlockSummary>, Status> {
    read_materialized_block_records(derive_store, start_height, end_height)?
        .into_iter()
        .map(|record| {
            record
                .summary
                .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing").into())
        })
        .collect()
}

fn read_materialized_block_records(
    derive_store: &DeriveStore,
    start_height: u32,
    end_height: u32,
) -> Result<Vec<BlockSummaryRecord>, Status> {
    let start_key = encode_height_key_ascending(BlockHeight::new(start_height));
    let end_key = encode_height_key_ascending(BlockHeight::new(end_height));
    derive_store
        .range_iterate_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &start_key,
            &end_key,
            MAX_BLOCK_SUMMARIES_PER_REQUEST as usize,
        )
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .into_iter()
        .map(|(_, payload)| {
            BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
                ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}")).into()
            })
        })
        .collect()
}

fn read_coinbase_artifacts(
    reader: &ChainEpochReader<'_>,
    records: &[BlockSummaryRecord],
) -> Result<HashMap<TransactionId, Option<TransactionFactsArtifact>>, Status> {
    let mut seen = HashSet::new();
    let mut transaction_ids = Vec::with_capacity(records.len());
    for record in records {
        let Some(transaction_id) = record.transaction_ids.first() else {
            continue;
        };
        let transaction_id = decode_rpc_transaction_id_hex(transaction_id)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if !seen.insert(transaction_id) {
            return Err(ExplorerError::internal(
                "BlockSummaryRecord range contains a duplicate coinbase transaction id",
            )
            .into());
        }
        transaction_ids.push(transaction_id);
    }
    reader
        .transaction_facts_by_ids(&transaction_ids)
        .map_err(|error| status_from_store_error(&error))
}

fn attach_coinbase_summaries(
    points: &mut [BlockProductionPoint],
    records: &[BlockSummaryRecord],
    artifacts: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
) -> Result<(), Status> {
    let mut coinbase_by_height = HashMap::with_capacity(records.len());
    for record in records {
        let summary = record
            .summary
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
        let Some(transaction_id) = record.transaction_ids.first() else {
            continue;
        };
        let transaction_id = decode_rpc_transaction_id_hex(transaction_id)
            .map_err(|error| ExplorerError::internal(error.to_string()))?;
        if coinbase_by_height
            .insert(summary.block_height, transaction_id)
            .is_some()
        {
            return Err(ExplorerError::internal(
                "BlockSummaryRecord range contains a duplicate block height",
            )
            .into());
        }
    }
    for point in points {
        let summary = point
            .summary
            .as_ref()
            .ok_or_else(|| ExplorerError::internal("BlockProductionPoint.summary missing"))?;
        let Some(transaction_id) = coinbase_by_height.get(&summary.block_height) else {
            continue;
        };
        let Some(artifact) = artifacts.get(transaction_id).and_then(Option::as_ref) else {
            continue;
        };
        validate_coinbase_artifact(summary, *transaction_id, artifact)?;
        point.coinbase = Some(CoinbaseTransactionSummary {
            transaction_id: encode_rpc_transaction_id_hex(*transaction_id),
            transparent_outputs: artifact
                .transparent_outputs
                .iter()
                .map(|output| wallet::TransparentOutput {
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key.clone(),
                })
                .collect(),
            has_shielded_outputs: Some(artifact.public_facts.counts.has_shielded_output()),
        });
    }
    Ok(())
}

fn validate_coinbase_artifact(
    summary: &BlockSummary,
    transaction_id: TransactionId,
    artifact: &TransactionFactsArtifact,
) -> Result<(), Status> {
    let expected_block_hash = decode_rpc_block_hash_hex(&summary.block_hash)
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let location = artifact.location;
    if location.transaction_id != transaction_id
        || location.block_height.value() != summary.block_height
        || location.block_hash != expected_block_hash
        || location.tx_index_in_block != 0
        || artifact.public_facts.transaction_id != transaction_id
        || !artifact.public_facts.is_coinbase
    {
        return Err(ExplorerError::internal(
            "canonical coinbase transaction fact does not match its block production point",
        )
        .into());
    }
    Ok(())
}

fn join_block_production_points(
    summaries: Vec<BlockSummary>,
    headers: Vec<Option<zinder_core::BlockHeaderArtifact>>,
    start_height: u32,
    canonical_tip: u32,
) -> Vec<BlockProductionPoint> {
    let mut summaries_by_height: HashMap<u32, BlockSummary> = summaries
        .into_iter()
        .map(|summary| (summary.block_height, summary))
        .collect();
    headers
        .into_iter()
        .enumerate()
        .filter_map(|(offset, header)| {
            let height = start_height.checked_add(u32::try_from(offset).ok()?)?;
            let header = header?;
            let mut summary = summaries_by_height.remove(&height)?;
            if header.height.value() != height
                || summary.block_hash != encode_rpc_block_hash_hex(header.block_hash)
                || summary.block_time_unix_seconds != header.block_time
            {
                return None;
            }
            annotate_request_time_fields(&mut summary, canonical_tip);
            Some(BlockProductionPoint {
                summary: Some(summary),
                bits: header.bits,
                coinbase: None,
            })
        })
        .collect()
}

pub(crate) async fn handle_block_detail(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockDetailRequest>,
) -> Result<Response<BlockDetailResponse>, Status> {
    let inner = request.into_inner();
    let materialized = read_materialized_block_view(derive_store, wallet_client, &inner).await?;
    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_BLOCK_DETAIL_V1,
            Some(materialized.chain_epoch),
            0,
        )?,
    )
    .await;
    Ok(Response::new(BlockDetailResponse {
        freshness: Some(freshness),
        summary: Some(materialized.summary),
        transaction_ids: materialized.transaction_ids,
    }))
}

pub(crate) async fn handle_block_transactions(
    chain_store: &SecondaryChainStore,
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<BlockDetailRequest>,
) -> Result<Response<BlockTransactionsResponse>, Status> {
    let inner = request.into_inner();
    let materialized = read_materialized_block_view(derive_store, wallet_client, &inner).await?;
    let transactions = read_block_transaction_rows(chain_store, derive_store, &materialized)?;
    let final_note_commitment_roots =
        read_block_final_note_commitment_roots(chain_store, &materialized)?;

    let freshness = attach_upstream_observation(
        upstream_observation_cache,
        build_explorer_freshness(
            Some(derive_store),
            EXPLORER_BLOCK_TRANSACTIONS_V2,
            Some(materialized.chain_epoch),
            0,
        )?,
    )
    .await;

    Ok(Response::new(BlockTransactionsResponse {
        freshness: Some(freshness),
        summary: Some(materialized.summary),
        transactions,
        final_note_commitment_roots,
    }))
}

fn read_block_final_note_commitment_roots(
    chain_store: &SecondaryChainStore,
    materialized: &MaterializedBlockView,
) -> Result<Option<BlockFinalNoteCommitmentRoots>, Status> {
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(materialized.chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = chain_store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    require_matching_chain_epoch(core_epoch, reader.chain_epoch())?;
    reader
        .final_note_commitment_roots_at(BlockHeight::new(materialized.summary.block_height))
        .map(|roots| {
            roots.map(|roots| BlockFinalNoteCommitmentRoots {
                sapling: roots.sapling.map(|root| root.as_bytes().to_vec()),
                orchard: roots.orchard.map(|root| root.as_bytes().to_vec()),
                ironwood: roots.ironwood.map(|root| root.as_bytes().to_vec()),
            })
        })
        .map_err(|error| status_from_store_error(&error))
}

fn read_block_transaction_rows(
    chain_store: &SecondaryChainStore,
    derive_store: &DeriveStore,
    materialized: &MaterializedBlockView,
) -> Result<Vec<BlockTransaction>, Status> {
    chain_store
        .try_catch_up()
        .map_err(|error| status_from_store_error(&error))?;
    let core_epoch = chain_epoch_from_message(materialized.chain_epoch.clone())
        .map_err(|error| ExplorerError::internal(error.to_string()))?;
    let reader = chain_store
        .chain_epoch_reader_at(core_epoch.id)
        .map_err(|error| status_from_store_error(&error))?;
    require_matching_chain_epoch(core_epoch, reader.chain_epoch())?;
    let transaction_ids = materialized
        .transaction_ids
        .iter()
        .map(|transaction_id| {
            decode_rpc_transaction_id_hex(transaction_id)
                .map_err(|error| ExplorerError::internal(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let artifacts_by_id = reader
        .transaction_facts_by_ids(&transaction_ids)
        .map_err(|error| status_from_store_error(&error))?;
    let parent_ids = parent_transaction_ids(artifacts_by_id.values().flatten());
    let parent_transactions = reader
        .transaction_facts_by_ids(&parent_ids)
        .map_err(|error| status_from_store_error(&error))?;
    let fee_lookup_targets = artifacts_by_id
        .iter()
        .filter_map(|(transaction_id, artifact)| {
            artifact
                .as_ref()
                .filter(|artifact| !artifact.public_facts.is_coinbase)
                .map(|artifact| (*transaction_id, artifact.public_facts.privacy_shape))
        })
        .collect::<Vec<_>>();
    let fee_records = zinder_derive::TransactionFeesConsumer::read_fees_records_many(
        derive_store,
        &fee_lookup_targets,
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?;
    encode_block_transaction_rows(
        materialized,
        transaction_ids,
        &artifacts_by_id,
        &parent_transactions,
        &fee_records,
    )
}

fn encode_block_transaction_rows(
    materialized: &MaterializedBlockView,
    transaction_ids: Vec<TransactionId>,
    artifacts_by_id: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    parent_transactions: &HashMap<TransactionId, Option<TransactionFactsArtifact>>,
    fee_records: &HashMap<TransactionId, zinder_proto::v1::explorer::TransactionFeesRecord>,
) -> Result<Vec<BlockTransaction>, Status> {
    let mut transactions = Vec::with_capacity(materialized.transaction_ids.len());

    for (index, (transaction_id, core_transaction_id)) in materialized
        .transaction_ids
        .iter()
        .zip(transaction_ids)
        .enumerate()
    {
        let transaction_index = u32::try_from(index)
            .map_err(|_| ExplorerError::internal("block transaction index exceeds u32"))?;
        let artifact = artifacts_by_id
            .get(&core_transaction_id)
            .and_then(Option::as_ref);
        let public_facts = artifact.map(|artifact| encode_public_facts(&artifact.public_facts));
        let transparent_outputs = artifact.map_or_else(Vec::new, |artifact| {
            artifact
                .transparent_outputs
                .iter()
                .map(|output| wallet::TransparentOutput {
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key.clone(),
                })
                .collect()
        });
        let transparent_inputs = artifact.map_or_else(Vec::new, |artifact| {
            encode_mined_transparent_inputs(
                artifact,
                parent_transactions,
                fee_records.get(&core_transaction_id),
            )
        });
        transactions.push(BlockTransaction {
            transaction_index,
            transaction_id: transaction_id.clone(),
            public_facts,
            transparent_outputs,
            transparent_inputs,
        });
    }

    Ok(transactions)
}

async fn read_materialized_block_view(
    derive_store: &DeriveStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: &BlockDetailRequest,
) -> Result<MaterializedBlockView, Status> {
    let height = resolve_block_height(wallet_client, request).await?;
    let key = encode_height_key_ascending(BlockHeight::new(height));
    let payload = derive_store
        .get_consumer(BLOCK_SUMMARY_COLUMN_FAMILY, &key)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(format!(
                "BlockSummary is not materialized for height {height}"
            ))
        })?;
    let record = BlockSummaryRecord::decode(payload.as_slice()).map_err(|error| {
        ExplorerError::internal(format!("BlockSummaryRecord decode failed: {error}"))
    })?;
    let mut summary = record
        .summary
        .ok_or_else(|| ExplorerError::internal("BlockSummaryRecord.summary missing"))?;
    let (chain_epoch, canonical_tip) = read_canonical_tip(wallet_client).await?;
    annotate_request_time_fields(&mut summary, canonical_tip);

    Ok(MaterializedBlockView {
        summary,
        transaction_ids: record.transaction_ids,
        chain_epoch,
    })
}

fn annotate_request_time_fields(
    summary: &mut zinder_proto::v1::explorer::BlockSummary,
    canonical_tip: u32,
) {
    summary.confirmations = canonical_tip
        .saturating_sub(summary.block_height)
        .saturating_add(1);
    summary.is_canonical = summary.block_height <= canonical_tip;
}

async fn resolve_block_height(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    request: &BlockDetailRequest,
) -> Result<u32, Status> {
    match request
        .selector
        .as_ref()
        .ok_or_else(|| ExplorerError::invalid_request("BlockDetailRequest.selector is required"))?
    {
        block_detail_request::Selector::BlockHeight(height) => Ok(*height),
        block_detail_request::Selector::BlockHash(hash) => {
            let selector = BlockSelector {
                selector: Some(block_selector::Selector::Hash(hash.clone())),
            };
            let response = wallet_client
                .block_id_by_selector(Request::new(wallet::BlockSelectorRequest {
                    selector: Some(selector),
                    at_epoch_id: request.at_epoch_id,
                }))
                .await?
                .into_inner();
            let block_id = response
                .block_id
                .ok_or_else(|| ExplorerError::internal("BlockIdResponse.block_id missing"))?;
            Ok(block_id.height)
        }
    }
}

async fn read_canonical_tip(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<(wallet::ChainEpoch, u32), Status> {
    let latest = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch_id: None }))
        .await?
        .into_inner();
    let chain_epoch = latest
        .chain_view
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| {
            ExplorerError::internal("LatestBlockResponse.chain_view.chain_epoch missing")
        })?;
    let canonical_tip = latest
        .latest_block
        .ok_or_else(|| ExplorerError::internal("LatestBlockResponse.latest_block missing"))?
        .height;
    Ok((chain_epoch, canonical_tip))
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use std::collections::HashMap;

    use super::{attach_coinbase_summaries, join_block_production_points};
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, TransactionFactsArtifact, TransactionId,
        TransactionLocation, TransactionVersion, TransparentAddressScriptHash,
        TransparentOutputFact,
        wire::{encode_rpc_block_hash_hex, encode_rpc_transaction_id_hex},
    };
    use zinder_proto::v1::explorer::{BlockProductionPoint, BlockSummary, BlockSummaryRecord};
    use zinder_testkit::synthetic_transaction_public_facts;

    #[test]
    fn block_production_attaches_validated_coinbase_outputs() -> eyre::Result<()> {
        let block_hash = BlockHash::from_bytes([1; 32]);
        let transaction_id = TransactionId::from_bytes([2; 32]);
        let summary = BlockSummary {
            block_height: 10,
            block_hash: encode_rpc_block_hash_hex(block_hash),
            ..Default::default()
        };
        let record = BlockSummaryRecord {
            summary: Some(summary.clone()),
            transaction_ids: vec![encode_rpc_transaction_id_hex(transaction_id)],
            ..Default::default()
        };
        let mut public_facts = synthetic_transaction_public_facts(transaction_id, 64);
        public_facts.is_coinbase = true;
        public_facts.version = TransactionVersion::V5;
        public_facts.unsupported_sections.clear();
        public_facts.counts.transparent_output_count = 1;
        let script_pub_key = vec![0x51];
        let artifact = TransactionFactsArtifact::new(
            TransactionLocation::new(transaction_id, BlockHeight::new(10), block_hash, 0),
            public_facts,
        )
        .with_transparent_facts(
            Vec::new(),
            vec![TransparentOutputFact::new(
                0,
                137_500_000,
                script_pub_key.clone(),
                TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
            )],
        );
        let artifacts = HashMap::from([(transaction_id, Some(artifact))]);
        let mut points = vec![BlockProductionPoint {
            summary: Some(summary),
            bits: 0,
            coinbase: None,
        }];

        attach_coinbase_summaries(&mut points, std::slice::from_ref(&record), &artifacts)?;

        let coinbase = points[0]
            .coinbase
            .as_ref()
            .ok_or_else(|| eyre::eyre!("coinbase summary missing"))?;
        assert_eq!(
            coinbase.transaction_id,
            encode_rpc_transaction_id_hex(transaction_id)
        );
        assert_eq!(coinbase.transparent_outputs.len(), 1);
        assert_eq!(coinbase.transparent_outputs[0].value_zat, 137_500_000);
        assert_eq!(coinbase.has_shielded_outputs, Some(false));

        points[0].coinbase = None;
        let unavailable_artifacts = HashMap::from([(transaction_id, None)]);
        attach_coinbase_summaries(&mut points, &[record], &unavailable_artifacts)?;
        assert!(points[0].coinbase.is_none());
        Ok(())
    }

    #[test]
    fn block_production_join_omits_mixed_epoch_rows() {
        let matching_hash = BlockHash::from_bytes([1; 32]);
        let mismatched_hash = BlockHash::from_bytes([2; 32]);
        let summaries = vec![
            BlockSummary {
                block_height: 10,
                block_hash: encode_rpc_block_hash_hex(matching_hash),
                block_time_unix_seconds: 1_000,
                ..Default::default()
            },
            BlockSummary {
                block_height: 11,
                block_hash: encode_rpc_block_hash_hex(mismatched_hash),
                block_time_unix_seconds: 1_075,
                ..Default::default()
            },
        ];
        let headers = vec![
            Some(block_header(10, matching_hash, 1_000, 0x1f34_bb90)),
            Some(block_header(11, matching_hash, 1_075, 0x1f34_bb90)),
        ];

        let points = join_block_production_points(summaries, headers, 10, 11);

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].bits, 0x1f34_bb90);
        assert_eq!(
            points[0]
                .summary
                .as_ref()
                .map(|summary| summary.block_height),
            Some(10)
        );
    }

    fn block_header(
        height: u32,
        block_hash: BlockHash,
        block_time_unix_seconds: i64,
        bits: u32,
    ) -> BlockHeaderArtifact {
        BlockHeaderArtifact::new(
            BlockHeight::new(height),
            block_hash,
            BlockHash::from_bytes([0; 32]),
            [0; 32],
            [0; 32],
            block_time_unix_seconds,
            bits,
            [0; 32],
            0,
            0,
        )
    }
}
