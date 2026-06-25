//! In-place store schema migration run at primary open.
//!
//! Version 11 converts `address_output_index` from epoch-suffixed
//! append-only history into a reorg-safe current projection. The projection
//! is fully derivable from `transparent_output` joined with
//! `transparent_spend_fact` (both keyed by outpoint), so a version-10 store
//! is rebuilt in place: drop the old family, stream a sorted merge join of
//! the two source families into the fresh one, and delete finalized-spent
//! rows from both sources to match the safe-tip retention invariant.
//!
//! The rebuild is idempotent. The store metadata version flips to 11 only
//! in the final batch, after every projection row is durable; a crash at
//! any earlier point leaves the metadata at 10 and the next open re-runs
//! the rebuild from the unchanged source families.

use std::time::Instant;

use zinder_core::{
    BlockHeight, ChainEpoch, Network, TransparentOutPoint, TransparentOutputArtifact,
};

use crate::{
    ArtifactFamily, StoreError,
    format::{
        StoreKey, decode_transparent_output_artifact, decode_transparent_spend_fact,
        encode_address_output_index_artifact, encode_chain_epoch,
    },
    kv::{MergedTableRow, RocksChainStore, StorageDelete, StoragePut, StorageTable},
};

use super::{
    CURRENT_ARTIFACT_SCHEMA_VERSION, REBUILDABLE_STORE_SCHEMA_VERSION, STORE_SCHEMA_VERSION,
    address_output_row, decode_store_metadata, encode_store_metadata, read_chain_epoch,
    read_current_chain_epoch_id, transparent_retention_swept_height_put,
};

const REBUILD_WRITE_CHUNK_ROWS: usize = 4096;

pub(super) fn migrate_primary_store_schema(inner: &RocksChainStore) -> Result<(), StoreError> {
    let key = StoreKey::store_metadata();
    let Some(metadata_bytes) = inner.get(StorageTable::StorageControl, &key)? else {
        return Ok(());
    };
    let metadata = decode_store_metadata(&key, &metadata_bytes)?;
    if metadata.schema_version == STORE_SCHEMA_VERSION {
        return Ok(());
    }
    if metadata.schema_version != REBUILDABLE_STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaMismatch {
            persisted_version: metadata.schema_version,
            expected_version: STORE_SCHEMA_VERSION,
        });
    }

    rebuild_address_output_projection(inner, metadata.network)
}

fn rebuild_address_output_projection(
    inner: &RocksChainStore,
    network: Network,
) -> Result<(), StoreError> {
    let started_at = Instant::now();
    let _control_guard = inner.lock_control();
    let current_chain_epoch = read_current_chain_epoch_id(inner)?
        .map(|chain_epoch_id| read_chain_epoch(inner, chain_epoch_id))
        .transpose()?;
    let safe_tip_height = current_chain_epoch.map_or(BlockHeight::new(0), |chain_epoch| {
        chain_epoch.settled_tip_height
    });

    tracing::info!(
        target: "zinder::store",
        event = "address_output_projection_rebuild_started",
        from_schema_version = REBUILDABLE_STORE_SCHEMA_VERSION,
        to_schema_version = STORE_SCHEMA_VERSION,
        safe_tip_height = safe_tip_height.value(),
        "rebuilding the address-output projection in place"
    );

    inner.recreate_column_family(StorageTable::AddressOutputIndex)?;

    let mut rebuild = ProjectionRebuild::new(network, safe_tip_height);
    inner.scan_tables_merged_by_key_suffix(
        StorageTable::TransparentOutput,
        StorageTable::TransparentSpendFact,
        &mut |merged_row| {
            rebuild.apply(merged_row)?;
            rebuild.flush_full_chunk(inner)
        },
    )?;
    rebuild.finalize(inner, current_chain_epoch)?;

    tracing::info!(
        target: "zinder::store",
        event = "address_output_projection_rebuild_completed",
        projected_rows = rebuild.projected_rows,
        swept_outpoints = rebuild.swept_outpoints,
        duration_seconds = started_at.elapsed().as_secs_f64(),
        "address-output projection rebuild completed"
    );

    Ok(())
}

