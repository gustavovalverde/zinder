use std::num::NonZeroU16;

use prost::Message;
use tempfile::TempDir;
use zinder_core::{
    BlockHash, BlockHeaderArtifact, BlockHeight, BlockId, CanonicalBlockFacts,
    CanonicalBlockFactsDigestVersion, CanonicalBlockReplayFormatVersion, CanonicalTransactionFacts,
    ChainTipMetadata, CommitmentTreeCheckpoint, CommitmentTreeFrontiers, ConsensusBranchId,
    LockTime, Network, NetworkUpgradeActivation, NetworkUpgradeActivations, PrivacyShape,
    SerializedBytesDigest, TransactionBlobArtifact, TransactionComponentCounts, TransactionId,
    TransactionIntrinsicValueBalances, TransactionLocation, TransactionPublicFacts,
    TransactionVersion, TransparentAddressScriptHash, TransparentInputFact, TransparentOutPoint,
    TransparentOutputFact, UnixTimestampMillis, encode_canonical_block_replay,
};
use zinder_proto::compat::lightwalletd::{ChainMetadata, CompactBlock as LightwalletdCompactBlock};
use zinder_store::{
    CanonicalBaselinePublication, CanonicalBuildBlock, CanonicalStoreBuildPlan,
    CanonicalStoreWorkload, RocksDbCanonicalBuilder, RocksDbCanonicalStore, RocksDbResourceBudget,
};
use zinder_wallet_projection::{
    WalletAddressTransactionKey, WalletAddressUnspentOutputKey, WalletOutpointKey,
    WalletProjectionFamilyRowCounts,
};
use zinder_wallet_rocksdb::{
    RocksDbWalletBuildOptions, RocksDbWalletBuildOutcome, RocksDbWalletStore,
    build_wallet_from_canonical,
};

#[test]
fn fixed_tip_build_matches_exact_version_one_wallet_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TempDir::new()?;
    let fixture = wallet_baseline_fixture();
    let canonical_store = build_ready_canonical_store(&temporary, &fixture)?;

    let outcome = build_wallet_from_canonical(
        &canonical_store,
        temporary.path().join("wallet"),
        RocksDbWalletBuildOptions {
            supported_reorg_depth: 2,
            ..RocksDbWalletBuildOptions::for_local_tests()
        },
    )?;

    assert_report(&outcome);
    assert_store(&outcome, &fixture)?;
    let expected_source = outcome.report.canonical_source_identity();
    drop(outcome.store);
    let reopened = RocksDbWalletStore::open_ready(
        temporary.path().join("wallet"),
        Network::ZcashRegtest,
        expected_source,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    assert_eq!(
        reopened.ready_evidence().source_sequence_digest,
        expected_source.source_sequence_digest()
    );
    Ok(())
}

fn build_ready_canonical_store(
    temporary: &TempDir,
    fixture: &WalletBaselineFixture,
) -> Result<RocksDbCanonicalStore, Box<dyn std::error::Error>> {
    let tip = block_id(&fixture.blocks[2]);
    let build_plan = CanonicalStoreBuildPlan::complete(&inactive_upgrade_activations()?, 0, tip)?;
    let mut builder = RocksDbCanonicalBuilder::create_fresh(
        temporary.path().join("canonical"),
        CanonicalStoreWorkload::Wallet,
        build_plan,
        RocksDbResourceBudget::for_local_tests(),
    )?;
    let build_blocks = fixture
        .blocks
        .iter()
        .enumerate()
        .map(|(index, facts)| canonical_build_block(facts.clone(), index == 2))
        .map(Ok::<_, std::io::Error>);
    builder.bulk_load_blocks(build_blocks)?;
    builder.load_subtree_roots(std::iter::empty())?;
    let tip_checkpoint = CommitmentTreeCheckpoint::new(tip, 3, CommitmentTreeFrontiers::default());
    builder.confirm_source_tip_checkpoint(&tip_checkpoint)?;
    let validated = builder.validate_for_publication()?;
    let publication = validated.prepare_baseline(CanonicalBaselinePublication::new(
        tip,
        UnixTimestampMillis::new(1_750_000_000_000),
    ))?;
    Ok(validated.publish_baseline(publication)?)
}

