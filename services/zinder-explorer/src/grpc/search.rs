//! `ExplorerQuery.Search` handler.
//!
//! Routes a raw user input string through the pure-input classifier in
//! [`zinder_core::explorer_search`], confirms non-shielded candidates
//! against the upstream wallet plane, and wraps the typed candidates in
//! the cross-cutting [`ExplorerFreshness`] envelope per
//! [ADR-0011](../../../docs/adrs/0011-explorer-freshness-envelope.md).
//!
//! The handler enforces the privacy invariant from
//! [ADR-0012](../../../docs/adrs/0012-typed-explorer-search-and-privacy-refusal.md):
//! shielded receivers, viewing keys, and shielded receivers inside a
//! unified address never reach a `WalletQuery` call. The wallet client
//! is only consulted to disambiguate `HashCandidate` arms and confirm
//! `Block { height }` candidates against the live tip.

use tonic::{Request, Response, Status};
use zinder_core::explorer_reasons::{
    SHIELDED_RECEIVER_IN_UNIFIED, SHIELDED_RECEIVER_NO_HISTORY, TEX_TRANSPARENT_SOURCE_ONLY,
    UNIFIED_RECEIVER_UNKNOWN_TYPECODE, VIEWING_KEY_NEVER_INDEXED,
};
use zinder_core::explorer_search::{
    SearchClassification, ShieldedReceiverKind, TexAddressClassification,
    TransparentAddressClassification, UnifiedAddressClassification,
    UnifiedAddressReceiverClassification, classify_search_input,
};
use zinder_core::{Network, wire::encode_zinder_native_chain_name};
use zinder_proto::capabilities::EXPLORER_SEARCH_V1;
use zinder_proto::v1::explorer::{
    BlockMatch, ExplorerFreshness, NotPubliclyIndexable, NotPubliclyIndexableReason,
    SearchCandidate, SearchRequest, SearchResponse, ShieldedAddressMatch, TexAddressMatch,
    TransactionMatch, TransparentAddressMatch, UnclassifiedMatch, UnifiedAddressMatch,
    UnifiedAddressReceiver, UnifiedAddressReceiverKind, ViewingKeyMatch, search_candidate,
    unified_address_receiver,
};
use zinder_proto::v1::wallet::{
    self, BlockSelector, LatestBlockRequest, block_selector, transaction_status_response,
    wallet_query_client::WalletQueryClient,
};
use zinder_runtime::AuthenticatedChannel;

use crate::store::DeriveStore;

const CONFIDENCE_HIGH: f32 = 1.0;
const CONFIDENCE_AMBIGUOUS: f32 = 0.5;

/// Executes one `ExplorerQuery.Search` request.
pub(crate) async fn handle_search(
    derive_store: Option<&DeriveStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: Network,
    request: Request<SearchRequest>,
) -> Result<Response<SearchResponse>, Status> {
    let inner = request.into_inner();
    let classifications = classify_search_input(&inner.query, network);
    let mut candidates = Vec::with_capacity(classifications.len());
    for classification in classifications {
        candidates.extend(project_classification(wallet_client, network, classification).await?);
    }
    let freshness = build_freshness(derive_store, wallet_client).await?;
    Ok(Response::new(SearchResponse {
        freshness: Some(freshness),
        candidates,
    }))
}

async fn project_classification(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    network: Network,
    classification: SearchClassification,
) -> Result<Vec<SearchCandidate>, Status> {
    match classification {
        SearchClassification::Block { height } => probe_block_height(wallet_client, height).await,
        SearchClassification::HashCandidate { bytes } => {
            probe_hash_candidate(wallet_client, bytes).await
        }
        SearchClassification::TransparentAddress(classification) => Ok(vec![candidate_with_match(
            search_candidate::Match::TransparentAddress(transparent_match(&classification)),
            CONFIDENCE_HIGH,
        )]),
        SearchClassification::TexAddress(classification) => Ok(vec![candidate_with_match(
            search_candidate::Match::TexAddress(tex_match(&classification)),
            CONFIDENCE_HIGH,
        )]),
        SearchClassification::UnifiedAddress(classification) => Ok(vec![candidate_with_match(
            search_candidate::Match::UnifiedAddress(unified_match(network, classification)),
            CONFIDENCE_HIGH,
        )]),
        SearchClassification::ShieldedAddress { canonical } => Ok(vec![candidate_with_match(
            search_candidate::Match::ShieldedAddress(ShieldedAddressMatch {
                not_publicly_indexable: Some(NotPubliclyIndexable {
                    reason: NotPubliclyIndexableReason::ShieldedAddress as i32,
                    human_reason: SHIELDED_RECEIVER_NO_HISTORY.to_owned(),
                    canonical_form: Some(canonical),
                }),
            }),
            CONFIDENCE_HIGH,
        )]),
        SearchClassification::ViewingKey => Ok(vec![candidate_with_match(
            search_candidate::Match::ViewingKey(ViewingKeyMatch {
                not_publicly_indexable: Some(NotPubliclyIndexable {
                    reason: NotPubliclyIndexableReason::ViewingKey as i32,
                    human_reason: VIEWING_KEY_NEVER_INDEXED.to_owned(),
                    canonical_form: None,
                }),
            }),
            CONFIDENCE_HIGH,
        )]),
        SearchClassification::Unclassified { hint } => Ok(vec![candidate_with_match(
            search_candidate::Match::Unclassified(UnclassifiedMatch { hint }),
            CONFIDENCE_HIGH,
        )]),
    }
}

