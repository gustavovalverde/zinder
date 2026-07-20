#![allow(
    missing_docs,
    reason = "Live test names describe the behavior under test."
)]

use std::sync::Arc;
use std::{fs, io, num::NonZeroU32};

use eyre::{Result, eyre};
use tempfile::tempdir;
use tonic::{Code, Request};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::wire::encode_zinder_native_chain_name;
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, Network, NetworkUpgradeActivations,
    TransparentAddressScriptHash, TransparentOutPoint,
};
use zinder_proto::compat::lightwalletd::{
    self, compact_tx_streamer_server::CompactTxStreamer as LightwalletdCompactTxStreamer,
};
use zinder_query::{
    QueryError, TransparentAddressUnspentOutputsRequest, WalletQuery, WalletQueryApi,
};
use zinder_source::NodeSource;
use zinder_store::{ArtifactFamily, ChainStoreOptions, PrimaryChainStore, StoreError};
use zinder_testkit::live::{init, require_live, require_live_for};

use crate::common::{
    BoundedIngestConfigToml, WalletServingIngestConfigToml, assert_native_wallet_read_responses,
    basic_auth_credentials, bounded_ingest_config_toml, fetch_live_network_upgrade_activations,
    live_bulk_catchup_run_config, wallet_serving_ingest_config_toml,
    zebra_source_from_bulk_catchup, zinder_ingest_command,
};

const WALLET_SERVING_BOUNDED_DEPTH_BLOCKS: u32 = 150;

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
async fn cli_runs_bounded_ingest_loop_from_config() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live()? else {
        return Ok(());
    };
    let (username, password) = basic_auth_credentials(&env)?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let target_height = match env.network() {
        Network::ZcashRegtest => BlockHeight::new(1),
        Network::ZcashTestnet | Network::ZcashMainnet => BlockHeight::new(2),
        other => return Err(eyre!("unsupported network for CLI test: {other:?}")),
    };
    fs::write(
        &config_path,
        bounded_ingest_config_toml(&BoundedIngestConfigToml {
            network_name: encode_zinder_native_chain_name(env.network()),
            json_rpc_addr: &env.target.json_rpc_addr,
            node_auth_username: username,
            node_auth_password: password,
            storage_path: &storage_path,
            target_height: target_height.value(),
            request_timeout_secs: env.target.request_timeout.as_secs(),
            allow_reorg_window_settlement: true,
        })?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| eyre!("config path not utf-8"))?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("event=\"chain_committed\""), "{stderr}");
    assert!(stderr.contains("chain_epoch_id=1"), "{stderr}");
    assert!(
        stderr.contains(&format!("tip_height={}", target_height.value())),
        "{stderr}"
    );

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let reader = store.current_chain_epoch_reader()?;
    let compact_block = reader
        .compact_block_at(target_height)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "tip compact block artifact"))?;

    assert_eq!(compact_block.height, target_height);
    let activations = fetch_live_network_upgrade_activations(&env).await?;
    assert_native_wallet_read_responses(
        &store,
        env.network(),
        1,
        target_height.value(),
        activations,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "live test; see CLAUDE.md §Live Node Tests"]
