#![allow(
    missing_docs,
    reason = "Integration test names describe the native protocol contract under test."
)]

use eyre::eyre;
use prost::Message;
use zinder_core::wire::{
    decode_rpc_block_hash_hex, decode_rpc_transaction_id_hex, encode_rpc_block_hash_hex,
    encode_rpc_transaction_id_hex,
};
use zinder_core::{BlockHash, TransactionId};
use zinder_proto::v1::{ingest, ops, wallet};

#[test]
fn chain_epoch_round_trips_through_prost() -> eyre::Result<()> {
    let chain_epoch = synthetic_chain_epoch();
    let decoded_chain_epoch = round_trip(&chain_epoch)?;

    assert_eq!(decoded_chain_epoch.chain_epoch_id, 7);
    assert_eq!(decoded_chain_epoch.network_name, "zcash-regtest");
    assert_eq!(decoded_chain_epoch.tip_hash, "11".repeat(32));
    assert_eq!(decoded_chain_epoch.safe_tip_hash, "22".repeat(32));

    Ok(())
}

#[test]
fn compact_block_round_trips_through_prost() -> eyre::Result<()> {
    let compact_block = wallet::CompactBlock {
        height: 42,
        block_hash: "33".repeat(32),
        payload_bytes: vec![0x01, 0x02, 0x03],
    };
    let decoded_compact_block = round_trip(&compact_block)?;

    assert_eq!(decoded_compact_block.height, 42);
    assert_eq!(decoded_compact_block.block_hash, "33".repeat(32));
    assert_eq!(decoded_compact_block.payload_bytes, vec![0x01, 0x02, 0x03]);

    Ok(())
}

#[test]
fn subtree_root_round_trips_through_prost() -> eyre::Result<()> {
    let subtree_root = wallet::SubtreeRoot {
        subtree_index: 9,
        root_hash: vec![0x44; 32],
        completing_block_hash: "45".repeat(32),
        completing_block_height: 123,
    };
    let decoded_subtree_root = round_trip(&subtree_root)?;

    assert_eq!(decoded_subtree_root.subtree_index, 9);
    assert_eq!(decoded_subtree_root.root_hash, vec![0x44; 32]);
    assert_eq!(decoded_subtree_root.completing_block_hash, "45".repeat(32));
    assert_eq!(decoded_subtree_root.completing_block_height, 123);

    Ok(())
}

#[test]
fn latest_block_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::LatestBlockResponse {
        chain_epoch: Some(synthetic_chain_epoch()),
        latest_block: Some(wallet::BlockMetadata {
            height: 42,
            block_hash: "55".repeat(32),
        }),
    };
    let decoded_response = round_trip(&response)?;
    let latest_block = decoded_response
        .latest_block
        .ok_or_else(|| eyre!("decoded latest block response is missing block metadata"))?;

    assert_eq!(latest_block.height, 42);
    assert_eq!(latest_block.block_hash, "55".repeat(32));
    assert!(decoded_response.chain_epoch.is_some());

    Ok(())
}

#[test]
fn broadcast_transaction_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::BroadcastTransactionResponse {
        outcome: Some(wallet::broadcast_transaction_response::Outcome::Rejected(
            wallet::BroadcastRejected {
                error_code: Some(-25),
                message: "bad-txns-invalid".to_owned(),
                kind: wallet::BroadcastRejectionReason::InvalidSignature as i32,
            },
        )),
    };
    let decoded_response = round_trip(&response)?;

    assert!(matches!(
        decoded_response.outcome,
        Some(wallet::broadcast_transaction_response::Outcome::Rejected(rejected))
            if rejected.error_code == Some(-25)
                && rejected.message == "bad-txns-invalid"
                && rejected.kind == wallet::BroadcastRejectionReason::InvalidSignature as i32
    ));

    Ok(())
}

#[test]
fn broadcast_transaction_response_carries_queued_outcome() -> eyre::Result<()> {
    let response = wallet::BroadcastTransactionResponse {
        outcome: Some(wallet::broadcast_transaction_response::Outcome::Queued(
            wallet::BroadcastQueued {
                message: "already queued for download".to_owned(),
            },
        )),
    };
    let decoded_response = round_trip(&response)?;

    assert!(matches!(
        decoded_response.outcome,
        Some(wallet::broadcast_transaction_response::Outcome::Queued(queued))
            if queued.message == "already queued for download"
    ));

    Ok(())
}