fn assert_report(outcome: &RocksDbWalletBuildOutcome) {
    assert_eq!(
        outcome.report.row_counts,
        WalletProjectionFamilyRowCounts {
            transparent_unspent_output_count: 4,
            transparent_unspent_output_by_address_count: 4,
            transparent_spent_output_count: 2,
            transparent_address_transaction_count: 6,
            transparent_address_balance_count: 2,
            reorg_undo_count: 2,
        }
    );
    assert_eq!(outcome.report.utxo_summary.utxo_count, 4);
    assert_eq!(outcome.report.utxo_summary.total_value_zat, 20);
    assert_eq!(
        outcome.report.projection_digest.as_bytes(),
        [
            0x8e, 0x5c, 0xf6, 0xfb, 0x63, 0x57, 0x9b, 0xa6, 0xf5, 0xc3, 0x36, 0x2b, 0x84, 0x07,
            0x0d, 0xcf, 0x09, 0x6a, 0xe4, 0x88, 0xd5, 0x90, 0x3b, 0x2c, 0x9b, 0xc5, 0xf5, 0xeb,
            0x68, 0xc2, 0xcd, 0x75,
        ]
    );
    assert_eq!(outcome.report.scanned_block_count, 3);
    assert_eq!(outcome.report.scanned_transaction_count, 4);
    assert_eq!(outcome.report.staged_output_count, 6);
    assert_eq!(outcome.report.staged_spend_count, 2);
    assert_eq!(outcome.report.historical_prevout_read_count, 0);
    assert!(outcome.report.logical_row_bytes > 0);
    assert!(outcome.report.write_batch_count > 0);
    assert!(outcome.report.peak_accounted_validation_relation_bytes > 0);
    assert!(
        outcome.report.peak_accounted_validation_relation_bytes
            <= outcome.report.max_accounted_validation_relation_bytes
    );
    assert_eq!(outcome.report.cold_validation_random_read_count, 6);
    let phases = outcome.report.phase_durations;
    let measured_phase_total = phases.store_initialization
        + phases.canonical_scan
        + phases.outpoint_sort
        + phases.outpoint_merge
        + phases.secondary_row_derivation
        + phases.logical_evidence
        + phases.row_load
        + phases.flush_and_cold_reopen
        + phases.cold_validation
        + phases.ready_publication;
    assert!(measured_phase_total <= phases.total);
    assert!(!phases.total.is_zero());
}

fn assert_store(
    outcome: &RocksDbWalletBuildOutcome,
    fixture: &WalletBaselineFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(outcome.store.address_balance(fixture.address_a)?, 12);
    assert_eq!(outcome.store.address_balance(fixture.address_b)?, 8);

    for outpoint in [
        fixture.left_unspent,
        fixture.block_two_unspent,
        fixture.final_primary_unspent,
        fixture.final_secondary_unspent,
    ] {
        assert!(outcome.store.find_unspent_output(outpoint)?.is_some());
    }
    for outpoint in [fixture.later_spent, fixture.same_block_spent] {
        assert!(outcome.store.find_spent_output(outpoint)?.is_some());
    }
    let left_unspent = outcome
        .store
        .find_unspent_output(fixture.left_unspent)?
        .ok_or("left unspent output must exist")?;
    assert_eq!(
        outcome
            .store
            .find_unspent_output_by_address_key(WalletAddressUnspentOutputKey::new(
                &left_unspent
            ))?,
        Some(left_unspent)
    );
    let address_transaction_key =
        WalletAddressTransactionKey::new(fixture.address_a, BlockHeight::new(3), 1);
    assert!(
        outcome
            .store
            .find_address_transaction(address_transaction_key)?
            .is_some()
    );
    let tip_undo = outcome
        .store
        .find_reorg_undo(BlockHeight::new(3))?
        .ok_or("tip undo must exist")?;
    assert_eq!(tip_undo.created_outpoints.len(), 3);
    assert!(tip_undo.spent_outpoints.is_empty());
    assert_eq!(tip_undo.address_transaction_keys.len(), 3);
    assert_eq!(
        tip_undo.created_outpoints,
        vec![
            WalletOutpointKey::new(fixture.same_block_spent),
            WalletOutpointKey::new(fixture.final_primary_unspent),
            WalletOutpointKey::new(fixture.final_secondary_unspent),
        ]
    );
    assert_address_pages(outcome, fixture)?;
    Ok(())
}