async fn probe_block_height(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    height: u32,
) -> Result<Vec<SearchCandidate>, Status> {
    let response = wallet_client
        .block_id_by_selector(Request::new(wallet::BlockIdBySelectorRequest {
            selector: Some(BlockSelector {
                selector: Some(block_selector::Selector::Height(height)),
            }),
            at_epoch: None,
        }))
        .await;
    match response {
        Ok(envelope) => match envelope.into_inner().block_id {
            Some(block_id) => Ok(vec![candidate_with_match(
                search_candidate::Match::Block(BlockMatch {
                    block_height: block_id.height,
                    block_hash: block_id.block_hash,
                }),
                CONFIDENCE_HIGH,
            )]),
            None => Ok(Vec::new()),
        },
        Err(status) if status.code() == tonic::Code::NotFound => Ok(Vec::new()),
        Err(status) => Err(status),
    }
}

async fn probe_hash_candidate(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    hash: [u8; 32],
) -> Result<Vec<SearchCandidate>, Status> {
    let mut candidates = Vec::new();
    if let Some(block_id) = resolve_block_by_hash(wallet_client, hash).await? {
        candidates.push(candidate_with_match(
            search_candidate::Match::Block(BlockMatch {
                block_height: block_id.height,
                block_hash: block_id.block_hash,
            }),
            CONFIDENCE_AMBIGUOUS,
        ));
    }
    if let Some(transaction_match) = resolve_transaction_by_hash(wallet_client, hash).await? {
        candidates.push(candidate_with_match(
            search_candidate::Match::Transaction(transaction_match),
            CONFIDENCE_AMBIGUOUS,
        ));
    }
    if candidates.len() == 1
        && let Some(only) = candidates.first_mut()
    {
        only.confidence = CONFIDENCE_HIGH;
    }
    Ok(candidates)
}

async fn resolve_block_by_hash(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    hash: [u8; 32],
) -> Result<Option<wallet::BlockMetadata>, Status> {
    match wallet_client
        .block_id_by_selector(Request::new(wallet::BlockIdBySelectorRequest {
            selector: Some(BlockSelector {
                selector: Some(block_selector::Selector::Hash(hash.to_vec())),
            }),
            at_epoch: None,
        }))
        .await
    {
        Ok(response) => Ok(response.into_inner().block_id),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(status) => Err(status),
    }
}

async fn resolve_transaction_by_hash(
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    hash: [u8; 32],
) -> Result<Option<TransactionMatch>, Status> {
    let response = wallet_client
        .transaction(Request::new(wallet::TransactionRequest {
            transaction_id: hash.to_vec(),
            at_epoch: None,
        }))
        .await;
    match response {
        Ok(envelope) => Ok(envelope
            .into_inner()
            .status
            .and_then(|status| build_transaction_match(hash, status))),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(status) => Err(status),
    }
}

fn build_transaction_match(
    hash: [u8; 32],
    status: transaction_status_response::Status,
) -> Option<TransactionMatch> {
    match status {
        transaction_status_response::Status::Mined(mined) => {
            let transaction = mined.transaction?;
            Some(TransactionMatch {
                transaction_id: hash.to_vec(),
                in_mempool: false,
                mined_block_height: transaction.block_height,
                mined_block_hash: transaction.block_hash,
            })
        }
        transaction_status_response::Status::InMempool(_) => Some(TransactionMatch {
            transaction_id: hash.to_vec(),
            in_mempool: true,
            mined_block_height: 0,
            mined_block_hash: Vec::new(),
        }),
        transaction_status_response::Status::Conflicting(_) => None,
    }
}

fn transparent_match(classification: &TransparentAddressClassification) -> TransparentAddressMatch {
    TransparentAddressMatch {
        canonical_form: classification.canonical_form.clone(),
        address_script_hash: classification.address_script_hash.as_bytes().to_vec(),
        is_p2pkh: classification.is_p2pkh,
    }
}