#[test]
fn chain_event_envelope_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::ChainEventEnvelope {
        cursor: vec![0x99; 82],
        event_sequence: 11,
        chain_epoch: Some(synthetic_chain_epoch()),
        safe_tip_height: 40,
        event: Some(wallet::chain_event_envelope::Event::ChainCommitted(
            wallet::ChainCommitted {
                committed: Some(wallet::ChainEpochCommitted {
                    chain_epoch: Some(synthetic_chain_epoch()),
                    start_height: 40,
                    end_height: 42,
                }),
            },
        )),
    };
    let decoded_response = round_trip(&response)?;

    assert_eq!(decoded_response.cursor, vec![0x99; 82]);
    assert_eq!(decoded_response.event_sequence, 11);
    assert!(decoded_response.chain_epoch.is_some());
    assert!(matches!(
        decoded_response.event,
        Some(wallet::chain_event_envelope::Event::ChainCommitted(committed))
            if committed.committed.as_ref().is_some_and(|inner| {
                inner.start_height == 40 && inner.end_height == 42
            })
    ));

    Ok(())
}

#[test]
fn writer_status_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = ingest::WriterStatusResponse {
        network_name: "zcash-regtest".to_owned(),
        latest_writer_chain_epoch_id: Some(9),
        latest_writer_tip_height: Some(42),
        latest_writer_safe_tip_height: Some(40),
        phase: ingest::WriterPhase::FollowingTip.into(),
        gap_blocks: Some(1),
        upstream_not_ready: Some(ops::UpstreamNotReadyDetail {
            upstream_committed_height: Some(42),
            upstream_estimated_height: Some(42),
            upstream_verification_progress: Some(1.0),
            upstream_health_source: "zebra_ready_endpoint".to_owned(),
            upstream_health_reason: "ok".to_owned(),
        }),
    };
    let decoded_response = round_trip(&response)?;

    assert_eq!(decoded_response.network_name, "zcash-regtest");
    assert_eq!(decoded_response.latest_writer_chain_epoch_id, Some(9));
    assert_eq!(decoded_response.latest_writer_tip_height, Some(42));
    assert_eq!(decoded_response.latest_writer_safe_tip_height, Some(40));
    assert_eq!(decoded_response.phase(), ingest::WriterPhase::FollowingTip);
    assert_eq!(decoded_response.gap_blocks, Some(1));
    let detail = decoded_response
        .upstream_not_ready
        .ok_or_else(|| eyre!("upstream_not_ready missing"))?;
    assert_eq!(detail.upstream_health_source, "zebra_ready_endpoint");
    assert_eq!(detail.upstream_health_reason, "ok");

    Ok(())
}

#[test]
fn writer_phase_enum_round_trips_each_variant() -> eyre::Result<()> {
    for phase in [
        ingest::WriterPhase::Unspecified,
        ingest::WriterPhase::AwaitingUpstream,
        ingest::WriterPhase::BulkCatchup,
        ingest::WriterPhase::FollowingTip,
    ] {
        let response = ingest::WriterStatusResponse {
            network_name: "zcash-regtest".to_owned(),
            latest_writer_chain_epoch_id: None,
            latest_writer_tip_height: None,
            latest_writer_safe_tip_height: None,
            phase: phase.into(),
            gap_blocks: None,
            upstream_not_ready: None,
        };
        let decoded = round_trip(&response)?;
        assert_eq!(decoded.phase(), phase, "phase round-trip failed: {phase:?}");
    }

    Ok(())
}

#[test]
fn tree_state_checkpoint_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::TreeStateResponse {
        chain_epoch: Some(synthetic_chain_epoch()),
        height: 42,
        block_hash: "66".repeat(32),
        payload_bytes: br#"{"hash":"block"}"#.to_vec(),
    };
    let decoded_response = round_trip(&response)?;

    assert_eq!(decoded_response.height, 42);
    assert_eq!(decoded_response.block_hash, "66".repeat(32));
    assert_eq!(decoded_response.payload_bytes, br#"{"hash":"block"}"#);
    assert!(decoded_response.chain_epoch.is_some());

    Ok(())
}

