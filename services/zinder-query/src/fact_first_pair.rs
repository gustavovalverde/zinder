//! Immutable, exact-fence read pairs for fact-first wallet serving.

use std::{fmt, io, num::NonZeroU16, sync::Arc};

use thiserror::Error;
use zinder_core::{
    BlockHeaderArtifact, BlockHeight, BlockHeightRange, BlockId, ChainEpoch,
    CommitmentTreeCheckpoint, CompactBlockArtifact, Network, SubtreeRootArtifact, SubtreeRootRange,
    TransactionBlobArtifact, TransactionId, TransactionLocation, TransparentAddressScriptHash,
};
use zinder_store::{CanonicalEventFence, CanonicalStoreError, RocksDbCanonicalSecondary};
use zinder_wallet_projection::{
    WalletAddressTransactionKey, WalletAddressUnspentOutputKey, WalletCanonicalSourceIdentity,
    WalletProjectionReadyEvidence, WalletProjectionSourcePosition,
};
use zinder_wallet_rocksdb::{
    RocksDbWalletError, RocksDbWalletSecondary, WalletAddressTransactionHistoryPage,
    WalletAddressUnspentOutputsPage,
};

use crate::QueryError;

/// Read-only canonical facts held at one immutable admitted fence.
///
/// Implementations must never catch up or otherwise mutate their observed
/// fence while a caller holds the same instance in a [`FactFirstReadPair`].
pub trait FactFirstCanonicalRead: Send + Sync + 'static {
    /// Returns the immutable network admitted by this canonical reader.
    fn network(&self) -> Network;

    /// Returns the exact admitted canonical event fence.
    fn event_fence(&self) -> CanonicalEventFence;

    /// Reads the visible chain epoch at the admitted fence.
    fn chain_epoch(&self) -> Result<ChainEpoch, CanonicalStoreError>;

    /// Reads one retained canonical epoch by its exact identifier.
    fn chain_epoch_at(
        &self,
        epoch_id: zinder_core::ChainEpochId,
    ) -> Result<ChainEpoch, CanonicalStoreError>;

    /// Reads one canonical header by height.
    fn block_header_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHeaderArtifact>, CanonicalStoreError>;

    /// Reads one compact block by height.
    fn compact_block_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<CompactBlockArtifact>, CanonicalStoreError>;

    /// Reads an inclusive compact-block range.
    fn compact_blocks_in_range(
        &self,
        range: BlockHeightRange,
    ) -> Result<Vec<CompactBlockArtifact>, CanonicalStoreError>;

    /// Reads the canonical location of a transaction.
    fn transaction_location(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionLocation>, CanonicalStoreError>;

    /// Reads a raw transaction at an authenticated canonical location.
    fn transaction_blob(
        &self,
        location: TransactionLocation,
    ) -> Result<Option<TransactionBlobArtifact>, CanonicalStoreError>;

    /// Reads the newest commitment-tree checkpoint at or below a height.
    fn tree_state_checkpoint_at_or_before(
        &self,
        height: BlockHeight,
    ) -> Result<Option<CommitmentTreeCheckpoint>, CanonicalStoreError>;

    /// Reads a contiguous range of subtree roots.
    fn subtree_roots(
        &self,
        range: SubtreeRootRange,
    ) -> Result<Vec<SubtreeRootArtifact>, CanonicalStoreError>;
}

/// Read-only wallet facts held at one immutable READY source identity.
///
/// Implementations must never follow a primary or mutate their observed READY
/// evidence while a caller holds the instance in a [`FactFirstReadPair`].
pub trait FactFirstWalletRead: Send + Sync + 'static {
    /// Returns the immutable wallet network admitted from store control.
    fn network(&self) -> Network;

    /// Returns the complete READY evidence bound to this reader.
    fn ready_evidence(&self) -> &WalletProjectionReadyEvidence;

    /// Reads one bounded page of unspent outputs for an address.
    fn address_unspent_outputs_page(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        after: Option<WalletAddressUnspentOutputKey>,
        page_size: NonZeroU16,
    ) -> Result<WalletAddressUnspentOutputsPage, RocksDbWalletError>;

    /// Reads one bounded page of address-touching transaction history.
    fn address_transaction_history_page(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        after: Option<WalletAddressTransactionKey>,
        page_size: NonZeroU16,
    ) -> Result<WalletAddressTransactionHistoryPage, RocksDbWalletError>;

    /// Reads one address balance, treating an absent row as zero.
    fn address_balance(
        &self,
        address_script_hash: TransparentAddressScriptHash,
    ) -> Result<u64, RocksDbWalletError>;
}

