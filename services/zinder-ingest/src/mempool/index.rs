//! Live in-memory mempool index owned by `zinder-ingest`.
//!
//! Stores hydrated [`MempoolEntry`] records and the secondary indexes
//! needed by the wallet data plane: transparent outputs by address and
//! transparent spends by outpoint. Mempool state is non-canonical and is
//! never written through `commit_ingest_batch`; the ingest layer holds it
//! exclusively and exposes read methods to the colocated `IngestControl`
//! gRPC handlers.
//!
//! The index is concurrency-safe; readers (snapshot, address-lookup,
//! presence checks) take a read lock, writers ([`MempoolIndex::apply_added`],
//! [`MempoolIndex::apply_invalidated`], [`MempoolIndex::apply_mined`]) take
//! a write lock.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::Arc;

use parking_lot::RwLock;
use zinder_core::{
    MempoolEntry, TransactionId, TransparentAddressScriptHash, TransparentMempoolOutput,
    TransparentMempoolSpend, TransparentOutPoint, TransparentOutput, TransparentOutputEntry,
    UnixTimestampMillis,
};

/// Outcome of applying a single source-observed event to the index.
///
/// Surfaced to the caller so that ingest can decide whether to write a
/// canonical [`zinder_store::MempoolEvent`] envelope (only on a real state
/// change) or treat the source observation as a duplicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MempoolApplyOutcome {
    /// State changed: an entry was inserted, removed, or replaced.
    Applied,
    /// Source observation was a no-op (e.g. duplicate `Added` for an
    /// already-known txid, or `Invalidated`/`Mined` for an unknown txid).
    NoChange,
}

/// Deterministic page of live mempool entries.
#[derive(Clone, Debug)]
pub struct MempoolSnapshotPage {
    /// Entries in transaction-id order.
    pub entries: Vec<Arc<MempoolEntry>>,
    /// Last transaction id included in this page when more entries remain.
    pub next_after_transaction_id: Option<TransactionId>,
    /// Last time this in-memory index observed an applied mempool state change.
    pub last_updated_at: UnixTimestampMillis,
}

/// Concurrent live mempool index.
#[derive(Clone, Debug, Default)]
pub struct MempoolIndex {
    state: Arc<RwLock<MempoolIndexState>>,
}

#[derive(Debug)]
struct MempoolIndexState {
    entries: HashMap<TransactionId, Arc<MempoolEntry>>,
    outputs_by_address: HashMap<
        TransparentAddressScriptHash,
        HashMap<TransparentOutPoint, TransparentMempoolOutput>,
    >,
    output_by_outpoint: HashMap<TransparentOutPoint, TransparentMempoolOutput>,
    spend_by_outpoint: HashMap<TransparentOutPoint, TransactionId>,
    last_updated_at: UnixTimestampMillis,
}

impl Default for MempoolIndexState {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            outputs_by_address: HashMap::new(),
            output_by_outpoint: HashMap::new(),
            spend_by_outpoint: HashMap::new(),
            last_updated_at: UnixTimestampMillis::now(),
        }
    }
}

impl MempoolIndex {
    /// Creates an empty mempool index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a hydrated entry. Returns [`MempoolApplyOutcome::NoChange`]
    /// when the entry's txid is already present; existing entries are
    /// immutable until the source emits an `Invalidated` or `Mined` event
    /// for them.
    ///
    /// The entry is wrapped in an [`Arc`] so subsequent reads return shared
    /// references without re-cloning the entry payload.
    #[must_use]
    pub fn apply_added(&self, entry: MempoolEntry) -> MempoolApplyOutcome {
        let mut state = self.state.write();
        let inserted_entry = match state.entries.entry(entry.transaction_id) {
            Entry::Occupied(_) => {
                drop(state);
                return MempoolApplyOutcome::NoChange;
            }
            Entry::Vacant(slot) => Arc::clone(slot.insert(Arc::new(entry))),
        };
        index_secondary_overlays(&mut state, inserted_entry.as_ref());
        state.last_updated_at = UnixTimestampMillis::now();
        drop(state);
        MempoolApplyOutcome::Applied
    }

    /// Removes the entry for `transaction_id` as if the upstream source
    /// invalidated it.
    #[must_use]
    pub fn apply_invalidated(&self, transaction_id: TransactionId) -> MempoolApplyOutcome {
        self.remove_entry(transaction_id)
    }