#[allow(
    clippy::too_many_lines,
    reason = "live CLI smoke keeps node-derived floor resolution, process execution, and wallet-read assertions in one auditable scenario"
)]
async fn cli_runs_bounded_wallet_serving_loop_from_config() -> Result<()> {
    let _guard = init();
    let Some(env) = require_live_for(&[Network::ZcashTestnet, Network::ZcashMainnet])? else {
        return Ok(());
    };
    let (username, password) = basic_auth_credentials(&env)?;

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("zinder-store");
    let config_path = tempdir.path().join("zinder-ingest.toml");
    let activations = fetch_live_network_upgrade_activations(&env).await?;
    let probe_config = live_bulk_catchup_run_config(
        &env,
        &storage_path,
        BlockHeight::new(1),
        BlockHeight::new(1),
        NonZeroU32::new(1).ok_or_else(|| eyre!("invalid probe batch size"))?,
        true,
        Arc::clone(&activations),
    );
    let source = zebra_source_from_bulk_catchup(&probe_config)?;
    let wallet_serving_floor = activations
        .earliest_wallet_servable_activation()
        .ok_or_else(|| eyre!("node did not advertise Sapling or NU5 activation heights"))?
        .activation_height;
    if wallet_serving_floor == BlockHeight::new(0) {
        return Err(eyre!("wallet-serving floor cannot be genesis"));
    }
    let to_height = BlockHeight::new(
        wallet_serving_floor
            .value()
            .checked_add(WALLET_SERVING_BOUNDED_DEPTH_BLOCKS - 1)
            .ok_or_else(|| eyre!("wallet-serving bounded height overflowed"))?,
    );
    let node_tip = NodeSource::tip_id(&source).await?.height;
    if node_tip.value() <= to_height.value() {
        return Err(eyre!(
            "wallet-serving bounded test needs tip above {}; got {}",
            to_height.value(),
            node_tip.value()
        ));
    }

    fs::write(
        &config_path,
        wallet_serving_ingest_config_toml(&WalletServingIngestConfigToml {
            network_name: encode_zinder_native_chain_name(env.network()),
            json_rpc_addr: &env.target.json_rpc_addr,
            node_auth_username: username,
            node_auth_password: password,
            storage_path: &storage_path,
            target_height: to_height.value(),
            request_timeout_secs: env.target.request_timeout.as_secs(),
        })?,
    )?;

    let output = zinder_ingest_command()
        .args([
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| eyre!("config path not utf-8"))?,
        ])
        .output()?;

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    let checkpoint_height = BlockHeight::new(wallet_serving_floor.value() - 1);
    assert!(
        stderr.contains("event=\"wallet_serving_modifiers_resolved\""),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("from_height={}", wallet_serving_floor.value())),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("checkpoint_height={}", checkpoint_height.value())),
        "{stderr}"
    );
    assert!(stderr.contains("event=\"chain_committed\""), "{stderr}");
    assert!(
        stderr.contains(&format!("tip_height={}", to_height.value())),
        "{stderr}"
    );

    let store =
        PrimaryChainStore::open(&storage_path, ChainStoreOptions::for_network(env.network()))?;
    let transparent_output_index_evidence = {
        let reader = store.current_chain_epoch_reader()?;
        assert!(matches!(
            reader.compact_block_at(checkpoint_height),
            Err(StoreError::ArtifactMissing { .. })
        ));
        let first_compact_block = reader
            .compact_block_at(wallet_serving_floor)?
            .ok_or_else(|| eyre!("missing first wallet-serving compact block"))?;
        let tip_tree_state = reader
            .tree_state_checkpoint_at_or_before(to_height)?
            .ok_or_else(|| eyre!("missing bounded wallet-serving tip tree state"))?;
        assert_eq!(first_compact_block.height, wallet_serving_floor);
        assert_eq!(tip_tree_state.height, to_height);
        assert_transaction_blobs_retained_for_range(&reader, wallet_serving_floor, to_height)?;
        assert_transparent_output_indexes_for_range(&reader, wallet_serving_floor, to_height)?
    };
    assert_wallet_read_errors_below_floor(&store, Arc::clone(&activations), checkpoint_height)
        .await?;
    assert_wallet_transparent_output_indexes(
        &store,
        Arc::clone(&activations),
        &transparent_output_index_evidence,
        wallet_serving_floor,
    )
    .await?;
    assert_native_wallet_read_responses(
        &store,
        env.network(),
        wallet_serving_floor.value(),
        to_height.value(),
        Arc::clone(&activations),
    )
    .await?;

    let wallet_query = WalletQuery::new(store, (), Arc::clone(&activations));
    let visible_tip_block = wallet_query.visible_tip_block(None).await?;
    assert_eq!(visible_tip_block.height, to_height);
    let compact_blocks = wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(wallet_serving_floor, to_height),
            None,
        )
        .await?;
    assert_eq!(
        compact_blocks.compact_blocks.len(),
        usize::try_from(WALLET_SERVING_BOUNDED_DEPTH_BLOCKS)?
    );
    let tree_state = wallet_query.tree_state_at(to_height, None).await?;
    assert_eq!(tree_state.height, to_height);
    Ok(())
}

