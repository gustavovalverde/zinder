#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;
use std::{net::SocketAddr, num::NonZeroU32, pin::Pin};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::{Stream, StreamExt as _, wrappers::TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status, transport::Server};
use tonic_types::StatusExt;
use zinder_core::wire::encode_rpc_transaction_id_hex;
use zinder_core::{
    ChainEpoch, ChainTipMetadata, CompactBlockArtifact, MAX_SUBTREE_ROOTS_PER_REQUEST,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentOutputArtifact,
    TreeStateArtifact, UnixTimestampMillis,
};
use zinder_proto::capabilities::{WALLET_BROADCAST_TRANSACTION_V1, WALLET_EVENTS_CHAIN_V1};
use zinder_proto::v1::{
    ingest::{
        MempoolTransactionRequest, WriterPhase, WriterStatusRequest, WriterStatusResponse,
        ingest_control_server::{IngestControl, IngestControlServer},
    },
    wallet::{self, wallet_query_server::WalletQuery as WalletQueryService},
};
use zinder_query::{
    WalletEndpointMetadata, WalletQuery, WalletQueryGrpcAdapter, WalletQueryOptions,
};
use zinder_store::{
    ChainEpochArtifacts, EventStreamStartPosition, PrimaryChainStore, StreamCursorTokenV1,
    chain_view_message, event_stream_start_message,
};
use zinder_testkit::{
    MockTransactionBroadcaster, StoreFixture, encode_fixture_block_replay,
    sample_regtest_upgrade_activations,
};

use crate::common::{
    chain_epoch_artifacts_with_sapling_outputs, chain_epoch_artifacts_with_transparent_facts,
    synthetic_chain_epoch,
};