    /// Removes the entry for `transaction_id` as if the upstream source
    /// mined it into a block.
    #[must_use]
    pub fn apply_mined(&self, transaction_id: TransactionId) -> MempoolApplyOutcome {
        self.remove_entry(transaction_id)
    }

    fn remove_entry(&self, transaction_id: TransactionId) -> MempoolApplyOutcome {
        let mut state = self.state.write();
        let Some(removed_entry) = state.entries.remove(&transaction_id) else {
            drop(state);
            return MempoolApplyOutcome::NoChange;
        };
        unindex_secondary_overlays(&mut state, removed_entry.as_ref());
        state.last_updated_at = UnixTimestampMillis::now();
        drop(state);
        MempoolApplyOutcome::Applied
    }

    /// Returns whether `transaction_id` is currently in the live index.
    #[must_use]
    pub fn is_in_mempool(&self, transaction_id: TransactionId) -> bool {
        self.state.read().entries.contains_key(&transaction_id)
    }

    /// Returns a shared handle to the entry for `transaction_id`, when
    /// present. The returned [`Arc`] aliases the in-index entry so callers
    /// avoid cloning the underlying payload.
    #[must_use]
    pub fn entry_for(&self, transaction_id: TransactionId) -> Option<Arc<MempoolEntry>> {
        self.state.read().entries.get(&transaction_id).cloned()
    }

    /// Returns the number of entries currently in the index.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.state.read().entries.len()
    }

    /// Returns transparent mempool outputs for `address_script_hash`,
    /// bounded by `max_entries`.
    #[must_use]
    pub fn transparent_outputs_by_address(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        max_entries: u32,
    ) -> Vec<TransparentMempoolOutput> {
        let state = self.state.read();
        let Some(outputs) = state.outputs_by_address.get(&address_script_hash) else {
            return Vec::new();
        };
        let bound = u32_to_usize(max_entries);
        let transparent_outputs = outputs.values().take(bound).cloned().collect::<Vec<_>>();
        drop(state);
        transparent_outputs
    }

    /// Resolves a batch of outpoints against the live mempool index.
    ///
    /// Returns one entry per requested outpoint, in input order. Each entry
    /// carries the resolved transparent output when the outpoint references
    /// a transaction currently in the mempool and the output index is in
    /// bounds; otherwise the entry's `prevout` is `None`.
    #[must_use]
    pub fn transparent_outputs_by_outpoints(
        &self,
        outpoints: &[TransparentOutPoint],
    ) -> Vec<TransparentOutputEntry> {
        let state = self.state.read();
        outpoints
            .iter()
            .map(|outpoint| {
                let prevout = state
                    .output_by_outpoint
                    .get(outpoint)
                    .map(|mempool_output| TransparentOutput {
                        value_zat: mempool_output.value_zat,
                        script_pub_key: mempool_output.script_pub_key.clone(),
                    });
                TransparentOutputEntry {
                    outpoint: *outpoint,
                    output: prevout,
                }
            })
            .collect()
    }

    /// Returns the mempool spend that consumes `spent_outpoint`, when
    /// present.
    #[must_use]
    pub fn transparent_spend_by_outpoint(
        &self,
        spent_outpoint: TransparentOutPoint,
    ) -> Option<TransparentMempoolSpend> {
        let state = self.state.read();
        let outcome = state
            .spend_by_outpoint
            .get(&spent_outpoint)
            .and_then(|spending_transaction_id| state.entries.get(spending_transaction_id))
            .and_then(|entry| {
                entry
                    .transparent_spends
                    .iter()
                    .find(|spend| spend.spent_outpoint == spent_outpoint)
                    .copied()
            });
        drop(state);
        outcome
    }

    /// Returns a snapshot of up to `max_entries` mempool entries.
    ///
    /// Each returned [`Arc`] aliases the in-index entry so callers serialize
    /// the snapshot without re-cloning the underlying payload. The order is
    /// implementation-defined and not stable across snapshots; callers
    /// requiring a deterministic ordering must sort the returned entries by
    /// their `transaction_id`.
    #[must_use]
    pub fn snapshot(&self, max_entries: u32) -> Vec<Arc<MempoolEntry>> {
        self.snapshot_page(max_entries, None).entries
    }

    /// Returns a deterministic page of mempool entries after `after_transaction_id`.
    ///
    /// Pagination is ordered by transaction id so callers can traverse the
    /// current in-memory view without relying on `HashMap` iteration order.
    #[must_use]
    pub fn snapshot_page(
        &self,
        max_entries: u32,
        after_transaction_id: Option<TransactionId>,
    ) -> MempoolSnapshotPage {
        let state = self.state.read();
        let last_updated_at = state.last_updated_at;
        let mut entries = state.entries.values().cloned().collect::<Vec<_>>();
        drop(state);

        entries.sort_by_key(|entry| entry.transaction_id.as_bytes());
        let start_index = after_transaction_id.map_or(0, |transaction_id| {
            let after_bytes = transaction_id.as_bytes();
            entries.partition_point(|entry| entry.transaction_id.as_bytes() <= after_bytes)
        });
        let bound = u32_to_usize(max_entries);
        let end_index = start_index.saturating_add(bound).min(entries.len());
        let has_more = end_index < entries.len();

        // Reuse the sorted vec for the page: trim the tail past end_index,
        // then shift the head out via drain. Avoids the second Vec allocation
        // an `entries[start..end].to_vec()` would force.
        entries.truncate(end_index);
        entries.drain(..start_index);
        let next_after_transaction_id = if has_more {
            entries.last().map(|entry| entry.transaction_id)
        } else {
            None
        };

        MempoolSnapshotPage {
            entries,
            next_after_transaction_id,
            last_updated_at,
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "zinder-core rejects targets with pointer widths below 32 bits"
)]
const fn u32_to_usize(count: u32) -> usize {
    count as usize
}