struct ProjectionRebuild {
    network: Network,
    safe_tip_height: BlockHeight,
    pending_puts: Vec<StoragePut>,
    pending_deletes: Vec<StorageDelete>,
    projected_rows: u64,
    swept_outpoints: u64,
}

impl ProjectionRebuild {
    fn new(network: Network, safe_tip_height: BlockHeight) -> Self {
        Self {
            network,
            safe_tip_height,
            pending_puts: Vec::new(),
            pending_deletes: Vec::new(),
            projected_rows: 0,
            swept_outpoints: 0,
        }
    }

    fn apply(&mut self, merged_row: MergedTableRow<'_>) -> Result<(), StoreError> {
        match merged_row {
            MergedTableRow::LeftOnly {
                key,
                value: envelope_bytes,
            } => {
                let output = decode_output_row(self.network, key, envelope_bytes)?;
                self.project(&output)?;
            }
            MergedTableRow::RightOnly {
                key,
                value: envelope_bytes,
            } => {
                // A spend fact without its output row: the spent output
                // predates the store's bootstrap checkpoint, so no address
                // row is derivable. Finalized rows still sweep.
                let outpoint = outpoint_from_key(ArtifactFamily::TransparentSpendFact, key)?;
                let spend_key = StoreKey::transparent_spend_fact(self.network, outpoint);
                let spend = decode_transparent_spend_fact(&spend_key, envelope_bytes, outpoint)?;
                if spend.block_height <= self.safe_tip_height {
                    self.pending_deletes.push(StorageDelete {
                        table: StorageTable::TransparentSpendFact,
                        key: spend_key,
                    });
                    self.swept_outpoints = self.swept_outpoints.saturating_add(1);
                }
            }
            MergedTableRow::Matched {
                left_key,
                left_value,
                right_value,
            } => {
                let output = decode_output_row(self.network, left_key, left_value)?;
                let spend_key = StoreKey::transparent_spend_fact(self.network, output.outpoint);
                let spend =
                    decode_transparent_spend_fact(&spend_key, right_value, output.outpoint)?;
                if spend.block_height <= self.safe_tip_height {
                    self.pending_deletes.push(StorageDelete {
                        table: StorageTable::TransparentOutput,
                        key: StoreKey::transparent_output(self.network, output.outpoint),
                    });
                    self.pending_deletes.push(StorageDelete {
                        table: StorageTable::TransparentSpendFact,
                        key: spend_key,
                    });
                    self.swept_outpoints = self.swept_outpoints.saturating_add(1);
                } else {
                    self.project(&output)?;
                }
            }
        }

        Ok(())
    }

    fn project(&mut self, output: &TransparentOutputArtifact) -> Result<(), StoreError> {
        self.pending_puts
            .push(address_row_put(self.network, output)?);
        self.projected_rows = self.projected_rows.saturating_add(1);
        Ok(())
    }

    fn flush_full_chunk(&mut self, inner: &RocksChainStore) -> Result<(), StoreError> {
        if self
            .pending_puts
            .len()
            .saturating_add(self.pending_deletes.len())
            >= REBUILD_WRITE_CHUNK_ROWS
        {
            inner.write_batch(
                std::mem::take(&mut self.pending_puts),
                std::mem::take(&mut self.pending_deletes),
            )?;
        }

        Ok(())
    }

    /// Writes the remaining rows together with the version-11 store
    /// metadata, the retention marker, and the migrated chain epoch. This
    /// batch is what makes the rebuild durable; everything before it can be
    /// re-run after a crash.
    fn finalize(
        &mut self,
        inner: &RocksChainStore,
        current_chain_epoch: Option<ChainEpoch>,
    ) -> Result<(), StoreError> {
        self.pending_puts.push(StoragePut {
            table: StorageTable::StorageControl,
            key: StoreKey::store_metadata(),
            value: encode_store_metadata(self.network),
        });
        self.pending_puts
            .push(transparent_retention_swept_height_put(self.safe_tip_height));
        if let Some(chain_epoch) = current_chain_epoch {
            let migrated_chain_epoch = ChainEpoch {
                artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
                ..chain_epoch
            };
            self.pending_puts.push(StoragePut {
                table: StorageTable::ChainEpoch,
                key: StoreKey::chain_epoch(migrated_chain_epoch.id),
                value: encode_chain_epoch(&migrated_chain_epoch),
            });
        }

        inner.write_batch(
            std::mem::take(&mut self.pending_puts),
            std::mem::take(&mut self.pending_deletes),
        )
    }
}