/// Typed reason why canonical and wallet readers cannot form one serving pair.
///
/// This error is intentionally distinct from [`QueryError`]. Runtime pair
/// managers use it to classify replica lag, wallet projection lag, and
/// malformed admitted evidence before deciding readiness. Query construction
/// maps it to a fail-closed request error.
#[derive(Debug, Error)]
pub enum FactFirstPairAdmissionError {
    /// The independently admitted readers committed different networks.
    #[error("canonical and wallet readers have different admitted networks")]
    NetworkMismatch {
        /// Network committed by the canonical reader.
        canonical: Network,
        /// Network committed by the wallet reader.
        wallet: Network,
    },
    /// The canonical reader could not decode its visible epoch.
    #[error("canonical reader failed while validating a fact-first pair")]
    CanonicalRead {
        /// Exact canonical storage failure.
        #[source]
        source: CanonicalStoreError,
    },
    /// The visible canonical epoch disagreed with the canonical READY fence.
    #[error("canonical READY epoch does not match its admitted event fence")]
    CanonicalFenceMismatch,
    /// The wallet READY identity did not exactly match canonical state.
    #[error("wallet READY evidence does not match the canonical fence and settlement boundary")]
    WalletSourceMismatch {
        /// Identity reconstructed from canonical READY state.
        canonical: Box<WalletCanonicalSourceIdentity>,
        /// Identity committed by wallet READY evidence.
        wallet: Box<WalletCanonicalSourceIdentity>,
    },
}

macro_rules! impl_canonical_read {
    ($store:ty) => {
        impl FactFirstCanonicalRead for $store {
            fn network(&self) -> Network {
                self.network()
            }

            fn event_fence(&self) -> CanonicalEventFence {
                self.event_fence()
            }

            fn chain_epoch(&self) -> Result<ChainEpoch, CanonicalStoreError> {
                self.chain_epoch()
            }

            fn chain_epoch_at(
                &self,
                epoch_id: zinder_core::ChainEpochId,
            ) -> Result<ChainEpoch, CanonicalStoreError> {
                self.chain_epoch_at(epoch_id)
            }

            fn block_header_at(
                &self,
                height: BlockHeight,
            ) -> Result<Option<BlockHeaderArtifact>, CanonicalStoreError> {
                self.block_header_at(height)
            }

            fn compact_block_at(
                &self,
                height: BlockHeight,
            ) -> Result<Option<CompactBlockArtifact>, CanonicalStoreError> {
                self.compact_block_at(height)
            }

            fn compact_blocks_in_range(
                &self,
                range: BlockHeightRange,
            ) -> Result<Vec<CompactBlockArtifact>, CanonicalStoreError> {
                self.compact_blocks_in_range(range)
            }

            fn transaction_location(
                &self,
                transaction_id: TransactionId,
            ) -> Result<Option<TransactionLocation>, CanonicalStoreError> {
                self.transaction_location(transaction_id)
            }

            fn transaction_blob(
                &self,
                location: TransactionLocation,
            ) -> Result<Option<TransactionBlobArtifact>, CanonicalStoreError> {
                self.transaction_blob(location)
            }

            fn tree_state_checkpoint_at_or_before(
                &self,
                height: BlockHeight,
            ) -> Result<Option<CommitmentTreeCheckpoint>, CanonicalStoreError> {
                self.tree_state_checkpoint_at_or_before(height)
            }

            fn subtree_roots(
                &self,
                range: SubtreeRootRange,
            ) -> Result<Vec<SubtreeRootArtifact>, CanonicalStoreError> {
                self.subtree_roots(range)
            }
        }
    };
}

impl_canonical_read!(RocksDbCanonicalSecondary);

impl FactFirstWalletRead for RocksDbWalletSecondary {
    fn network(&self) -> Network {
        self.network()
    }

    fn ready_evidence(&self) -> &WalletProjectionReadyEvidence {
        self.ready_evidence()
    }

    fn address_unspent_outputs_page(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        after: Option<WalletAddressUnspentOutputKey>,
        page_size: NonZeroU16,
    ) -> Result<WalletAddressUnspentOutputsPage, RocksDbWalletError> {
        self.address_unspent_outputs_page(address_script_hash, after, page_size)
    }

    fn address_transaction_history_page(
        &self,
        address_script_hash: TransparentAddressScriptHash,
        after: Option<WalletAddressTransactionKey>,
        page_size: NonZeroU16,
    ) -> Result<WalletAddressTransactionHistoryPage, RocksDbWalletError> {
        self.address_transaction_history_page(address_script_hash, after, page_size)
    }

    fn address_balance(
        &self,
        address_script_hash: TransparentAddressScriptHash,
    ) -> Result<u64, RocksDbWalletError> {
        self.address_balance(address_script_hash)
    }
}

