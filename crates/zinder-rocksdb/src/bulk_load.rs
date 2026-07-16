//! Bounded external sorting and ordered SST construction for `RocksDB`.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use rust_rocksdb::{Options, SstFileWriter};
use thiserror::Error;

const MAX_MERGE_FAN_IN: usize = 64;

/// Failure while staging opaque ordered records for `RocksDB` ingestion.
#[derive(Debug, Error)]
pub enum BulkLoadError {
    /// A caller supplied an invalid sorter or SST boundary.
    #[error("RocksDB bulk-load input is invalid: {reason}")]
    InvalidInput {
        /// Stable description of the rejected invariant.
        reason: String,
    },
    /// A staging path could not be read, written, or removed.
    #[error("RocksDB bulk-load path is unavailable: {path}")]
    PathUnavailable {
        /// Path whose operation failed.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// `RocksDB` rejected an ordered SST operation.
    #[error("RocksDB bulk-load {operation} failed")]
    RocksDbOperation {
        /// Stable operation label.
        operation: &'static str,
        /// Underlying `RocksDB` failure.
        #[source]
        source: rust_rocksdb::Error,
    },
}

impl BulkLoadError {
    fn invalid(reason: impl Into<String>) -> Self {
        Self::InvalidInput {
            reason: reason.into(),
        }
    }
}

/// Physical SST files produced from one strictly ordered logical family.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct SstFileSet {
    /// Ordered paths ready for external-file ingestion.
    pub paths: Vec<PathBuf>,
    /// Total physical bytes occupied by `paths`.
    pub file_bytes: u64,
}

/// Writes strictly increasing opaque keys into bounded-size SST files.
pub struct OrderedSstWriter<'options> {
    staging_path: &'options Path,
    artifact_label: &'static str,
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

impl<'options> OrderedSstWriter<'options> {
    /// Creates an ordered writer whose files rotate after the target is reached.
    pub fn new(
        staging_path: &'options Path,
        artifact_label: &'static str,
        options: &'options Options,
        target_logical_bytes: u64,
    ) -> Result<Self, BulkLoadError> {
        if target_logical_bytes == 0 {
            return Err(BulkLoadError::invalid(
                "SST target logical bytes must be greater than zero",
            ));
        }
        Ok(Self {
            staging_path,
            artifact_label,
            options,
            target_logical_bytes,
            writer: None,
            current_path: None,
            current_logical_bytes: 0,
            next_file_index: 0,
            previous_key: None,
            paths: Vec::new(),
            file_bytes: 0,
        })
    }

    /// Appends one key and value after verifying strict key order.
    pub fn put(&mut self, key: &[u8], encoded_value: &[u8]) -> Result<(), BulkLoadError> {
        if self
            .previous_key
            .as_deref()
            .is_some_and(|previous| previous >= key)
        {
            return Err(BulkLoadError::invalid(format!(
                "{} keys are duplicated or not strictly increasing",
                self.artifact_label
            )));
        }
        if self.writer.is_some() && self.current_logical_bytes >= self.target_logical_bytes {
            self.finish_current()?;
        }
        if self.writer.is_none() {
            self.open_next()?;
        }
        let writer = self.writer.as_mut().ok_or_else(|| {
            BulkLoadError::invalid("ordered SST writer did not retain its open file")
        })?;
        writer
            .put(key, encoded_value)
            .map_err(|source| BulkLoadError::RocksDbOperation {
                operation: "ordered SST write",
                source,
            })?;
        self.current_logical_bytes = self
            .current_logical_bytes
            .checked_add(logical_row_bytes(key.len(), encoded_value.len())?)
            .ok_or_else(|| BulkLoadError::invalid("current SST logical bytes exceed u64::MAX"))?;
        self.previous_key = Some(key.to_vec());
        Ok(())
    }

    /// Finishes the last file and returns every produced SST path.
    pub fn finish(mut self) -> Result<SstFileSet, BulkLoadError> {
        self.finish_current()?;
        Ok(SstFileSet {
            paths: self.paths,
            file_bytes: self.file_bytes,
        })
    }