fn assert_address_pages(
    outcome: &RocksDbWalletBuildOutcome,
    fixture: &WalletBaselineFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    let address_a_outputs = collect_address_unspent_outputs(
        &outcome.store,
        fixture.address_a,
        NonZeroU16::new(1).ok_or("page size must be non-zero")?,
    )?;
    assert_eq!(
        address_a_outputs
            .iter()
            .map(WalletAddressUnspentOutputKey::new)
            .collect::<Vec<_>>(),
        vec![
            WalletAddressUnspentOutputKey::new(
                &outcome
                    .store
                    .find_unspent_output(fixture.left_unspent)?
                    .ok_or("left output must remain unspent")?,
            ),
            WalletAddressUnspentOutputKey::new(
                &outcome
                    .store
                    .find_unspent_output(fixture.final_primary_unspent)?
                    .ok_or("final primary output must remain unspent")?,
            ),
        ]
    );

    let address_a_history = collect_address_transaction_history(
        &outcome.store,
        fixture.address_a,
        NonZeroU16::new(1).ok_or("page size must be non-zero")?,
    )?;
    assert_eq!(
        address_a_history
            .iter()
            .map(|transaction| transaction.key)
            .collect::<Vec<_>>(),
        vec![
            WalletAddressTransactionKey::new(fixture.address_a, BlockHeight::new(1), 0),
            WalletAddressTransactionKey::new(fixture.address_a, BlockHeight::new(2), 0),
            WalletAddressTransactionKey::new(fixture.address_a, BlockHeight::new(3), 0),
            WalletAddressTransactionKey::new(fixture.address_a, BlockHeight::new(3), 1),
        ]
    );
    Ok(())
}

fn collect_address_unspent_outputs(
    store: &zinder_wallet_rocksdb::RocksDbWalletStore,
    address_script_hash: TransparentAddressScriptHash,
    page_size: NonZeroU16,
) -> Result<Vec<zinder_wallet_projection::WalletUnspentOutput>, Box<dyn std::error::Error>> {
    let mut outputs = Vec::new();
    let mut after = None;
    loop {
        let page = store.address_unspent_outputs_page(address_script_hash, after, page_size)?;
        outputs.extend(page.outputs);
        let Some(next_page_after) = page.next_page_after else {
            return Ok(outputs);
        };
        after = Some(next_page_after);
    }
}

fn collect_address_transaction_history(
    store: &zinder_wallet_rocksdb::RocksDbWalletStore,
    address_script_hash: TransparentAddressScriptHash,
    page_size: NonZeroU16,
) -> Result<Vec<zinder_wallet_projection::WalletAddressTransaction>, Box<dyn std::error::Error>> {
    let mut transactions = Vec::new();
    let mut after = None;
    loop {
        let page = store.address_transaction_history_page(address_script_hash, after, page_size)?;
        transactions.extend(page.transactions);
        let Some(next_page_after) = page.next_page_after else {
            return Ok(transactions);
        };
        after = Some(next_page_after);
    }
}