#[tokio::test]
async fn native_grpc_service_returns_wallet_reads_from_stored_artifacts() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_artifacts = commit_wallet_artifacts(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());
    let grpc_responses = read_wallet_grpc_responses(&grpc_adapter).await?;

    assert_wallet_grpc_response_epochs(&grpc_responses, stored_artifacts.chain_epoch.id.value());
    assert_eq!(
        grpc_responses
            .visible_tip_block
            .visible_tip_block
            .ok_or_else(|| eyre!("missing latest block"))?
            .height,
        1
    );
    assert_eq!(
        grpc_responses
            .compact_block_range
            .first()
            .ok_or_else(|| eyre!("missing compact block"))?
            .compact_block
            .as_ref()
            .ok_or_else(|| eyre!("missing compact block"))?,
        &zinder_proto::wire::compact_block_message(&stored_artifacts.compact_block)
    );
    assert_eq!(
        grpc_responses.latest_tree_state_checkpoint.payload_bytes,
        stored_artifacts.tree_state.payload_bytes
    );
    assert_eq!(
        grpc_responses
            .subtree_roots
            .subtree_roots
            .first()
            .ok_or_else(|| eyre!("missing subtree root"))?
            .root_hash,
        stored_artifacts.subtree_root.root_hash.as_bytes()
    );
    assert!(
        grpc_responses
            .network_upgrade_activations
            .activations
            .iter()
            .any(|activation| activation.name == "NU6.2")
    );

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_checks_range_limit_before_opening_reader() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let wallet_query = WalletQuery::with_options(
        store,
        (),
        Arc::new(sample_regtest_upgrade_activations()),
        WalletQueryOptions {
            max_compact_block_range: NonZeroU32::new(1)
                .ok_or_else(|| eyre!("invalid range limit"))?,
            ..WalletQueryOptions::default()
        },
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let status = match WalletQueryService::compact_blocks_in_range(
        &grpc_adapter,
        Request::new(wallet::CompactBlocksInRangeRequest {
            start_height: 1,
            end_height: 2,
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(_response) => return Err(eyre!("expected range error, got success")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_rejects_oversized_subtree_root_range() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());
    let requested = MAX_SUBTREE_ROOTS_PER_REQUEST.saturating_add(1);

    let status = match WalletQueryService::subtree_roots(
        &grpc_adapter,
        Request::new(wallet::SubtreeRootsRequest {
            shielded_protocol: wallet::ShieldedProtocol::Sapling as i32,
            start_index: 0,
            max_entries: requested,
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(response) => {
            return Err(eyre!("expected subtree-root range error, got {response:?}"));
        }
        Err(status) => status,
    };
    let details = status.get_error_details();
    let reason = details
        .error_info()
        .map(|error_info| error_info.reason.as_str());
    let violation = details
        .bad_request()
        .and_then(|bad_request| bad_request.field_violations.first());

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(reason, Some("SUBTREE_ROOT_RANGE_TOO_LARGE"));
    assert!(matches!(
        violation,
        Some(violation)
            if violation.field == "max_entries"
                && violation.description.contains("maximum is 1024")
    ));
    assert!(status.message().contains("maximum 1024"));
    Ok(())
}

#[tokio::test]
async fn native_grpc_service_maps_missing_artifacts_to_not_found() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0, 0);
    store.commit_chain_epoch(chain_epoch_artifacts_with_sapling_outputs(
        chain_epoch,
        block,
        65_536,
    )?)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let tree_state_status = match WalletQueryService::tree_state_at_height(
        &grpc_adapter,
        Request::new(wallet::TreeStateAtHeightRequest {
            height: 1,
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected tree-state error, got {response:?}")),
        Err(status) => status,
    };
    let subtree_roots_status = match WalletQueryService::subtree_roots(
        &grpc_adapter,
        Request::new(wallet::SubtreeRootsRequest {
            shielded_protocol: wallet::ShieldedProtocol::Sapling as i32,
            start_index: 0,
            max_entries: 1,
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected subtree-root error, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(tree_state_status.code(), Code::FailedPrecondition);
    assert_eq!(subtree_roots_status.code(), Code::NotFound);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_returns_not_found_when_transaction_missing() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_wallet_artifacts(&store)?;
    let requested_transaction_id = TransactionId::from_bytes([0x45; 32]);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let status = match WalletQueryService::transaction(
        &grpc_adapter,
        Request::new(wallet::TransactionRequest {
            transaction_id: encode_rpc_transaction_id_hex(requested_transaction_id),
            at_epoch_id: None,
        }),
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected transaction error, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);
    assert!(
        status.message().contains("not visible"),
        "expected wire message to describe visibility, got {:?}",
        status.message()
    );

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_streams_chain_events_from_the_store() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let stored_artifacts = commit_wallet_artifacts(&store)?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let mut event_stream = WalletQueryService::chain_events(
        &grpc_adapter,
        Request::new(wallet::ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::EarliestRetained,
            )),
            family: wallet::ChainEventStreamFamily::Visible as i32,
            address_filter: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let first_event = event_stream
        .next()
        .await
        .ok_or_else(|| eyre!("chain-events stream closed before first event"))??;

    assert_eq!(first_event.event_sequence, 1);
    assert_eq!(
        first_event
            .chain_view
            .and_then(|chain_view| chain_view.chain_epoch)
            .ok_or_else(|| eyre!("missing chain epoch"))?
            .chain_epoch_id,
        stored_artifacts.chain_epoch.id.value()
    );
    assert!(matches!(
        first_event.event,
        Some(wallet::chain_event_envelope::Event::ChainCommitted(committed))
            if committed.committed.as_ref().is_some_and(|inner| {
                inner.start_height == 1 && inner.end_height == 1
            })
    ));
    assert!(!first_event.cursor.is_empty());

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_rejects_unset_event_stream_start() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_wallet_artifacts(&store)?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    for start in [None, Some(wallet::EventStreamStart { position: None })] {
        let status = match WalletQueryService::chain_events(
            &grpc_adapter,
            Request::new(wallet::ChainEventsRequest {
                start,
                family: wallet::ChainEventStreamFamily::Visible as i32,
                address_filter: Vec::new(),
            }),
        )
        .await
        {
            Ok(_response) => return Err(eyre!("expected unset-start rejection")),
            Err(status) => status,
        };
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_live_tail_delivers_only_post_subscribe_events() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_wallet_artifacts(&store)?;
    let wallet_query = WalletQuery::new(
        store.clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());

    let mut event_stream = WalletQueryService::chain_events(
        &grpc_adapter,
        Request::new(wallet::ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::LiveTail,
            )),
            family: wallet::ChainEventStreamFamily::Visible as i32,
            address_filter: Vec::new(),
        }),
    )
    .await?
    .into_inner();

    let (mut second_epoch, second_block, _second_compact_block) = synthetic_chain_epoch(2, 2);
    second_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0, 0);
    let second_compact_block = zinder_core::CompactBlockArtifact::empty(
        zinder_core::BlockId::new(second_block.height, second_block.block_hash),
        second_block.parent_hash,
        u32::try_from(second_block.block_time)
            .map_err(|_| eyre!("fixture block time is not representable as u32"))?,
        zinder_core::CompactChainMetadata {
            sapling_commitment_tree_size: 65_536,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    );
    let second_replay = encode_fixture_block_replay(&second_block, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_replay],
        vec![second_compact_block],
    ))?;

    let first_event = tokio::time::timeout(std::time::Duration::from_secs(5), event_stream.next())
        .await?
        .ok_or_else(|| eyre!("chain-events stream closed before the post-subscribe event"))??;
    assert_eq!(first_event.event_sequence, 2);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_expires_pruned_chain_event_cursors() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let mut first_cursor = Vec::new();

    for height in 1..=3 {
        let (chain_epoch, block, compact_block) = synthetic_chain_epoch(u64::from(height), height);
        let replay = encode_fixture_block_replay(&block, &[]);
        let commit = store.commit_chain_epoch(ChainEpochArtifacts::new(
            chain_epoch,
            vec![block],
            vec![replay],
            vec![compact_block],
        ))?;
        if height == 1 {
            first_cursor = commit.event_envelope.cursor.as_bytes().to_vec();
        }
    }
    store.prune_chain_events_before(UnixTimestampMillis::new(1_774_668_300_003))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());
    let mut event_stream = WalletQueryService::chain_events(
        &grpc_adapter,
        Request::new(wallet::ChainEventsRequest {
            start: Some(event_stream_start_message(
                &EventStreamStartPosition::AfterCursor(StreamCursorTokenV1::from_bytes(
                    first_cursor,
                )),
            )),
            family: wallet::ChainEventStreamFamily::Visible as i32,
            address_filter: Vec::new(),
        }),
    )
    .await?
    .into_inner();
    let status = match event_stream
        .next()
        .await
        .ok_or_else(|| eyre!("chain-events stream closed before cursor error"))?
    {
        Ok(event) => return Err(eyre!("expected cursor expiry, got event {event:?}")),
        Err(status) => status,
    };
    let details = status.get_error_details();
    let violation = details
        .precondition_failure()
        .and_then(|failure| failure.violations.first())
        .cloned();

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(matches!(
        violation,
        Some(violation)
            if violation.r#type == "CHAIN_EVENT_CURSOR_EXPIRED"
                && violation.subject == "chain_event:1"
                && violation.description.contains('3')
    ));

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_honors_request_epoch_pin() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);
    let first_replay = encode_fixture_block_replay(&first_block, &[]);
    let second_replay = encode_fixture_block_replay(&second_block, &[]);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_replay],
        vec![first_compact_block],
    ))?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_replay],
        vec![second_compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, WalletEndpointMetadata::default());
    let response = WalletQueryService::visible_tip_block(
        &grpc_adapter,
        Request::new(wallet::VisibleTipBlockRequest {
            at_epoch_id: Some(first_epoch.id.value()),
        }),
    )
    .await?
    .into_inner();
    let response_epoch = response
        .chain_view
        .clone()
        .and_then(|chain_view| chain_view.chain_epoch)
        .ok_or_else(|| eyre!("missing response chain epoch"))?;
    let visible_tip_block = response
        .visible_tip_block
        .ok_or_else(|| eyre!("missing latest block"))?;

    assert_eq!(response_epoch.chain_epoch_id, first_epoch.id.value());
    assert_eq!(visible_tip_block.height, 1);

    Ok(())
}