fn index_secondary_overlays(state: &mut MempoolIndexState, entry: &MempoolEntry) {
    for transparent_output in &entry.transparent_outputs {
        state
            .outputs_by_address
            .entry(transparent_output.address_script_hash)
            .or_default()
            .insert(transparent_output.outpoint, transparent_output.clone());
        state
            .output_by_outpoint
            .insert(transparent_output.outpoint, transparent_output.clone());
    }
    for transparent_spend in &entry.transparent_spends {
        state
            .spend_by_outpoint
            .insert(transparent_spend.spent_outpoint, entry.transaction_id);
    }
}

fn unindex_secondary_overlays(state: &mut MempoolIndexState, entry: &MempoolEntry) {
    for transparent_output in &entry.transparent_outputs {
        if let Some(outputs) = state
            .outputs_by_address
            .get_mut(&transparent_output.address_script_hash)
        {
            outputs.remove(&transparent_output.outpoint);
            if outputs.is_empty() {
                state
                    .outputs_by_address
                    .remove(&transparent_output.address_script_hash);
            }
        }
        state
            .output_by_outpoint
            .remove(&transparent_output.outpoint);
    }
    for transparent_spend in &entry.transparent_spends {
        state
            .spend_by_outpoint
            .remove(&transparent_spend.spent_outpoint);
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::{MempoolApplyOutcome, MempoolIndex};
    use zinder_core::{
        BlockHash, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, MempoolEntry, Network,
        RawTransactionBytes, TransactionId, TransparentAddressScriptHash, TransparentMempoolOutput,
        TransparentMempoolSpend, TransparentOutPoint, UnixTimestampMillis,
    };
    use zinder_store::CURRENT_ARTIFACT_SCHEMA_VERSION;

    fn synthetic_chain_epoch() -> ChainEpoch {
        ChainEpoch {
            id: ChainEpochId::new(7),
            network: Network::ZcashRegtest,
            visible_tip_height: BlockHeight::new(100),
            visible_tip_hash: BlockHash::from_bytes([0x42; 32]),
            settled_tip_height: BlockHeight::new(100),
            settled_tip_hash: BlockHash::from_bytes([0x42; 32]),
            artifact_schema_version: CURRENT_ARTIFACT_SCHEMA_VERSION,
            tip_metadata: ChainTipMetadata::empty(),
            created_at: UnixTimestampMillis::new(1_700_000_000_000),
        }
    }

    fn entry_with_outputs_and_spend(
        transaction_id_byte: u8,
        address_byte: u8,
        spent_outpoint_txid_byte: u8,
    ) -> MempoolEntry {
        let transaction_id = TransactionId::from_bytes([transaction_id_byte; 32]);
        let address_script_hash = TransparentAddressScriptHash::from_bytes([address_byte; 32]);
        MempoolEntry {
            transaction_id,
            auth_digest: None,
            raw_transaction_bytes: RawTransactionBytes::new(vec![transaction_id_byte; 8]),
            compact_transaction_bytes: vec![transaction_id_byte; 4],
            first_seen_unix_millis: UnixTimestampMillis::new(1_700_000_000_000),
            first_seen_chain_epoch: synthetic_chain_epoch(),
            transparent_outputs: vec![TransparentMempoolOutput {
                address_script_hash,
                script_pub_key: vec![address_byte; 25],
                outpoint: TransparentOutPoint::new(transaction_id, 0),
                value_zat: 1_000,
            }],
            transparent_spends: vec![TransparentMempoolSpend {
                spent_outpoint: TransparentOutPoint::new(
                    TransactionId::from_bytes([spent_outpoint_txid_byte; 32]),
                    0,
                ),
                spending_transaction_id: transaction_id,
            }],
        }
    }

    #[test]
    fn apply_added_inserts_entry_and_secondary_indexes() {
        let index = MempoolIndex::new();
        let entry = entry_with_outputs_and_spend(0x10, 0xAA, 0x20);

        let outcome = index.apply_added(entry.clone());

        assert_eq!(outcome, MempoolApplyOutcome::Applied);
        assert!(index.is_in_mempool(entry.transaction_id));
        assert_eq!(index.entry_count(), 1);

        let outputs = index.transparent_outputs_by_address(
            TransparentAddressScriptHash::from_bytes([0xAA; 32]),
            10,
        );
        assert_eq!(outputs.len(), 1);

        let spend = index.transparent_spend_by_outpoint(TransparentOutPoint::new(
            TransactionId::from_bytes([0x20; 32]),
            0,
        ));
        assert!(spend.is_some());
    }

    #[test]
    fn apply_added_is_idempotent_for_duplicate_txid() {
        let index = MempoolIndex::new();
        let entry = entry_with_outputs_and_spend(0x10, 0xAA, 0x20);
        assert_eq!(
            index.apply_added(entry.clone()),
            MempoolApplyOutcome::Applied
        );

        let outcome = index.apply_added(entry);

        assert_eq!(outcome, MempoolApplyOutcome::NoChange);
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn apply_invalidated_removes_entry_and_secondary_indexes() {
        let index = MempoolIndex::new();
        let entry = entry_with_outputs_and_spend(0x10, 0xAA, 0x20);
        let _ = index.apply_added(entry.clone());

        let outcome = index.apply_invalidated(entry.transaction_id);

        assert_eq!(outcome, MempoolApplyOutcome::Applied);
        assert!(!index.is_in_mempool(entry.transaction_id));
        assert!(
            index
                .transparent_outputs_by_address(
                    TransparentAddressScriptHash::from_bytes([0xAA; 32]),
                    10,
                )
                .is_empty()
        );
        assert!(
            index
                .transparent_spend_by_outpoint(TransparentOutPoint::new(
                    TransactionId::from_bytes([0x20; 32]),
                    0,
                ))
                .is_none()
        );
    }

    #[test]
    fn apply_mined_returns_no_change_for_unknown_txid() {
        let index = MempoolIndex::new();
        let outcome = index.apply_mined(TransactionId::from_bytes([0xFF; 32]));
        assert_eq!(outcome, MempoolApplyOutcome::NoChange);
    }

    #[test]
    fn snapshot_respects_max_entries_bound() {
        let index = MempoolIndex::new();
        for index_byte in 0u8..5 {
            let _ = index.apply_added(entry_with_outputs_and_spend(index_byte, 0xAA, 0x20));
        }
        assert_eq!(index.snapshot(2).len(), 2);
        assert_eq!(index.snapshot(10).len(), 5);
    }

    #[test]
    fn transparent_outputs_lookup_returns_empty_for_unknown_address() {
        let index = MempoolIndex::new();
        let outputs = index.transparent_outputs_by_address(
            TransparentAddressScriptHash::from_bytes([0x99; 32]),
            10,
        );
        assert!(outputs.is_empty());
    }
}
