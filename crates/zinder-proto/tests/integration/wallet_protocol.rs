#![allow(
    missing_docs,
    reason = "Integration test names describe the native protocol contract under test."
)]

use eyre::eyre;
use prost::Message;
use zinder_proto::v1::{ingest, wallet};

#[test]
fn chain_epoch_round_trips_through_prost() -> eyre::Result<()> {
    let chain_epoch = synthetic_chain_epoch();
    let decoded_chain_epoch = round_trip(&chain_epoch)?;

    assert_eq!(decoded_chain_epoch.chain_epoch_id, 7);
    assert_eq!(decoded_chain_epoch.network_name, "zcash-regtest");
    assert_eq!(decoded_chain_epoch.tip_hash, vec![0x11; 32]);
    assert_eq!(decoded_chain_epoch.finalized_hash, vec![0x22; 32]);

    Ok(())
}

#[test]
fn compact_block_round_trips_through_prost() -> eyre::Result<()> {
    let compact_block = wallet::CompactBlock {
        height: 42,
        block_hash: vec![0x33; 32],
        payload_bytes: vec![0x01, 0x02, 0x03],
    };
    let decoded_compact_block = round_trip(&compact_block)?;

    assert_eq!(decoded_compact_block.height, 42);
    assert_eq!(decoded_compact_block.block_hash, vec![0x33; 32]);
    assert_eq!(decoded_compact_block.payload_bytes, vec![0x01, 0x02, 0x03]);

    Ok(())
}

#[test]
fn subtree_root_round_trips_through_prost() -> eyre::Result<()> {
    let subtree_root = wallet::SubtreeRoot {
        subtree_index: 9,
        root_hash: vec![0x44; 32],
        completing_block_hash: vec![0x45; 32],
        completing_block_height: 123,
    };
    let decoded_subtree_root = round_trip(&subtree_root)?;

    assert_eq!(decoded_subtree_root.subtree_index, 9);
    assert_eq!(decoded_subtree_root.root_hash, vec![0x44; 32]);
    assert_eq!(decoded_subtree_root.completing_block_hash, vec![0x45; 32]);
    assert_eq!(decoded_subtree_root.completing_block_height, 123);

    Ok(())
}

#[test]
fn latest_block_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::LatestBlockResponse {
        chain_epoch: Some(synthetic_chain_epoch()),
        latest_block: Some(wallet::BlockMetadata {
            height: 42,
            block_hash: vec![0x55; 32],
        }),
    };
    let decoded_response = round_trip(&response)?;
    let latest_block = decoded_response
        .latest_block
        .ok_or_else(|| eyre!("decoded latest block response is missing block metadata"))?;

    assert_eq!(latest_block.height, 42);
    assert_eq!(latest_block.block_hash, vec![0x55; 32]);
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
            },
        )),
    };
    let decoded_response = round_trip(&response)?;

    assert!(matches!(
        decoded_response.outcome,
        Some(wallet::broadcast_transaction_response::Outcome::Rejected(rejected))
            if rejected.error_code == Some(-25) && rejected.message == "bad-txns-invalid"
    ));

    Ok(())
}

#[test]
fn chain_event_envelope_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::ChainEventEnvelope {
        cursor: vec![0x99; 82],
        event_sequence: 11,
        chain_epoch: Some(synthetic_chain_epoch()),
        finalized_height: 40,
        event: Some(wallet::chain_event_envelope::Event::Committed(
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
        Some(wallet::chain_event_envelope::Event::Committed(committed))
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
        latest_writer_finalized_height: Some(40),
    };
    let decoded_response = round_trip(&response)?;

    assert_eq!(decoded_response.network_name, "zcash-regtest");
    assert_eq!(decoded_response.latest_writer_chain_epoch_id, Some(9));
    assert_eq!(decoded_response.latest_writer_tip_height, Some(42));
    assert_eq!(decoded_response.latest_writer_finalized_height, Some(40));

    Ok(())
}

#[test]
fn tree_state_response_round_trips_through_prost() -> eyre::Result<()> {
    let response = wallet::TreeStateResponse {
        chain_epoch: Some(synthetic_chain_epoch()),
        height: 42,
        block_hash: vec![0x66; 32],
        payload_bytes: br#"{"hash":"block"}"#.to_vec(),
    };
    let decoded_response = round_trip(&response)?;

    assert_eq!(decoded_response.height, 42);
    assert_eq!(decoded_response.block_hash, vec![0x66; 32]);
    assert_eq!(decoded_response.payload_bytes, br#"{"hash":"block"}"#);
    assert!(decoded_response.chain_epoch.is_some());

    Ok(())
}

fn synthetic_chain_epoch() -> wallet::ChainEpoch {
    wallet::ChainEpoch {
        chain_epoch_id: 7,
        network_name: "zcash-regtest".to_owned(),
        tip_height: 42,
        tip_hash: vec![0x11; 32],
        finalized_height: 40,
        finalized_hash: vec![0x22; 32],
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