#[tokio::test]
async fn native_grpc_service_resolves_unpinned_mempool_transactions() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x89; 32]);
    let mempool_status = wallet::TransactionStatusResponse {
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(test_chain_epoch()),
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: None,
        }),
        location: Some(wallet::TransactionLocation {
            location: Some(wallet::transaction_location::Location::InMempool(
                wallet::MempoolEntry {
                    transaction_id: encode_rpc_transaction_id_hex(transaction_id),
                    auth_digest: String::new(),
                    raw_transaction_bytes: vec![0x01, 0x02, 0x03],
                    compact_transaction_data: Some(wallet::CompactTransactionData::default()),
                    first_seen_unix_millis: 1_700_000_000_000,
                    first_seen_chain_epoch: Some(test_chain_epoch()),
                    transparent_outputs: Vec::new(),
                    transparent_spends: Vec::new(),
                },
            )),
        }),
    };
    let ingest_control =
        StaticIngestControl::new().with_mempool_transaction_response(mempool_status);
    let (ingest_control_addr, cancel, server_handle) =
        spawn_ingest_control_server(ingest_control).await?;
    let store_fixture = StoreFixture::open()?;
    let stored_artifacts = commit_wallet_artifacts(store_fixture.chain_store())?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        wallet_query,
        WalletEndpointMetadata::default(),
        format!("http://{ingest_control_addr}"),
    );

    let response = WalletQueryService::transaction(
        &grpc_adapter,
        Request::new(wallet::TransactionRequest {
            transaction_id: encode_rpc_transaction_id_hex(transaction_id),
            at_epoch_id: None,
        }),
    )
    .await?
    .into_inner();
    assert!(matches!(
        response.location.and_then(|location| location.location),
        Some(wallet::transaction_location::Location::InMempool(transaction))
            if transaction.raw_transaction_bytes == [0x01, 0x02, 0x03]
                && transaction.first_seen_unix_millis == 1_700_000_000_000
    ));

    let Err(pinned_status) = WalletQueryService::transaction(
        &grpc_adapter,
        Request::new(wallet::TransactionRequest {
            transaction_id: encode_rpc_transaction_id_hex(transaction_id),
            at_epoch_id: Some(stored_artifacts.chain_epoch.id.value()),
        }),
    )
    .await
    else {
        return Err(eyre!(
            "pinned canonical miss unexpectedly read live mempool state"
        ));
    };
    assert_eq!(pinned_status.code(), Code::NotFound);

    cancel.cancel();
    server_handle.await??;
    Ok(())
}

