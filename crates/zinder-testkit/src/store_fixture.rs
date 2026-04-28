//! Tempdir-backed [`PrimaryChainStore`] fixtures for tests.

use std::path::Path;

use tempfile::TempDir;
use zinder_core::{ChainEpoch, ChainEpochId, Network};
use zinder_store::{ChainStoreOptions, PrimaryChainStore, StoreError};

use crate::chain_fixture::ChainFixture;

/// Owns a [`tempfile::TempDir`] together with a [`PrimaryChainStore`] opened on it.
///
/// The temporary directory is deleted when the fixture is dropped. Use
/// [`StoreFixture::open`] for an empty store and
/// [`StoreFixture::with_chain_committed`] to pre-populate the store from a
/// [`ChainFixture`] before handing it to a test.
pub struct StoreFixture {
    tempdir: TempDir,
    chain_store: PrimaryChainStore,
    committed_chain_epoch: Option<ChainEpoch>,
}

impl StoreFixture {
    /// Opens an empty [`PrimaryChainStore`] on a fresh temporary directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary directory cannot be created or the
    /// store cannot be opened.
    pub fn open() -> Result<Self, StoreFixtureError> {
        let tempdir = TempDir::new().map_err(StoreFixtureError::TempDirCreation)?;
        let chain_store =
            PrimaryChainStore::open(tempdir.path(), ChainStoreOptions::for_local_tests())
                .map_err(StoreFixtureError::ChainStoreOpen)?;
        Ok(Self {
            tempdir,
            chain_store,
            committed_chain_epoch: None,
        })
    }

    /// Opens a [`PrimaryChainStore`] and commits every block in `chain_fixture` as
    /// one chain epoch identified by `chain_epoch_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary directory cannot be created, the
    /// store cannot be opened, or the chain fixture is empty.
    pub fn with_chain_committed(
        chain_fixture: &ChainFixture,
        chain_epoch_id: ChainEpochId,
    ) -> Result<Self, StoreFixtureError> {
        let chain_epoch_artifacts = chain_fixture
            .chain_epoch_artifacts(chain_epoch_id)
            .ok_or(StoreFixtureError::EmptyChainFixture)?;
        let committed_chain_epoch = chain_epoch_artifacts.chain_epoch;
        let mut fixture = Self::open()?;
        fixture
            .chain_store
            .commit_chain_epoch(chain_epoch_artifacts)
            .map_err(StoreFixtureError::ChainEpochCommit)?;
        fixture.committed_chain_epoch = Some(committed_chain_epoch);
        Ok(fixture)
    }

    /// Opens a [`PrimaryChainStore`] pre-populated with one synthetic block for the
    /// given network. Tests that just need a non-empty store reach for this
    /// constructor.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary directory cannot be created or the
    /// store cannot be opened.
    pub fn with_single_block(network: Network) -> Result<Self, StoreFixtureError> {
        let chain_fixture = ChainFixture::new(network).extend_blocks(1);
        Self::with_chain_committed(&chain_fixture, ChainEpochId::new(1))
    }

    /// Returns a borrowed view of the underlying [`PrimaryChainStore`].
    #[must_use]
    pub const fn chain_store(&self) -> &PrimaryChainStore {
        &self.chain_store
    }

    /// Returns the on-disk path of the underlying temporary directory.
    ///
    /// The path is removed when the fixture is dropped.
    #[must_use]
    pub fn tempdir_path(&self) -> &Path {
        self.tempdir.path()
    }

    /// Returns the chain epoch this fixture committed during construction, or
    /// `None` if the fixture was opened empty.
    #[must_use]
    pub const fn committed_chain_epoch(&self) -> Option<&ChainEpoch> {
        self.committed_chain_epoch.as_ref()
    }

    /// Consumes the fixture and returns its `(TempDir, PrimaryChainStore)` pair.
    ///
    /// Useful when a test needs to keep the directory alive while passing the
    /// store into a struct that takes ownership.
    #[must_use]
    pub fn into_parts(self) -> (TempDir, PrimaryChainStore) {
        (self.tempdir, self.chain_store)
    }
}

/// Errors raised while building a [`StoreFixture`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreFixtureError {
    /// Temporary directory creation failed.
    #[error("could not create temporary directory: {0}")]
    TempDirCreation(#[source] std::io::Error),
    /// Chain store could not be opened on the temporary directory.
    #[error("could not open chain store: {0}")]
    ChainStoreOpen(#[source] StoreError),
    /// Committing the seeded chain epoch failed.
    #[error("could not commit chain epoch: {0}")]
    ChainEpochCommit(#[source] StoreError),
    /// Cannot pre-populate a store from an empty chain fixture.
    #[error("chain fixture has no blocks; extend_blocks(N) before committing")]
    EmptyChainFixture,
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::StoreFixture;
    use crate::chain_fixture::ChainFixture;
    use zinder_core::{BlockHeight, ChainEpochId, Network};

    #[test]
    fn open_creates_a_writable_chain_store() -> Result<(), Box<dyn Error>> {
        let store_fixture = StoreFixture::open()?;
        assert!(store_fixture.tempdir_path().exists());
        Ok(())
    }

    #[test]
    fn single_block_fixture_exposes_committed_chain_epoch() -> Result<(), Box<dyn Error>> {
        let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
        let reader = store_fixture.chain_store().current_chain_epoch_reader()?;
        let chain_epoch = reader.chain_epoch();
        assert_eq!(chain_epoch.tip_height, BlockHeight::new(1));
        assert_eq!(chain_epoch.network, Network::ZcashRegtest);
        Ok(())
    }

    #[test]
    fn with_chain_committed_persists_every_fixture_block() -> Result<(), Box<dyn Error>> {
        let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
        let store_fixture =
            StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
        let reader = store_fixture.chain_store().current_chain_epoch_reader()?;
        let chain_epoch = reader.chain_epoch();
        assert_eq!(chain_epoch.tip_height, BlockHeight::new(3));

        for height_value in 1..=3_u32 {
            let block = reader.block_at(BlockHeight::new(height_value))?;
            assert!(
                block.is_some(),
                "block at height {height_value} should be readable after commit"
            );
        }
        Ok(())
    }
}