struct WalletBaselineFixture {
    blocks: [CanonicalBlockFacts; 3],
    address_a: TransparentAddressScriptHash,
    address_b: TransparentAddressScriptHash,
    left_unspent: TransparentOutPoint,
    later_spent: TransparentOutPoint,
    same_block_spent: TransparentOutPoint,
    block_two_unspent: TransparentOutPoint,
    final_primary_unspent: TransparentOutPoint,
    final_secondary_unspent: TransparentOutPoint,
}

fn wallet_baseline_fixture() -> WalletBaselineFixture {
    let network = Network::ZcashRegtest;
    let address_a = TransparentAddressScriptHash::from_bytes([0xa1; 32]);
    let address_b = TransparentAddressScriptHash::from_bytes([0xb2; 32]);
    let transaction_one = TransactionId::from_bytes([0x11; 32]);
    let transaction_two = TransactionId::from_bytes([0x22; 32]);
    let transaction_three = TransactionId::from_bytes([0x31; 32]);
    let transaction_four = TransactionId::from_bytes([0x32; 32]);
    let left_unspent = TransparentOutPoint::new(transaction_one, 0);
    let later_spent = TransparentOutPoint::new(transaction_one, 1);
    let same_block_spent = TransparentOutPoint::new(transaction_three, 0);
    let block_two_unspent = TransparentOutPoint::new(transaction_two, 0);
    let final_primary_unspent = TransparentOutPoint::new(transaction_four, 0);
    let final_secondary_unspent = TransparentOutPoint::new(transaction_four, 1);

    let block_one = block_facts(
        1,
        network.genesis_hash().as_bytes(),
        [0xc1; 32],
        vec![transaction_facts(
            transaction_one,
            true,
            vec![TransparentInputFact::new(
                0,
                TransparentOutPoint::COINBASE_SENTINEL,
            )],
            vec![
                TransparentOutputFact::new(0, 11, [0x51], address_a),
                TransparentOutputFact::new(1, 7, [0x52], address_a),
            ],
        )],
    );
    let block_two = block_facts(
        2,
        [0xc1; 32],
        [0xc2; 32],
        vec![transaction_facts(
            transaction_two,
            false,
            vec![TransparentInputFact::new(0, later_spent)],
            vec![TransparentOutputFact::new(0, 5, [0x53], address_b)],
        )],
    );
    let block_three = block_facts(
        3,
        [0xc2; 32],
        [0xc3; 32],
        vec![
            transaction_facts(
                transaction_three,
                false,
                Vec::new(),
                vec![TransparentOutputFact::new(0, 2, [0x54], address_a)],
            ),
            transaction_facts(
                transaction_four,
                false,
                vec![TransparentInputFact::new(0, same_block_spent)],
                vec![
                    TransparentOutputFact::new(0, 1, [0x55], address_a),
                    TransparentOutputFact::new(1, 3, [0x56], address_b),
                ],
            ),
        ],
    );
    WalletBaselineFixture {
        blocks: [block_one, block_two, block_three],
        address_a,
        address_b,
        left_unspent,
        later_spent,
        same_block_spent,
        block_two_unspent,
        final_primary_unspent,
        final_secondary_unspent,
    }
}

fn inactive_upgrade_activations()
-> Result<NetworkUpgradeActivations, zinder_core::NetworkUpgradeActivationsError> {
    let activations = [
        "Overwinter",
        "Sapling",
        "Blossom",
        "Heartwood",
        "Canopy",
        "NU5",
        "NU6",
        "NU6.1",
        "NU6.2",
        "NU6.3",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, name)| NetworkUpgradeActivation {
        branch_id: ConsensusBranchId::new(u32::try_from(index).unwrap_or(u32::MAX) + 1),
        activation_height: BlockHeight::new(100 + u32::try_from(index).unwrap_or(u32::MAX)),
        name: name.to_owned(),
    })
    .collect();
    NetworkUpgradeActivations::new(Network::ZcashRegtest, activations)
}