#[tokio::test]
async fn native_grpc_service_combines_confirmed_balance_with_pending_transparent_activity()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let fixture = commit_confirmed_transparent_balance_fixture(&store)?;

    let pending_inflow_zat = 300;
    let ingest_control = configured_mempool_balance_ingest_control(&fixture, pending_inflow_zat);
    let (ingest_control_addr, cancel, server_handle) =
        spawn_ingest_control_server(ingest_control).await?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::with_ingest_control_proxy(
        wallet_query,
        WalletEndpointMetadata::default(),
        format!("http://{ingest_control_addr}"),
    );

    let response = WalletQueryService::transparent_address_balance(
        &grpc_adapter,
        Request::new(wallet::TransparentAddressBalanceRequest {
            addresses: vec![wallet::AddressLookup {
                selector: Some(wallet::address_lookup::Selector::ScriptHash(
                    fixture.address_script_hash.as_bytes().to_vec(),
                )),
            }],
            at_epoch_id: Some(fixture.chain_epoch.id.value()),
        }),
    )
    .await?
    .into_inner();

    assert_eq!(response.confirmed_zat, fixture.confirmed_value_zat);
    assert_eq!(
        response.unconfirmed_delta_zat,
        i64::try_from(pending_inflow_zat)? - i64::try_from(fixture.confirmed_value_zat)?,
        "the live overlay adds pending inflows and subtracts spends of confirmed outputs"
    );
    assert_eq!(response.address_count, 1);
    assert_eq!(
        response
            .chain_view
            .and_then(|chain_view| chain_view.chain_epoch)
            .ok_or_else(|| eyre!("balance response omitted its chain epoch"))?
            .chain_epoch_id,
        fixture.chain_epoch.id.value(),
        "the public balance response remains pinned to the canonical epoch"
    );

    cancel.cancel();
    server_handle.await??;
    Ok(())
}