    fn open_next(&mut self) -> Result<(), BulkLoadError> {
        let path = self.staging_path.join(format!(
            "{}-{:08}.sst",
            self.artifact_label, self.next_file_index
        ));
        self.next_file_index = self
            .next_file_index
            .checked_add(1)
            .ok_or_else(|| BulkLoadError::invalid("SST file count exceeds u64::MAX"))?;
        let writer = SstFileWriter::create(self.options);
        writer
            .open(&path)
            .map_err(|source| BulkLoadError::RocksDbOperation {
                operation: "ordered SST open",
                source,
            })?;
        self.writer = Some(writer);
        self.current_path = Some(path);
        self.current_logical_bytes = 0;
        Ok(())
    }

    fn finish_current(&mut self) -> Result<(), BulkLoadError> {
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        let path = self
            .current_path
            .take()
            .ok_or_else(|| BulkLoadError::invalid("open ordered SST writer has no output path"))?;
        writer
            .finish()
            .map_err(|source| BulkLoadError::RocksDbOperation {
                operation: "ordered SST finish",
                source,
            })?;
        let file_bytes = fs::metadata(&path)
            .map_err(|source| path_unavailable(&path, source))?
            .len();
        self.file_bytes = self
            .file_bytes
            .checked_add(file_bytes)
            .ok_or_else(|| BulkLoadError::invalid("total SST file bytes exceed u64::MAX"))?;
        self.paths.push(path);
        Ok(())
    }
}

/// Bounded external sorter for opaque fixed-width records.
///
/// Records are ordered lexicographically. `KEY_LEN` is supplied at finish so
/// the sorter can reject duplicate logical keys before emitting SST files.
pub struct FixedRecordSorter<const RECORD_LEN: usize> {
    staging_path: PathBuf,
    artifact_label: &'static str,
    record_capacity: usize,
    records: Vec<[u8; RECORD_LEN]>,
    runs: Vec<PathBuf>,
    next_run_index: u64,
}

impl<const RECORD_LEN: usize> FixedRecordSorter<RECORD_LEN> {
    /// Creates a sorter with a caller-computed in-memory record capacity.
    pub fn new(
        staging_path: &Path,
        artifact_label: &'static str,
        record_capacity: usize,
    ) -> Result<Self, BulkLoadError> {
        if RECORD_LEN == 0 {
            return Err(BulkLoadError::invalid(
                "fixed records must contain at least one byte",
            ));
        }
        if record_capacity == 0 {
            return Err(BulkLoadError::invalid(
                "fixed-record sort memory must hold at least one record",
            ));
        }
        Ok(Self {
            staging_path: staging_path.to_path_buf(),
            artifact_label,
            record_capacity,
            records: Vec::with_capacity(record_capacity),
            runs: Vec::new(),
            next_run_index: 0,
        })
    }

    /// Stages one opaque record and flushes a sorted run at the memory bound.
    pub fn push(&mut self, record: [u8; RECORD_LEN]) -> Result<(), BulkLoadError> {
        self.records.push(record);
        if self.records.len() >= self.record_capacity {
            self.flush_run()?;
        }
        Ok(())
    }

    /// Merges every run and emits ordered SST files split at `KEY_LEN`.
    pub fn finish<const KEY_LEN: usize>(
        mut self,
        options: &Options,
        sst_target_logical_bytes: u64,
    ) -> Result<SstFileSet, BulkLoadError> {
        validate_key_length::<RECORD_LEN, KEY_LEN>()?;
        if sst_target_logical_bytes == 0 {
            return Err(BulkLoadError::invalid(
                "SST target logical bytes must be greater than zero",
            ));
        }
        self.flush_run()?;
        let Some(sorted_run) = merge_sort_runs::<RECORD_LEN, KEY_LEN>(
            &self.staging_path,
            self.artifact_label,
            self.runs,
        )?
        else {
            return Ok(SstFileSet::default());
        };
        let file =
            File::open(&sorted_run).map_err(|source| path_unavailable(&sorted_run, source))?;
        let mut reader = BufReader::new(file);
        let mut writer = OrderedSstWriter::new(
            &self.staging_path,
            self.artifact_label,
            options,
            sst_target_logical_bytes,
        )?;
        while let Some(record) = read_fixed_record::<RECORD_LEN>(&mut reader, &sorted_run)? {
            writer.put(&record[..KEY_LEN], &record[KEY_LEN..])?;
        }
        let files = writer.finish()?;
        fs::remove_file(&sorted_run).map_err(|source| path_unavailable(&sorted_run, source))?;
        Ok(files)
    }

