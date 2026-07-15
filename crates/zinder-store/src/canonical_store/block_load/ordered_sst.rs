use std::{
    fs,
    path::{Path, PathBuf},
};

use rust_rocksdb::{Options, SstFileWriter};

use super::{super::CanonicalStoreError, checked_add_sst_bytes, checked_row_bytes};

pub(super) struct OrderedSstSet<'options> {
    staging_path: &'options Path,
    prefix: &'static str,
    options: &'options Options,
    target_logical_bytes: u64,
    writer: Option<SstFileWriter<'options>>,
    current_path: Option<PathBuf>,
    current_logical_bytes: u64,
    next_file_index: u64,
    previous_key: Option<Vec<u8>>,
    paths: Vec<PathBuf>,
    file_bytes: u64,
}

pub(super) struct SstArtifacts {
    pub(super) paths: Vec<PathBuf>,
    pub(super) file_bytes: u64,
}

impl<'options> OrderedSstSet<'options> {
    pub(super) fn new(
        staging_path: &'options Path,
        prefix: &'static str,
        options: &'options Options,
        target_logical_bytes: u64,
    ) -> Self {
        Self {
            staging_path,
            prefix,
            options,
            target_logical_bytes,
            writer: None,
            current_path: None,
            current_logical_bytes: 0,
            next_file_index: 0,
            previous_key: None,
            paths: Vec::new(),
            file_bytes: 0,
        }
    }

    pub(super) fn put(&mut self, key: &[u8], row_bytes: &[u8]) -> Result<(), CanonicalStoreError> {
        if self
            .previous_key
            .as_deref()
            .is_some_and(|previous| previous >= key)
        {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "{} keys are duplicated or not strictly increasing",
                self.prefix
            )));
        }
        if self.writer.is_some() && self.current_logical_bytes >= self.target_logical_bytes {
            self.finish_current()?;
        }
        if self.writer.is_none() {
            self.open_next()?;
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| CanonicalStoreError::block_load_sequence("SST writer did not open"))?;
        writer
            .put(key, row_bytes)
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "canonical ordered SST write",
                source,
            })?;
        self.current_logical_bytes = self
            .current_logical_bytes
            .checked_add(checked_row_bytes(key.len(), row_bytes.len())?)
            .ok_or_else(|| {
                CanonicalStoreError::block_load_sequence(
                    "current SST logical byte count exceeds u64::MAX",
                )
            })?;
        self.previous_key = Some(key.to_vec());
        Ok(())
    }

    fn open_next(&mut self) -> Result<(), CanonicalStoreError> {
        let path = self
            .staging_path
            .join(format!("{}-{:08}.sst", self.prefix, self.next_file_index));
        self.next_file_index = self.next_file_index.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("SST file count exceeds u64::MAX")
        })?;
        let writer = SstFileWriter::create(self.options);
        writer
            .open(&path)
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "canonical ordered SST open",
                source,
            })?;
        self.writer = Some(writer);
        self.current_path = Some(path);
        self.current_logical_bytes = 0;
        Ok(())
    }

    fn finish_current(&mut self) -> Result<(), CanonicalStoreError> {
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        let path = self.current_path.take().ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("open SST writer has no output path")
        })?;
        writer
            .finish()
            .map_err(|source| CanonicalStoreError::RocksDbOperation {
                operation: "canonical ordered SST finish",
                source,
            })?;
        let file_bytes = fs::metadata(&path)
            .map_err(|source| CanonicalStoreError::PathUnavailable {
                path: path.clone(),
                source,
            })?
            .len();
        self.file_bytes = checked_add_sst_bytes(self.file_bytes, file_bytes)?;
        self.paths.push(path);
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<SstArtifacts, CanonicalStoreError> {
        self.finish_current()?;
        Ok(SstArtifacts {
            paths: self.paths,
            file_bytes: self.file_bytes,
        })
    }
}