fn canonical_build_block(facts: CanonicalBlockFacts, is_tip: bool) -> CanonicalBuildBlock {
    let height = facts.block_header.height;
    let block_hash = facts.block_header.block_hash;
    let parent_hash = facts.block_header.parent_hash;
    let compact_payload = LightwalletdCompactBlock {
        height: u64::from(height.value()),
        hash: block_hash.as_bytes().to_vec(),
        prev_hash: parent_hash.as_bytes().to_vec(),
        chain_metadata: Some(ChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        }),
        ..Default::default()
    }
    .encode_to_vec();
    let transaction_blobs = facts
        .transactions
        .iter()
        .enumerate()
        .map(|(index, transaction)| {
            TransactionBlobArtifact::new(
                TransactionLocation::new(
                    transaction.public_facts.transaction_id,
                    height,
                    block_hash,
                    u32::try_from(index).unwrap_or(u32::MAX),
                ),
                transaction.public_facts.transaction_id.as_bytes(),
            )
        })
        .collect();
    let tree_state_checkpoint = is_tip.then(|| {
        CommitmentTreeCheckpoint::new(
            BlockId::new(height, block_hash),
            u32::try_from(facts.block_header.block_time).unwrap_or(u32::MAX),
            CommitmentTreeFrontiers::default(),
        )
    });
    let replay_envelope = encode_canonical_block_replay(
        &facts,
        CanonicalBlockReplayFormatVersion::V1,
        CanonicalBlockFactsDigestVersion::V1,
    );
    CanonicalBuildBlock {
        facts,
        replay_envelope,
        compact_block: zinder_core::CompactBlockArtifact::new(height, block_hash, compact_payload),
        tip_metadata: ChainTipMetadata::new(0, 0, 0),
        tree_state_checkpoint,
        block_final_note_commitment_roots: None,
        transaction_blobs,
        block_blob: None,
    }
}

fn block_id(facts: &CanonicalBlockFacts) -> BlockId {
    BlockId::new(facts.block_header.height, facts.block_header.block_hash)
}

fn block_facts(
    height: u32,
    parent_hash: [u8; 32],
    block_hash: [u8; 32],
    transactions: Vec<CanonicalTransactionFacts>,
) -> CanonicalBlockFacts {
    CanonicalBlockFacts {
        block_header: BlockHeaderArtifact::new(
            BlockHeight::new(height),
            BlockHash::from_bytes(block_hash),
            BlockHash::from_bytes(parent_hash),
            [0; 32],
            [0; 32],
            i64::from(height),
            0,
            [0; 32],
            0,
            0,
        ),
        serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(&block_hash),
        transactions,
    }
}

fn transaction_facts(
    transaction_id: TransactionId,
    is_coinbase: bool,
    transparent_inputs: Vec<TransparentInputFact>,
    transparent_outputs: Vec<TransparentOutputFact>,
) -> CanonicalTransactionFacts {
    let counts = TransactionComponentCounts {
        transparent_input_count: u32::try_from(transparent_inputs.len()).unwrap_or(u32::MAX),
        transparent_output_count: u32::try_from(transparent_outputs.len()).unwrap_or(u32::MAX),
        ..TransactionComponentCounts::EMPTY
    };
    CanonicalTransactionFacts {
        public_facts: TransactionPublicFacts {
            transaction_id,
            auth_digest: None,
            wtxid: None,
            version: TransactionVersion::V4,
            consensus_branch_id: None,
            lock_time: LockTime::Unlocked,
            expiry_height: None,
            size_bytes: 32,
            counts,
            orchard_value_balance_zat: None,
            orchard_anchor: None,
            ironwood_value_balance_zat: None,
            privacy_shape: PrivacyShape::Unclassified,
            is_coinbase,
            unsupported_sections: Vec::new(),
        },
        serialized_bytes_digest: SerializedBytesDigest::from_serialized_bytes(
            &transaction_id.as_bytes(),
        ),
        intrinsic_value_balances: TransactionIntrinsicValueBalances::default(),
        transparent_inputs,
        transparent_outputs,
    }
}