struct ConfirmedTransparentBalanceFixture {
    chain_epoch: ChainEpoch,
    address_script_hash: TransparentAddressScriptHash,
    confirmed_outpoint: TransparentOutPoint,
    confirmed_value_zat: u64,
}

fn commit_confirmed_transparent_balance_fixture(
    store: &PrimaryChainStore,
) -> eyre::Result<ConfirmedTransparentBalanceFixture> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes([0x42; 32]);
    let confirmed_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x51; 32]), 0);
    let confirmed_value_zat = 10_000;
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let confirmed_output = TransparentOutputArtifact::new(
        confirmed_outpoint,
        confirmed_value_zat,
        vec![0x76, 0xa9, 0x14],
        address_script_hash,
        block.height,
        block.block_hash,
    );
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        chain_epoch,
        vec![block],
        vec![compact_block],
        &[confirmed_output],
        Vec::new(),
    )?)?;

    Ok(ConfirmedTransparentBalanceFixture {
        chain_epoch,
        address_script_hash,
        confirmed_outpoint,
        confirmed_value_zat,
    })
}

fn configured_mempool_balance_ingest_control(
    fixture: &ConfirmedTransparentBalanceFixture,
    pending_inflow_zat: u64,
) -> StaticIngestControl {
    StaticIngestControl::new()
        .with_transparent_mempool_outputs_by_address_response(
            wallet::TransparentMempoolOutputsByAddressResponse {
                chain_view: Some(chain_view_message(fixture.chain_epoch)),
                outputs: vec![wallet::TransparentMempoolOutput {
                    address_script_hash: fixture.address_script_hash.as_bytes().to_vec(),
                    script_pub_key: vec![0x76, 0xa9, 0x14],
                    outpoint: Some(wallet::OutPoint {
                        transaction_id: encode_rpc_transaction_id_hex(TransactionId::from_bytes(
                            [0x52; 32],
                        )),
                        output_index: 0,
                    }),
                    value_zat: pending_inflow_zat,
                }],
            },
        )
        .with_transparent_mempool_spends_by_outpoint_response(
            wallet::TransparentMempoolSpendsByOutpointResponse {
                chain_view: Some(chain_view_message(fixture.chain_epoch)),
                spends: vec![wallet::TransparentMempoolSpend {
                    spent_outpoint: Some(wallet::OutPoint {
                        transaction_id: encode_rpc_transaction_id_hex(
                            fixture.confirmed_outpoint.transaction_id,
                        ),
                        output_index: fixture.confirmed_outpoint.output_index,
                    }),
                    spending_transaction_id: encode_rpc_transaction_id_hex(
                        TransactionId::from_bytes([0x53; 32]),
                    ),
                }],
            },
        )
}

#[tokio::test]
async fn native_grpc_service_uses_query_owned_capabilities() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let read_only_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let read_only_adapter =
        WalletQueryGrpcAdapter::new(read_only_query, WalletEndpointMetadata::default());
    let read_only_info = WalletQueryService::server_info(
        &read_only_adapter,
        Request::new(wallet::ServerInfoRequest {}),
    )
    .await?
    .into_inner()
    .info
    .ok_or_else(|| eyre!("missing read-only info"))?;

    assert!(has_capability(&read_only_info, WALLET_EVENTS_CHAIN_V1));
    assert!(!has_capability(
        &read_only_info,
        WALLET_BROADCAST_TRANSACTION_V1
    ));

    let broadcaster = MockTransactionBroadcaster::accepted(TransactionId::from_bytes([0x33; 32]));
    let broadcast_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster,
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let broadcast_adapter =
        WalletQueryGrpcAdapter::new(broadcast_query, WalletEndpointMetadata::default());
    let broadcast_info = WalletQueryService::server_info(
        &broadcast_adapter,
        Request::new(wallet::ServerInfoRequest {}),
    )
    .await?
    .into_inner()
    .info
    .ok_or_else(|| eyre!("missing broadcast info"))?;

    assert!(!has_capability(
        &broadcast_info,
        WALLET_BROADCAST_TRANSACTION_V1
    ));

    Ok(())
}