/// One immutable canonical and wallet reader pair at the exact same source.
///
/// Pair construction validates network, epoch, event cursor, visible tip,
/// sequence digest, and settled tip. Callers can safely hold an `Arc` to this
/// pair for an entire request while a different generation catches up.
pub struct FactFirstReadPair {
    canonical: Arc<dyn FactFirstCanonicalRead>,
    wallet: Arc<dyn FactFirstWalletRead>,
    canonical_fence: CanonicalEventFence,
    wallet_source: WalletCanonicalSourceIdentity,
}

impl FactFirstReadPair {
    /// Creates an immutable pair only when both readers prove one exact source.
    pub fn new(
        canonical: Arc<dyn FactFirstCanonicalRead>,
        wallet: Arc<dyn FactFirstWalletRead>,
    ) -> Result<Self, QueryError> {
        Self::validate_readers(canonical.as_ref(), wallet.as_ref())
            .map_err(|error| pair_admission_query_error(&error))?;
        Ok(Self {
            canonical_fence: canonical.event_fence(),
            wallet_source: WalletCanonicalSourceIdentity::from_ready_evidence(
                wallet.ready_evidence(),
            ),
            canonical,
            wallet,
        })
    }

    /// Validates whether two immutable readers represent one exact source.
    ///
    /// Candidate-pair managers use this before publication. It compares the
    /// admitted network, epoch, event cursor, visible tip, sequence digest,
    /// and settlement boundary without retaining either reader.
    pub fn validate_readers(
        canonical: &(dyn FactFirstCanonicalRead + 'static),
        wallet: &(dyn FactFirstWalletRead + 'static),
    ) -> Result<(), FactFirstPairAdmissionError> {
        if canonical.network() != wallet.network() {
            return Err(FactFirstPairAdmissionError::NetworkMismatch {
                canonical: canonical.network(),
                wallet: wallet.network(),
            });
        }
        let canonical_fence = canonical.event_fence();
        let canonical_epoch = canonical
            .chain_epoch()
            .map_err(|source| FactFirstPairAdmissionError::CanonicalRead { source })?;
        let canonical_visible_tip = BlockId::new(
            canonical_epoch.visible_tip_height,
            canonical_epoch.visible_tip_hash,
        );
        if canonical_epoch.network != canonical.network()
            || canonical_epoch.id != canonical_fence.chain_epoch_id()
            || canonical_visible_tip != canonical_fence.visible_tip()
        {
            return Err(FactFirstPairAdmissionError::CanonicalFenceMismatch);
        }
        let canonical_source = WalletCanonicalSourceIdentity::new(
            WalletProjectionSourcePosition::new(
                canonical_fence.chain_epoch_id(),
                canonical_fence.visible_tip(),
                canonical_fence.chain_event_sequence(),
            ),
            canonical_fence.sequence_digest(),
            BlockId::new(
                canonical_epoch.settled_tip_height,
                canonical_epoch.settled_tip_hash,
            ),
        );
        let wallet_source =
            WalletCanonicalSourceIdentity::from_ready_evidence(wallet.ready_evidence());
        if wallet_source != canonical_source {
            return Err(FactFirstPairAdmissionError::WalletSourceMismatch {
                canonical: Box::new(canonical_source),
                wallet: Box::new(wallet_source),
            });
        }
        Ok(())
    }

    /// Returns the canonical reader frozen for this pair's entire lifetime.
    #[must_use]
    pub fn canonical(&self) -> &(dyn FactFirstCanonicalRead + 'static) {
        self.canonical.as_ref()
    }

    /// Returns the wallet reader frozen for this pair's entire lifetime.
    #[must_use]
    pub fn wallet(&self) -> &(dyn FactFirstWalletRead + 'static) {
        self.wallet.as_ref()
    }

    /// Returns the exact canonical fence admitted for this pair.
    #[must_use]
    pub const fn canonical_fence(&self) -> CanonicalEventFence {
        self.canonical_fence
    }

    /// Returns the exact wallet source admitted for this pair.
    #[must_use]
    pub const fn wallet_source(&self) -> WalletCanonicalSourceIdentity {
        self.wallet_source
    }
}

impl fmt::Debug for FactFirstReadPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FactFirstReadPair")
            .field("canonical_fence", &self.canonical_fence)
            .field("wallet_source", &self.wallet_source)
            .finish_non_exhaustive()
    }
}

fn pair_admission_query_error(error: &FactFirstPairAdmissionError) -> QueryError {
    QueryError::WalletProjectionRead {
        source: Box::new(io::Error::other(error.to_string())),
    }
}