fn assert_transaction_blobs_retained_for_range(
    reader: &zinder_store::ChainEpochReader<'_>,
    from_height: BlockHeight,
    to_height: BlockHeight,
) -> Result<()> {
    for height_value in from_height.value()..=to_height.value() {
        let height = BlockHeight::new(height_value);
        let transaction_ids = reader.transaction_ids_at_height(height)?;
        if transaction_ids.is_empty() {
            return Err(eyre!(
                "wallet-serving block {} has no indexed transactions",
                height.value()
            ));
        }

        for (transaction_index, transaction_id) in transaction_ids.into_iter().enumerate() {
            let transaction_blob = reader.transaction_blob_by_id(transaction_id)?.ok_or_else(
                || {
                    eyre!(
                        "missing transaction blob for transaction {transaction_id:?} at height {}",
                        height.value()
                    )
                },
            )?;
            let expected_transaction_index = u32::try_from(transaction_index)?;

            assert_eq!(transaction_blob.location.transaction_id, transaction_id);
            assert_eq!(transaction_blob.location.block_height, height);
            assert_eq!(
                transaction_blob.location.tx_index_in_block,
                expected_transaction_index
            );
            assert!(
                !transaction_blob.raw_transaction_bytes.is_empty(),
                "transaction blob for {transaction_id:?} at height {} is empty",
                height.value()
            );
        }
    }

    Ok(())
}

fn assert_transparent_output_indexes_for_range(
    reader: &zinder_store::ChainEpochReader<'_>,
    from_height: BlockHeight,
    to_height: BlockHeight,
) -> Result<TransparentOutputIndexEvidence> {
    let transparent_outputs =
        collect_transparent_output_index_evidence(reader, from_height, to_height)?;
    let unspent_outputs =
        assert_address_output_index_for_unspent_outputs(reader, from_height, &transparent_outputs)?;

    Ok(TransparentOutputIndexEvidence {
        transparent_outputs,
        unspent_outputs,
    })
}

fn collect_transparent_output_index_evidence(
    reader: &zinder_store::ChainEpochReader<'_>,
    from_height: BlockHeight,
    to_height: BlockHeight,
) -> Result<Vec<TransparentOutputEvidence>> {
    let mut transparent_outputs = Vec::new();
    for height_value in from_height.value()..=to_height.value() {
        let height = BlockHeight::new(height_value);
        for transaction_id in reader.transaction_ids_at_height(height)? {
            let transaction_facts = reader.transaction_facts_by_id(transaction_id)?.ok_or_else(
                || {
                    eyre!(
                        "missing transaction facts for transaction {transaction_id:?} at height {}",
                        height.value()
                    )
                },
            )?;
            for output in transaction_facts.transparent_outputs {
                let outpoint = TransparentOutPoint::new(transaction_id, output.output_index);
                let indexed_outputs = reader.transparent_outputs_by_outpoints(&[outpoint])?;
                let indexed_output = indexed_outputs.get(&outpoint).ok_or_else(|| {
                    eyre!(
                        "missing transparent output index for outpoint {outpoint:?} at height {}",
                        height.value()
                    )
                })?;

                assert_eq!(indexed_output.outpoint, outpoint);
                assert_eq!(indexed_output.value_zat, output.value_zat);
                assert_eq!(indexed_output.script_pub_key, output.script_pub_key);
                assert_eq!(
                    indexed_output.address_script_hash,
                    output.address_script_hash
                );
                assert_eq!(indexed_output.block_height, height);

                transparent_outputs.push(TransparentOutputEvidence {
                    outpoint,
                    address_script_hash: output.address_script_hash,
                    value_zat: output.value_zat,
                    script_pub_key: output.script_pub_key,
                    block_height: height,
                    block_hash: indexed_output.block_hash,
                });
            }
        }
    }
    if transparent_outputs.is_empty() {
        return Err(eyre!(
            "bounded wallet-serving range contains no transparent outputs to prove"
        ));
    }

    Ok(transparent_outputs)
}