struct StoredWalletArtifacts {
    chain_epoch: ChainEpoch,
    compact_block: CompactBlockArtifact,
    tree_state: TreeStateArtifact,
    subtree_root: SubtreeRootArtifact,
}

struct WalletGrpcResponses {
    visible_tip_block: wallet::VisibleTipBlockResponse,
    compact_block_range: Vec<wallet::CompactBlocksInRangeChunk>,
    latest_tree_state_checkpoint: wallet::TreeStateResponse,
    subtree_roots: wallet::SubtreeRootsResponse,
    network_upgrade_activations: wallet::NetworkUpgradeActivationsResponse,
}

fn commit_wallet_artifacts(store: &PrimaryChainStore) -> eyre::Result<StoredWalletArtifacts> {
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0, 0);
    let artifacts = chain_epoch_artifacts_with_sapling_outputs(chain_epoch, block.clone(), 65_536)?;
    let compact_block = artifacts
        .compact_blocks
        .first()
        .cloned()
        .ok_or_else(|| eyre!("missing compact block fixture"))?;
    let tree_state = TreeStateArtifact::new(
        block.height,
        block.block_hash,
        u32::try_from(block.block_time)?,
        b"tree-state-1".to_vec(),
    );
    let subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x71; 32]),
        block.height,
        block.block_hash,
    );
    store.commit_chain_epoch(
        artifacts
            .with_tree_states(vec![tree_state.clone()])
            .with_subtree_roots(vec![subtree_root.clone()]),
    )?;

    Ok(StoredWalletArtifacts {
        chain_epoch,
        compact_block,
        tree_state,
        subtree_root,
    })
}

async fn read_wallet_grpc_responses(
    grpc_adapter: &WalletQueryGrpcAdapter<WalletQuery<PrimaryChainStore>>,
) -> Result<WalletGrpcResponses, tonic::Status> {
    let visible_tip_block = WalletQueryService::visible_tip_block(
        grpc_adapter,
        Request::new(wallet::VisibleTipBlockRequest { at_epoch_id: None }),
    )
    .await?
    .into_inner();
    let mut compact_block_stream = WalletQueryService::compact_blocks_in_range(
        grpc_adapter,
        Request::new(wallet::CompactBlocksInRangeRequest {
            start_height: 1,
            end_height: 1,
            at_epoch_id: None,
        }),
    )
    .await?
    .into_inner();
    let mut compact_block_range = Vec::new();
    while let Some(compact_block_chunk) = compact_block_stream.next().await {
        compact_block_range.push(compact_block_chunk?);
    }
    let latest_tree_state_checkpoint = WalletQueryService::latest_tree_state_checkpoint(
        grpc_adapter,
        Request::new(wallet::LatestTreeStateCheckpointRequest { at_epoch_id: None }),
    )
    .await?
    .into_inner();
    let subtree_roots = WalletQueryService::subtree_roots(
        grpc_adapter,
        Request::new(wallet::SubtreeRootsRequest {
            shielded_protocol: wallet::ShieldedProtocol::Sapling as i32,
            start_index: 0,
            max_entries: 1,
            at_epoch_id: None,
        }),
    )
    .await?
    .into_inner();
    let network_upgrade_activations = WalletQueryService::network_upgrade_activations(
        grpc_adapter,
        Request::new(wallet::NetworkUpgradeActivationsRequest {}),
    )
    .await?
    .into_inner();

    Ok(WalletGrpcResponses {
        visible_tip_block,
        compact_block_range,
        latest_tree_state_checkpoint,
        subtree_roots,
        network_upgrade_activations,
    })
}