fn decode_output_row(
    network: Network,
    key_bytes: &[u8],
    envelope_bytes: &[u8],
) -> Result<TransparentOutputArtifact, StoreError> {
    let outpoint = outpoint_from_key(ArtifactFamily::TransparentOutput, key_bytes)?;
    let key = StoreKey::transparent_output(network, outpoint);
    decode_transparent_output_artifact(&key, envelope_bytes, outpoint)
}

fn outpoint_from_key(
    family: ArtifactFamily,
    key_bytes: &[u8],
) -> Result<TransparentOutPoint, StoreError> {
    StoreKey::transparent_outpoint_from_key(key_bytes).ok_or(StoreError::ArtifactCorrupt {
        family,
        key: StoreKey::from_raw_bytes(key_bytes).into(),
        reason: "transparent artifact key is malformed",
    })
}

fn address_row_put(
    network: Network,
    output: &TransparentOutputArtifact,
) -> Result<StoragePut, StoreError> {
    Ok(StoragePut {
        table: StorageTable::AddressOutputIndex,
        key: StoreKey::address_output_index(
            network,
            output.address_script_hash,
            output.block_height,
            output.outpoint,
        ),
        value: encode_address_output_index_artifact(address_output_row(output))?,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, BlockHeaderArtifact, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata,
        CompactBlockArtifact, TransactionId, TransparentAddressScriptHash, TransparentSpendFact,
        UnixTimestampMillis,
    };

    use crate::{
        ChainEpochArtifacts, ChainStoreOptions, PrimaryChainStore,
        chain_store::read_transparent_retention_swept_height, kv::PrefixScanControl,
    };

    use super::*;

    const NETWORK: Network = Network::ZcashRegtest;
    const ADDRESS_SCRIPT_HASH: TransparentAddressScriptHash =
        TransparentAddressScriptHash::from_bytes([61; 32]);

    #[test]
    fn v10_store_rebuilds_to_the_exact_current_projection() -> Result<(), Box<dyn Error>> {
        let tempdir = tempdir()?;
        let unspent_output = synthetic_output(BlockHeight::new(1), [1; 32]);
        let finalized_spent_output = synthetic_output(BlockHeight::new(1), [2; 32]);
        let in_window_spent_output = synthetic_output(BlockHeight::new(1), [3; 32]);
        let outputs = [
            unspent_output.clone(),
            finalized_spent_output.clone(),
            in_window_spent_output.clone(),
        ];

        {
            let store =
                PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
            store.commit_chain_epoch(
                synthetic_epoch_artifacts()
                    .with_transparent_outputs_by_outpoint(outputs.to_vec())
                    .with_transparent_spend_facts(vec![
                        synthetic_spend(BlockHeight::new(2), &finalized_spent_output),
                        synthetic_spend(BlockHeight::new(5), &in_window_spent_output),
                    ]),
            )?;
            downgrade_to_v10_layout(&store, &outputs)?;
        }

        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        assert_projection_matches_expected(
            &store,
            &unspent_output,
            &finalized_spent_output,
            &in_window_spent_output,
        )?;

        Ok(())
    }

    #[test]
    fn rebuild_reruns_to_the_same_state_after_a_crash_before_finalize() -> Result<(), Box<dyn Error>>
    {
        let tempdir = tempdir()?;
        let unspent_output = synthetic_output(BlockHeight::new(1), [1; 32]);
        let finalized_spent_output = synthetic_output(BlockHeight::new(1), [2; 32]);
        let in_window_spent_output = synthetic_output(BlockHeight::new(1), [3; 32]);
        let outputs = [
            unspent_output.clone(),
            finalized_spent_output.clone(),
            in_window_spent_output.clone(),
        ];

        {
            let store =
                PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
            store.commit_chain_epoch(
                synthetic_epoch_artifacts()
                    .with_transparent_outputs_by_outpoint(outputs.to_vec())
                    .with_transparent_spend_facts(vec![
                        synthetic_spend(BlockHeight::new(2), &finalized_spent_output),
                        synthetic_spend(BlockHeight::new(5), &in_window_spent_output),
                    ]),
            )?;
            downgrade_to_v10_layout(&store, &outputs)?;
        }

        {
            let store =
                PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
            // Simulate a crash that lost only the metadata flip: a rebuild
            // that already swept and projected must re-run to the same end
            // state from the unchanged source families.
            store.store.inner.write(vec![StoragePut {
                table: StorageTable::StorageControl,
                key: StoreKey::store_metadata(),
                value: encode_store_metadata_with_version(
                    REBUILDABLE_STORE_SCHEMA_VERSION,
                    NETWORK,
                ),
            }])?;
        }

        let store = PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())?;
        assert_projection_matches_expected(
            &store,
            &unspent_output,
            &finalized_spent_output,
            &in_window_spent_output,
        )?;

        Ok(())
    }

    /// Rewrites the store into the version-10 shape: epoch-suffixed
    /// append-only address rows (with a duplicated row from a second epoch)
    /// and version-10 store metadata.
    fn downgrade_to_v10_layout(
        store: &PrimaryChainStore,
        outputs: &[TransparentOutputArtifact],
    ) -> Result<(), StoreError> {
        store
            .store
            .inner
            .recreate_column_family(StorageTable::AddressOutputIndex)?;
        let mut puts = Vec::new();
        for output in outputs {
            puts.push(v10_address_row_put(output, ChainEpochId::new(1))?);
        }
        puts.push(v10_address_row_put(&outputs[0], ChainEpochId::new(2))?);
        puts.push(StoragePut {
            table: StorageTable::StorageControl,
            key: StoreKey::store_metadata(),
            value: encode_store_metadata_with_version(REBUILDABLE_STORE_SCHEMA_VERSION, NETWORK),
        });
        store.store.inner.write(puts)
    }

    fn v10_address_row_put(
        output: &TransparentOutputArtifact,
        chain_epoch: ChainEpochId,
    ) -> Result<StoragePut, StoreError> {
        let mut key_bytes = StoreKey::address_output_index(
            NETWORK,
            output.address_script_hash,
            output.block_height,
            output.outpoint,
        )
        .into_bytes();
        key_bytes.extend_from_slice(&chain_epoch.value().to_be_bytes());
        Ok(StoragePut {
            table: StorageTable::AddressOutputIndex,
            key: StoreKey::from_raw_bytes(&key_bytes),
            value: encode_address_output_index_artifact(address_output_row(output))?,
        })
    }

    fn encode_store_metadata_with_version(schema_version: u16, network: Network) -> Vec<u8> {
        let mut metadata = Vec::with_capacity(6);
        metadata.extend_from_slice(&schema_version.to_be_bytes());
        metadata.extend_from_slice(&network.id().to_be_bytes());
        metadata
    }

    fn assert_projection_matches_expected(
        store: &PrimaryChainStore,
        unspent_output: &TransparentOutputArtifact,
        finalized_spent_output: &TransparentOutputArtifact,
        in_window_spent_output: &TransparentOutputArtifact,
    ) -> Result<(), Box<dyn Error>> {
        let address_keys = raw_address_row_keys(store)?;
        let expected_keys = [
            StoreKey::address_output_index(
                NETWORK,
                unspent_output.address_script_hash,
                unspent_output.block_height,
                unspent_output.outpoint,
            )
            .into_bytes(),
            StoreKey::address_output_index(
                NETWORK,
                in_window_spent_output.address_script_hash,
                in_window_spent_output.block_height,
                in_window_spent_output.outpoint,
            )
            .into_bytes(),
        ];
        let mut expected_keys = expected_keys.to_vec();
        expected_keys.sort();
        assert_eq!(address_keys, expected_keys);

        let reader = store.current_chain_epoch_reader()?;
        let outpoints = [
            unspent_output.outpoint,
            finalized_spent_output.outpoint,
            in_window_spent_output.outpoint,
        ];
        let resolved_outputs = reader.transparent_outputs_by_outpoints(&outpoints)?;
        assert!(resolved_outputs.contains_key(&unspent_output.outpoint));
        assert!(!resolved_outputs.contains_key(&finalized_spent_output.outpoint));
        assert!(resolved_outputs.contains_key(&in_window_spent_output.outpoint));

        let resolved_spends = reader.transparent_spend_facts_by_outpoints(&outpoints)?;
        assert!(!resolved_spends.contains_key(&finalized_spent_output.outpoint));
        assert!(resolved_spends.contains_key(&in_window_spent_output.outpoint));

        assert_eq!(
            read_transparent_retention_swept_height(store.store.inner.as_ref())?,
            Some(BlockHeight::new(3))
        );

        Ok(())
    }

    fn raw_address_row_keys(store: &PrimaryChainStore) -> Result<Vec<Vec<u8>>, StoreError> {
        let prefix = StoreKey::address_output_index_prefix(NETWORK, ADDRESS_SCRIPT_HASH);
        let mut keys = Vec::new();
        store.store.inner.scan_prefix(
            StorageTable::AddressOutputIndex,
            &prefix,
            &mut |key_bytes, _| {
                keys.push(key_bytes.to_vec());
                Ok(PrefixScanControl::Continue)
            },
        )?;
        Ok(keys)
    }

    fn synthetic_epoch_artifacts() -> ChainEpochArtifacts {
        let blocks = (1..=5).map(synthetic_block).collect::<Vec<_>>();
        let compact_blocks = (1..=5)
            .map(|height| {
                CompactBlockArtifact::new(
                    BlockHeight::new(height),
                    block_hash(height),
                    format!("rebuild-compact-{height}").into_bytes(),
                )
            })
            .collect();
        let chain_epoch = ChainEpoch {
            id: ChainEpochId::new(1),
            network: NETWORK,
            visible_tip_height: BlockHeight::new(5),
            visible_tip_hash: block_hash(5),
            settled_tip_height: BlockHeight::new(3),
            settled_tip_hash: block_hash(3),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_774_668_300_000),
        };

        ChainEpochArtifacts::new(chain_epoch, blocks, compact_blocks)
    }

    fn synthetic_block(height: u32) -> BlockHeaderArtifact {
        BlockHeaderArtifact::new(
            BlockHeight::new(height),
            block_hash(height),
            block_hash(height.saturating_sub(1)),
            [0; 32],
            [0; 32],
            0,
            0,
            [0; 32],
            0,
            32,
        )
    }

    fn synthetic_output(height: BlockHeight, txid_seed: [u8; 32]) -> TransparentOutputArtifact {
        TransparentOutputArtifact::new(
            TransparentOutPoint::new(TransactionId::from_bytes(txid_seed), 0),
            42_000,
            b"rebuild-script".to_vec(),
            ADDRESS_SCRIPT_HASH,
            height,
            block_hash(height.value()),
        )
    }

    fn synthetic_spend(
        height: BlockHeight,
        output: &TransparentOutputArtifact,
    ) -> TransparentSpendFact {
        let mut spending_txid = output.outpoint.transaction_id.as_bytes();
        spending_txid[0] ^= 0xff;
        TransparentSpendFact::from_input_and_output(
            output.outpoint,
            0,
            TransactionId::from_bytes(spending_txid),
            0,
            height,
            block_hash(height.value()),
            output,
        )
    }

    fn block_hash(seed: u32) -> BlockHash {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&seed.to_be_bytes());
        }
        BlockHash::from_bytes(bytes)
    }
}