fn assert_address_output_index_for_unspent_outputs(
    reader: &zinder_store::ChainEpochReader<'_>,
    from_height: BlockHeight,
    transparent_outputs: &[TransparentOutputEvidence],
) -> Result<Vec<TransparentOutputEvidence>> {
    let outpoints = transparent_outputs
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    let unspent_entries = reader.transparent_unspent_outputs_by_outpoints(&outpoints)?;
    if unspent_entries.is_empty() {
        return Err(eyre!(
            "bounded wallet-serving range contains no unspent transparent outputs to prove address-output index"
        ));
    }

    let mut unspent_outputs = Vec::with_capacity(unspent_entries.len());
    for entry in unspent_entries {
        let evidence = find_transparent_output_evidence(transparent_outputs, entry.outpoint)?;
        let Some(output) = entry.output else {
            return Err(eyre!(
                "unspent transparent output entry for {outpoint:?} is missing output bytes",
                outpoint = entry.outpoint
            ));
        };
        assert_eq!(output.value_zat, evidence.value_zat);
        assert_eq!(output.script_pub_key, evidence.script_pub_key);

        let address_outputs = reader.address_output_index(
            evidence.address_script_hash,
            from_height,
            NonZeroU32::MAX,
        )?;
        let address_output = address_outputs
            .iter()
            .find(|output| output.outpoint == evidence.outpoint)
            .ok_or_else(|| {
                eyre!(
                    "missing address-output index entry for unspent outpoint {:?}",
                    evidence.outpoint
                )
            })?;
        assert_transparent_unspent_output_matches_evidence(address_output, evidence);
        unspent_outputs.push(evidence.clone());
    }

    Ok(unspent_outputs)
}

async fn assert_wallet_transparent_output_indexes(
    store: &PrimaryChainStore,
    activations: Arc<NetworkUpgradeActivations>,
    evidence: &TransparentOutputIndexEvidence,
    start_height: BlockHeight,
) -> Result<()> {
    let wallet_query = WalletQuery::new(store.clone(), (), activations);
    let outpoints = evidence
        .transparent_outputs
        .iter()
        .map(|output| output.outpoint)
        .collect::<Vec<_>>();
    let outpoint_response = wallet_query
        .transparent_outputs_by_outpoint(outpoints.clone(), None)
        .await?;
    assert_eq!(outpoint_response.entries.len(), outpoints.len());
    for (expected_outpoint, entry) in outpoints.into_iter().zip(outpoint_response.entries) {
        assert_eq!(entry.outpoint, expected_outpoint);
        let expected =
            find_transparent_output_evidence(&evidence.transparent_outputs, expected_outpoint)?;
        let Some(output) = entry.output else {
            return Err(eyre!(
                "native transparent_outputs_by_outpoint omitted output for {expected_outpoint:?}"
            ));
        };
        assert_eq!(output.value_zat, expected.value_zat);
        assert_eq!(output.script_pub_key, expected.script_pub_key);
    }

    for expected in &evidence.unspent_outputs {
        let address_response = wallet_query
            .transparent_address_unspent_outputs(
                TransparentAddressUnspentOutputsRequest {
                    address_script_hash: expected.address_script_hash,
                    start_height,
                },
                None,
            )
            .await?;
        let address_output = address_response
            .outputs
            .iter()
            .find(|output| output.outpoint == expected.outpoint)
            .ok_or_else(|| {
                eyre!(
                    "native transparent_address_unspent_outputs omitted unspent outpoint {:?}",
                    expected.outpoint
                )
            })?;
        assert_transparent_unspent_output_matches_evidence(address_output, expected);
    }

    Ok(())
}