fn assert_wallet_grpc_response_epochs(responses: &WalletGrpcResponses, chain_epoch_id: u64) {
    assert_eq!(
        response_chain_epoch_id(&responses.visible_tip_block),
        chain_epoch_id
    );
    for compact_block_chunk in &responses.compact_block_range {
        assert_eq!(response_chain_epoch_id(compact_block_chunk), chain_epoch_id);
    }
    assert_eq!(
        response_chain_epoch_id(&responses.latest_tree_state_checkpoint),
        chain_epoch_id
    );
    assert_eq!(
        response_chain_epoch_id(&responses.subtree_roots),
        chain_epoch_id
    );
}

trait HasChainEpoch {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch>;
}

impl HasChainEpoch for wallet::VisibleTipBlockResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
    }
}

impl HasChainEpoch for wallet::CompactBlocksInRangeChunk {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
    }
}

impl HasChainEpoch for wallet::TreeStateResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
    }
}

impl HasChainEpoch for wallet::SubtreeRootsResponse {
    fn chain_epoch(&self) -> Option<&wallet::ChainEpoch> {
        self.chain_view
            .as_ref()
            .and_then(|chain_view| chain_view.chain_epoch.as_ref())
    }
}

fn response_chain_epoch_id(response: &impl HasChainEpoch) -> u64 {
    response
        .chain_epoch()
        .map_or(0, |chain_epoch| chain_epoch.chain_epoch_id)
}

fn has_capability(wallet_info: &wallet::WalletServerInfo, capability: &str) -> bool {
    wallet_info.common.as_ref().is_some_and(|common| {
        common
            .capabilities
            .iter()
            .any(|advertised| advertised == capability)
    })
}

type StaticVisibleChainEventsStream =
    Pin<Box<dyn Stream<Item = Result<wallet::ChainEventEnvelope, Status>> + Send>>;

#[derive(Clone)]
struct StaticIngestControl {
    transaction: Option<wallet::TransactionStatusResponse>,
    outputs_by_address: Option<wallet::TransparentMempoolOutputsByAddressResponse>,
    spends_by_outpoint: Option<wallet::TransparentMempoolSpendsByOutpointResponse>,
}

impl StaticIngestControl {
    fn new() -> Self {
        Self {
            transaction: None,
            outputs_by_address: None,
            spends_by_outpoint: None,
        }
    }

    fn with_mempool_transaction_response(
        mut self,
        response: wallet::TransactionStatusResponse,
    ) -> Self {
        self.transaction = Some(response);
        self
    }

    fn with_transparent_mempool_outputs_by_address_response(
        mut self,
        response: wallet::TransparentMempoolOutputsByAddressResponse,
    ) -> Self {
        self.outputs_by_address = Some(response);
        self
    }

    fn with_transparent_mempool_spends_by_outpoint_response(
        mut self,
        response: wallet::TransparentMempoolSpendsByOutpointResponse,
    ) -> Self {
        self.spends_by_outpoint = Some(response);
        self
    }
}

#[tonic::async_trait]
impl IngestControl for StaticIngestControl {
    type VisibleChainEventsStream = StaticVisibleChainEventsStream;
    type MempoolEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<wallet::MempoolEventEnvelope, Status>> + Send>,
    >;

    async fn server_info(
        &self,
        _request: Request<zinder_proto::v1::ingest::ServerInfoRequest>,
    ) -> Result<Response<zinder_proto::v1::ingest::ServerInfoResponse>, Status> {
        Ok(Response::new(
            zinder_proto::v1::ingest::ServerInfoResponse { server_info: None },
        ))
    }