fn tex_match(classification: &TexAddressClassification) -> TexAddressMatch {
    TexAddressMatch {
        canonical_tex_form: classification.canonical_tex_form.clone(),
        equivalent_p2pkh_form: classification.equivalent_p2pkh_form.clone(),
        transparent: Some(transparent_match(&classification.transparent)),
        transparent_source_only: true,
        spend_side_note: TEX_TRANSPARENT_SOURCE_ONLY.to_owned(),
    }
}

fn unified_match(
    network: Network,
    classification: UnifiedAddressClassification,
) -> UnifiedAddressMatch {
    UnifiedAddressMatch {
        canonical_form: classification.canonical_form,
        network: encode_zinder_native_chain_name(network).to_owned(),
        receivers: classification
            .receivers
            .into_iter()
            .map(unified_receiver)
            .collect(),
    }
}

fn unified_receiver(receiver: UnifiedAddressReceiverClassification) -> UnifiedAddressReceiver {
    match receiver {
        UnifiedAddressReceiverClassification::Transparent(classification) => {
            let kind = if classification.is_p2pkh {
                UnifiedAddressReceiverKind::P2pkh
            } else {
                UnifiedAddressReceiverKind::P2sh
            };
            UnifiedAddressReceiver {
                kind: kind as i32,
                body: Some(unified_address_receiver::Body::Transparent(
                    transparent_match(&classification),
                )),
            }
        }
        UnifiedAddressReceiverClassification::Shielded { kind } => {
            let receiver_kind = match kind {
                ShieldedReceiverKind::Sapling => UnifiedAddressReceiverKind::Sapling,
                ShieldedReceiverKind::Orchard => UnifiedAddressReceiverKind::Orchard,
            };
            UnifiedAddressReceiver {
                kind: receiver_kind as i32,
                body: Some(unified_address_receiver::Body::Shielded(
                    NotPubliclyIndexable {
                        reason: NotPubliclyIndexableReason::ShieldedReceiverInUnified as i32,
                        human_reason: SHIELDED_RECEIVER_IN_UNIFIED.to_owned(),
                        canonical_form: None,
                    },
                )),
            }
        }
        UnifiedAddressReceiverClassification::Unknown { typecode } => UnifiedAddressReceiver {
            kind: UnifiedAddressReceiverKind::Unknown as i32,
            body: Some(unified_address_receiver::Body::Shielded(
                NotPubliclyIndexable {
                    reason: NotPubliclyIndexableReason::ShieldedReceiverInUnified as i32,
                    human_reason: format!(
                        "{UNIFIED_RECEIVER_UNKNOWN_TYPECODE} (typecode 0x{typecode:02x})"
                    ),
                    canonical_form: None,
                },
            )),
        },
    }
}

const fn candidate_with_match(
    match_arm: search_candidate::Match,
    confidence: f32,
) -> SearchCandidate {
    SearchCandidate {
        r#match: Some(match_arm),
        confidence,
    }
}

async fn build_freshness(
    derive_store: Option<&DeriveStore>,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
) -> Result<ExplorerFreshness, Status> {
    let latest = wallet_client
        .latest_block(Request::new(LatestBlockRequest { at_epoch: None }))
        .await?
        .into_inner();
    let chain_epoch = latest
        .chain_epoch
        .ok_or_else(|| Status::internal("LatestBlockResponse.chain_epoch missing"))?;
    let canonical_tip = latest
        .latest_block
        .ok_or_else(|| Status::internal("LatestBlockResponse.latest_block missing"))?
        .height;
    let derive_cursor_lag_blocks = derive_store
        .and_then(|store| highest_block_summary_height(store).ok().flatten())
        .map_or(0, |materialized| {
            u64::from(canonical_tip.saturating_sub(materialized))
        });
    Ok(ExplorerFreshness {
        chain_epoch: Some(chain_epoch),
        snapshot_age_millis: 0,
        derive_cursor_lag_blocks,
        derive_cursor_lag_millis: 0,
        capability_version: EXPLORER_SEARCH_V1.to_owned(),
        unavailable: Vec::new(),
    })
}

fn highest_block_summary_height(derive_store: &DeriveStore) -> Result<Option<u32>, Status> {
    use crate::consumer::block_summary::BLOCK_SUMMARY_COLUMN_FAMILY;
    let highest = derive_store
        .last_consumer_key(BLOCK_SUMMARY_COLUMN_FAMILY)
        .map_err(|error| Status::internal(error.to_string()))?
        .and_then(|key| <[u8; 4]>::try_from(key.as_slice()).ok())
        .map(u32::from_be_bytes);
    Ok(highest)
}