fn find_transparent_output_evidence(
    evidence: &[TransparentOutputEvidence],
    outpoint: TransparentOutPoint,
) -> Result<&TransparentOutputEvidence> {
    evidence
        .iter()
        .find(|candidate| candidate.outpoint == outpoint)
        .ok_or_else(|| eyre!("missing collected transparent-output evidence for {outpoint:?}"))
}

fn assert_transparent_unspent_output_matches_evidence(
    output: &zinder_core::TransparentUnspentOutput,
    evidence: &TransparentOutputEvidence,
) {
    assert_eq!(output.address_script_hash, evidence.address_script_hash);
    assert_eq!(output.script_pub_key, evidence.script_pub_key);
    assert_eq!(output.outpoint, evidence.outpoint);
    assert_eq!(output.value_zat, evidence.value_zat);
    assert_eq!(output.block_height, evidence.block_height);
    assert_eq!(output.block_hash, evidence.block_hash);
}

async fn assert_wallet_read_errors_below_floor(
    store: &PrimaryChainStore,
    activations: Arc<NetworkUpgradeActivations>,
    height: BlockHeight,
) -> Result<()> {
    let wallet_query = WalletQuery::new(store.clone(), (), Arc::clone(&activations));
    let Err(error) = wallet_query.compact_block_at(height, None).await else {
        return Err(eyre!(
            "native compact_block_at served below wallet-serving floor height {}",
            height.value()
        ));
    };
    assert_compact_block_artifact_unavailable(&error);

    let Err(error) = wallet_query
        .compact_blocks_in_range(BlockHeightRange::inclusive(height, height), None)
        .await
    else {
        return Err(eyre!(
            "native compact_blocks_in_range served below wallet-serving floor height {}",
            height.value()
        ));
    };
    assert_compact_block_artifact_unavailable(&error);

    let grpc_adapter = LightwalletdGrpcAdapter::new(wallet_query, activations);
    let Err(status) = LightwalletdCompactTxStreamer::get_block(
        &grpc_adapter,
        Request::new(lightwalletd::BlockId {
            height: u64::from(height.value()),
            hash: Vec::new(),
        }),
    )
    .await
    else {
        return Err(eyre!(
            "lightwalletd GetBlock served below wallet-serving floor height {}",
            height.value()
        ));
    };
    assert_eq!(status.code(), Code::NotFound);

    let Err(status) = LightwalletdCompactTxStreamer::get_block_range(
        &grpc_adapter,
        Request::new(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: u64::from(height.value()),
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: u64::from(height.value()),
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }),
    )
    .await
    else {
        return Err(eyre!(
            "lightwalletd GetBlockRange served below wallet-serving floor height {}",
            height.value()
        ));
    };
    assert_eq!(status.code(), Code::NotFound);

    Ok(())
}

fn assert_compact_block_artifact_unavailable(error: &QueryError) {
    assert!(
        matches!(
            error,
            QueryError::ArtifactUnavailable {
                family: ArtifactFamily::CompactBlock,
                ..
            }
        ),
        "expected compact-block artifact unavailable error, got {error:?}"
    );
}

struct TransparentOutputIndexEvidence {
    transparent_outputs: Vec<TransparentOutputEvidence>,
    unspent_outputs: Vec<TransparentOutputEvidence>,
}

#[derive(Clone)]
struct TransparentOutputEvidence {
    outpoint: TransparentOutPoint,
    address_script_hash: TransparentAddressScriptHash,
    value_zat: u64,
    script_pub_key: Vec<u8>,
    block_height: BlockHeight,
    block_hash: BlockHash,
}