    async fn writer_status(
        &self,
        _request: Request<WriterStatusRequest>,
    ) -> Result<Response<WriterStatusResponse>, Status> {
        Ok(Response::new(WriterStatusResponse {
            chain_view: Some(wallet::ChainView {
                chain_epoch: Some(wallet::ChainEpoch {
                    chain_epoch_id: 11,
                    network_name: "zcash-regtest".to_owned(),
                    artifact_schema_version: 1,
                    created_at_millis: 123,
                    visible_tip: Some(wallet::BlockTip {
                        height: 5,
                        hash: "05".repeat(32),
                    }),
                    settled_tip: Some(wallet::BlockTip {
                        height: 4,
                        hash: "04".repeat(32),
                    }),
                    sapling_commitment_tree_size: 0,
                    orchard_commitment_tree_size: 0,
                    ironwood_commitment_tree_size: 0,
                }),
                indexed_tip: None,
                upstream_tip: None,
                materialized_views: None,
            }),
            network_name: "zcash-regtest".to_owned(),
            phase: WriterPhase::FollowingTip.into(),
            gap_blocks: Some(0),
            upstream_not_ready: None,
        }))
    }

    async fn visible_chain_events(
        &self,
        _request: Request<wallet::EventStreamStart>,
    ) -> Result<Response<Self::VisibleChainEventsStream>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub VisibleChainEvents",
        ))
    }

    async fn mempool_snapshot(
        &self,
        _request: Request<wallet::MempoolSnapshotRequest>,
    ) -> Result<Response<wallet::MempoolSnapshotResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub MempoolSnapshot",
        ))
    }

    async fn mempool_transaction(
        &self,
        _request: Request<MempoolTransactionRequest>,
    ) -> Result<Response<wallet::TransactionStatusResponse>, Status> {
        self.transaction
            .clone()
            .map(Response::new)
            .ok_or_else(|| Status::not_found("transaction is not in the test mempool"))
    }

    async fn mempool_events(
        &self,
        _request: Request<wallet::MempoolEventsRequest>,
    ) -> Result<Response<Self::MempoolEventsStream>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub MempoolEvents",
        ))
    }

    async fn transparent_mempool_outputs_by_address(
        &self,
        _request: Request<wallet::TransparentMempoolOutputsByAddressRequest>,
    ) -> Result<Response<wallet::TransparentMempoolOutputsByAddressResponse>, Status> {
        self.outputs_by_address
            .clone()
            .map(Response::new)
            .ok_or_else(|| {
                Status::unimplemented(
                    "test scaffold does not stub TransparentMempoolOutputsByAddress",
                )
            })
    }

    async fn transparent_mempool_spends_by_outpoint(
        &self,
        _request: Request<wallet::TransparentMempoolSpendsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentMempoolSpendsByOutpointResponse>, Status> {
        self.spends_by_outpoint
            .clone()
            .map(Response::new)
            .ok_or_else(|| {
                Status::unimplemented(
                    "test scaffold does not stub TransparentMempoolSpendsByOutpoint",
                )
            })
    }

    async fn transparent_mempool_outputs_by_outpoint(
        &self,
        _request: Request<wallet::TransparentMempoolOutputsByOutpointRequest>,
    ) -> Result<Response<wallet::TransparentOutputsByOutpointResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub TransparentMempoolOutputsByOutpoint",
        ))
    }

    async fn chain_value_pools_at_tip(
        &self,
        _request: Request<wallet::ChainValuePoolsAtTipRequest>,
    ) -> Result<Response<wallet::ChainValuePoolsAtTipResponse>, Status> {
        Err(Status::unimplemented(
            "test scaffold does not stub ChainValuePoolsAtTip",
        ))
    }
}

fn test_chain_epoch() -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: 11,
        network_name: "zcash-regtest".to_owned(),
        artifact_schema_version: 1,
        created_at_millis: 123,
        visible_tip: Some(wallet::BlockTip {
            height: 5,
            hash: "05".repeat(32),
        }),
        settled_tip: Some(wallet::BlockTip {
            height: 4,
            hash: "04".repeat(32),
        }),
        sapling_commitment_tree_size: 0,
        orchard_commitment_tree_size: 0,
        ironwood_commitment_tree_size: 0,
    }
}

async fn spawn_ingest_control_server(
    ingest_control: StaticIngestControl,
) -> eyre::Result<(
    SocketAddr,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(IngestControlServer::new(ingest_control))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(listener),
                server_cancel.cancelled_owned(),
            )
            .await
    });

    Ok((listen_addr, cancel, server))
}