    fn flush_run(&mut self) -> Result<(), BulkLoadError> {
        if self.records.is_empty() {
            return Ok(());
        }
        self.records.sort_unstable();
        let path = self.staging_path.join(format!(
            "{}-run-{:08}.bin",
            self.artifact_label, self.next_run_index
        ));
        self.next_run_index = self
            .next_run_index
            .checked_add(1)
            .ok_or_else(|| BulkLoadError::invalid("sort run count exceeds u64::MAX"))?;
        let file = File::create(&path).map_err(|source| path_unavailable(&path, source))?;
        let mut writer = BufWriter::new(file);
        for record in self.records.drain(..) {
            writer
                .write_all(&record)
                .map_err(|source| path_unavailable(&path, source))?;
        }
        writer
            .flush()
            .map_err(|source| path_unavailable(&path, source))?;
        self.runs.push(path);
        Ok(())
    }
}

/// Calculates how many fixed-width records fit inside a byte limit.
pub fn fixed_record_capacity<const RECORD_LEN: usize>(
    memory_bytes: usize,
) -> Result<usize, BulkLoadError> {
    if RECORD_LEN == 0 {
        return Err(BulkLoadError::invalid(
            "fixed records must contain at least one byte",
        ));
    }
    let capacity = memory_bytes / RECORD_LEN;
    if capacity == 0 {
        return Err(BulkLoadError::invalid(
            "fixed-record sort memory must hold at least one record",
        ));
    }
    Ok(capacity)
}

fn validate_key_length<const RECORD_LEN: usize, const KEY_LEN: usize>() -> Result<(), BulkLoadError>
{
    if KEY_LEN == 0 || KEY_LEN > RECORD_LEN {
        return Err(BulkLoadError::invalid(
            "fixed-record key length must be between one and the record length",
        ));
    }
    Ok(())
}

fn merge_sort_runs<const RECORD_LEN: usize, const KEY_LEN: usize>(
    staging_path: &Path,
    artifact_label: &'static str,
    mut runs: Vec<PathBuf>,
) -> Result<Option<PathBuf>, BulkLoadError> {
    let mut pass = 0_u64;
    while runs.len() > 1 {
        let mut merged_runs = Vec::with_capacity(runs.len().div_ceil(MAX_MERGE_FAN_IN));
        for (group_index, group) in runs.chunks(MAX_MERGE_FAN_IN).enumerate() {
            let path = staging_path.join(format!(
                "{artifact_label}-merge-{pass:04}-{group_index:08}.bin"
            ));
            merge_run_group::<RECORD_LEN, KEY_LEN>(group, &path, artifact_label)?;
            for input in group {
                fs::remove_file(input).map_err(|source| path_unavailable(input, source))?;
            }
            merged_runs.push(path);
        }
        runs = merged_runs;
        pass = pass
            .checked_add(1)
            .ok_or_else(|| BulkLoadError::invalid("sort merge pass count exceeds u64::MAX"))?;
    }
    Ok(runs.pop())
}