/// Locks the wallet-plane txid wire-shape contract.
///
/// Every wallet-plane `transaction_id` wire field must carry the
/// canonical RPC-form hex encoded through
/// [`encode_rpc_transaction_id_hex`], and consumers must be able to
/// decode it back with [`decode_rpc_transaction_id_hex`] into the same
/// internal-form `TransactionId` the upstream observed. A regression to
/// internal byte order on the wire (the bug ADR-0021 fixes) fails this
/// test deterministically because the assert compares against the RPC-
/// form string a Zexplorer user paste would produce.
#[test]
fn transaction_id_wire_field_carries_rpc_form_hex() -> eyre::Result<()> {
    let internal_txid = TransactionId::from_bytes([
        0x36, 0x94, 0x55, 0xb7, 0x8a, 0xfc, 0xa3, 0xdc, 0xb5, 0x2b, 0xec, 0xfd, 0x38, 0x72, 0xba,
        0xf5, 0xd0, 0x51, 0xb3, 0x2e, 0x81, 0x65, 0xbc, 0x2c, 0x79, 0x61, 0x06, 0x9e, 0xe6, 0x0c,
        0xca, 0xc3,
    ]);
    let canonical_rpc_form = "c3ca0ce69e0661792cbc65812eb351d0f5ba7238fdec2bb5dca3fc8ab7559436";

    let request = wallet::TransactionRequest {
        transaction_id: encode_rpc_transaction_id_hex(internal_txid),
        at_epoch: None,
    };
    let decoded_request = round_trip(&request)?;
    assert_eq!(decoded_request.transaction_id, canonical_rpc_form);
    assert_eq!(
        decode_rpc_transaction_id_hex(&decoded_request.transaction_id)?,
        internal_txid,
    );

    let accepted = wallet::BroadcastAccepted {
        transaction_id: encode_rpc_transaction_id_hex(internal_txid),
    };
    let decoded_accepted = round_trip(&accepted)?;
    assert_eq!(decoded_accepted.transaction_id, canonical_rpc_form);

    Ok(())
}

/// Locks the wallet-plane block-hash wire-shape contract.
///
/// Two responses carry block hashes on different wire shapes
/// (`ChainEpoch.tip_hash` and `BlockMetadata.block_hash`); both must
/// encode the canonical RPC form so the consumer can compare strings
/// byte-for-byte without knowing which response produced the field.
#[test]
fn block_hash_wire_field_carries_rpc_form_hex() -> eyre::Result<()> {
    let internal_block_hash = BlockHash::from_bytes([
        0xee, 0xce, 0xfc, 0x22, 0xf4, 0xa0, 0x9f, 0xe4, 0x30, 0x6f, 0x40, 0xaf, 0xa3, 0xa6, 0xf3,
        0xdb, 0x17, 0x3f, 0x1a, 0x5e, 0x3a, 0x0c, 0xcc, 0x3d, 0x8f, 0xeb, 0x22, 0xc6, 0xba, 0xf1,
        0x33, 0x00,
    ]);
    let canonical_rpc_form = "0033f1bac622eb8f3dcc0c3a5e1a3f17dbf3a6a3af406f30e49fa0f422fcceee";

    let metadata = wallet::BlockMetadata {
        height: 4_031_230,
        block_hash: encode_rpc_block_hash_hex(internal_block_hash),
    };
    let decoded_metadata = round_trip(&metadata)?;
    assert_eq!(decoded_metadata.block_hash, canonical_rpc_form);
    assert_eq!(
        decode_rpc_block_hash_hex(&decoded_metadata.block_hash)?,
        internal_block_hash,
    );

    Ok(())
}

fn synthetic_chain_epoch() -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: 7,
        network_name: "zcash-regtest".to_owned(),
        tip_height: 42,
        tip_hash: "11".repeat(32),
        safe_tip_height: 40,
        safe_tip_hash: "22".repeat(32),
        artifact_schema_version: 1,
        created_at_millis: 1_774_670_400_000,
        sapling_commitment_tree_size: 0,
        orchard_commitment_tree_size: 0,
    }
}

fn round_trip<MessageType>(message: &MessageType) -> Result<MessageType, prost::DecodeError>
where
    MessageType: Message + Default,
{
    let encoded_message = message.encode_to_vec();
    MessageType::decode(encoded_message.as_slice())
}