fn merge_run_group<const RECORD_LEN: usize, const KEY_LEN: usize>(
    inputs: &[PathBuf],
    output: &Path,
    artifact_label: &'static str,
) -> Result<(), BulkLoadError> {
    if inputs.len() > MAX_MERGE_FAN_IN {
        return Err(BulkLoadError::invalid(
            "sort merge exceeded its fixed file-descriptor fan-in",
        ));
    }
    let mut readers = Vec::with_capacity(inputs.len());
    let mut heap = BinaryHeap::new();
    for (reader_index, path) in inputs.iter().enumerate() {
        let file = File::open(path).map_err(|source| path_unavailable(path, source))?;
        let mut reader = BufReader::new(file);
        if let Some(record) = read_fixed_record::<RECORD_LEN>(&mut reader, path)? {
            heap.push(Reverse((record, reader_index)));
        }
        readers.push(reader);
    }
    let file = File::create(output).map_err(|source| path_unavailable(output, source))?;
    let mut writer = BufWriter::new(file);
    let mut previous_key: Option<Vec<u8>> = None;
    while let Some(Reverse((record, reader_index))) = heap.pop() {
        if previous_key
            .as_deref()
            .is_some_and(|previous| previous == &record[..KEY_LEN])
        {
            return Err(duplicate_key_error(artifact_label));
        }
        writer
            .write_all(&record)
            .map_err(|source| path_unavailable(output, source))?;
        previous_key = Some(record[..KEY_LEN].to_vec());
        if let Some(next) =
            read_fixed_record::<RECORD_LEN>(&mut readers[reader_index], &inputs[reader_index])?
        {
            heap.push(Reverse((next, reader_index)));
        }
    }
    writer
        .flush()
        .map_err(|source| path_unavailable(output, source))
}

fn duplicate_key_error(artifact_label: &'static str) -> BulkLoadError {
    BulkLoadError::invalid(format!("{artifact_label} contains a duplicate logical key"))
}

fn read_fixed_record<const RECORD_LEN: usize>(
    reader: &mut BufReader<File>,
    path: &Path,
) -> Result<Option<[u8; RECORD_LEN]>, BulkLoadError> {
    let mut record = [0_u8; RECORD_LEN];
    let first_byte_count = reader
        .read(&mut record[..1])
        .map_err(|source| path_unavailable(path, source))?;
    if first_byte_count == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut record[1..])
        .map_err(|source| path_unavailable(path, source))?;
    Ok(Some(record))
}

fn logical_row_bytes(key_bytes: usize, value_bytes: usize) -> Result<u64, BulkLoadError> {
    let row_bytes = key_bytes
        .checked_add(value_bytes)
        .ok_or_else(|| BulkLoadError::invalid("logical row bytes exceed usize::MAX"))?;
    u64::try_from(row_bytes)
        .map_err(|_| BulkLoadError::invalid("logical row bytes exceed u64::MAX"))
}

fn path_unavailable(path: &Path, source: std::io::Error) -> BulkLoadError {
    BulkLoadError::PathUnavailable {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn fixed_record_sorter_rejects_memory_smaller_than_one_record() {
        assert!(matches!(
            fixed_record_capacity::<72>(71),
            Err(BulkLoadError::InvalidInput { .. })
        ));
    }

    #[test]
    fn fixed_record_sorter_rejects_same_key_with_different_values_across_runs()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let mut sorter = FixedRecordSorter::<4>::new(temporary.path(), "duplicate", 1)?;
        sorter.push([1, 2, 3, 4])?;
        sorter.push([1, 2, 5, 6])?;

        let error = sorter
            .finish::<2>(&Options::default(), 1_024)
            .err()
            .ok_or("duplicate logical keys must fail")?;

        assert!(matches!(error, BulkLoadError::InvalidInput { .. }));
        Ok(())
    }

    #[test]
    fn ordered_sst_writer_rejects_non_increasing_keys() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let options = Options::default();
        let mut writer = OrderedSstWriter::new(temporary.path(), "rows", &options, 1_024)?;
        writer.put(b"b", b"value")?;

        assert!(matches!(
            writer.put(b"a", b"value"),
            Err(BulkLoadError::InvalidInput { .. })
        ));
        Ok(())
    }

    #[test]
    fn empty_fixed_record_sorter_still_rejects_zero_sst_target()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let sorter = FixedRecordSorter::<4>::new(temporary.path(), "empty", 1)?;

        assert!(matches!(
            sorter.finish::<2>(&Options::default(), 0),
            Err(BulkLoadError::InvalidInput { .. })
        ));
        Ok(())
    }
}
