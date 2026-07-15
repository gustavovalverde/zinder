use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use rust_rocksdb::Options;

use super::{
    super::CanonicalStoreError,
    ordered_sst::{OrderedSstSet, SstArtifacts},
};

const MAX_MERGE_FAN_IN: usize = 64;

pub(super) struct FixedRecordSorter<const RECORD_LEN: usize> {
    staging_path: PathBuf,
    prefix: &'static str,
    record_capacity: usize,
    records: Vec<[u8; RECORD_LEN]>,
    runs: Vec<PathBuf>,
    next_run_index: u64,
}

impl<const RECORD_LEN: usize> FixedRecordSorter<RECORD_LEN> {
    pub(super) fn new(staging_path: &Path, prefix: &'static str, record_capacity: usize) -> Self {
        Self {
            staging_path: staging_path.to_path_buf(),
            prefix,
            record_capacity,
            records: Vec::with_capacity(record_capacity),
            runs: Vec::new(),
            next_run_index: 0,
        }
    }

    pub(super) fn push(&mut self, record: [u8; RECORD_LEN]) -> Result<(), CanonicalStoreError> {
        self.records.push(record);
        if self.records.len() >= self.record_capacity {
            self.flush_run()?;
        }
        Ok(())
    }

    fn flush_run(&mut self) -> Result<(), CanonicalStoreError> {
        if self.records.is_empty() {
            return Ok(());
        }
        self.records.sort_unstable();
        if self.records.windows(2).any(|rows| rows[0] == rows[1]) {
            return Err(CanonicalStoreError::block_load_sequence(format!(
                "{} contains a duplicate reverse-index key",
                self.prefix
            )));
        }
        let path = self.staging_path.join(format!(
            "{}-run-{:08}.bin",
            self.prefix, self.next_run_index
        ));
        self.next_run_index = self.next_run_index.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("sort run count exceeds u64::MAX")
        })?;
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

    pub(super) fn finish<const KEY_LEN: usize>(
        mut self,
        options: &Options,
        sst_target_logical_bytes: u64,
    ) -> Result<SstArtifacts, CanonicalStoreError> {
        self.flush_run()?;
        let Some(sorted_run) =
            merge_sort_runs::<RECORD_LEN, KEY_LEN>(&self.staging_path, self.prefix, self.runs)?
        else {
            return Ok(SstArtifacts {
                paths: Vec::new(),
                file_bytes: 0,
            });
        };
        let file =
            File::open(&sorted_run).map_err(|source| path_unavailable(&sorted_run, source))?;
        let mut reader = BufReader::new(file);
        let mut writer = OrderedSstSet::new(
            &self.staging_path,
            self.prefix,
            options,
            sst_target_logical_bytes,
        );
        let mut previous_key: Option<Vec<u8>> = None;
        while let Some(record) = read_fixed_record::<RECORD_LEN>(&mut reader, &sorted_run)? {
            if previous_key
                .as_deref()
                .is_some_and(|previous| previous == &record[..KEY_LEN])
            {
                return Err(duplicate_key_error(self.prefix));
            }
            writer.put(&record[..KEY_LEN], &record[KEY_LEN..])?;
            previous_key = Some(record[..KEY_LEN].to_vec());
        }
        let artifacts = writer.finish()?;
        fs::remove_file(&sorted_run).map_err(|source| path_unavailable(&sorted_run, source))?;
        Ok(artifacts)
    }
}

pub(super) fn record_capacity<const RECORD_LEN: usize>(
    memory_bytes: usize,
) -> Result<usize, CanonicalStoreError> {
    let capacity = memory_bytes / RECORD_LEN;
    if capacity == 0 {
        return Err(CanonicalStoreError::block_load_sequence(
            "reverse-index sort memory must hold at least one fixed record",
        ));
    }
    Ok(capacity)
}

fn merge_sort_runs<const RECORD_LEN: usize, const KEY_LEN: usize>(
    staging_path: &Path,
    prefix: &'static str,
    mut runs: Vec<PathBuf>,
) -> Result<Option<PathBuf>, CanonicalStoreError> {
    let mut pass = 0_u64;
    while runs.len() > 1 {
        let mut merged_runs = Vec::with_capacity(runs.len().div_ceil(MAX_MERGE_FAN_IN));
        for (group_index, group) in runs.chunks(MAX_MERGE_FAN_IN).enumerate() {
            let path = staging_path.join(format!("{prefix}-merge-{pass:04}-{group_index:08}.bin"));
            merge_run_group::<RECORD_LEN, KEY_LEN>(group, &path, prefix)?;
            for input in group {
                fs::remove_file(input).map_err(|source| path_unavailable(input, source))?;
            }
            merged_runs.push(path);
        }
        runs = merged_runs;
        pass = pass.checked_add(1).ok_or_else(|| {
            CanonicalStoreError::block_load_sequence("sort merge pass count exceeds u64::MAX")
        })?;
    }
    Ok(runs.pop())
}

fn merge_run_group<const RECORD_LEN: usize, const KEY_LEN: usize>(
    inputs: &[PathBuf],
    output: &Path,
    prefix: &'static str,
) -> Result<(), CanonicalStoreError> {
    if inputs.len() > MAX_MERGE_FAN_IN {
        return Err(CanonicalStoreError::block_load_sequence(
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
            return Err(duplicate_key_error(prefix));
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

fn duplicate_key_error(prefix: &'static str) -> CanonicalStoreError {
    CanonicalStoreError::block_load_sequence(format!(
        "{prefix} contains a duplicate reverse-index key"
    ))
}

fn read_fixed_record<const RECORD_LEN: usize>(
    reader: &mut BufReader<File>,
    path: &Path,
) -> Result<Option<[u8; RECORD_LEN]>, CanonicalStoreError> {
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

fn path_unavailable(path: &Path, source: std::io::Error) -> CanonicalStoreError {
    CanonicalStoreError::PathUnavailable {
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
        let error = record_capacity::<72>(71);
        assert!(matches!(
            error,
            Err(CanonicalStoreError::BlockLoadSequenceInvalid { .. })
        ));
    }

    #[test]
    fn fixed_record_sorter_rejects_same_key_with_different_values_across_runs()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TempDir::new()?;
        let mut sorter = FixedRecordSorter::<4>::new(temporary.path(), "duplicate", 1);
        sorter.push([1, 2, 3, 4])?;
        sorter.push([1, 2, 5, 6])?;

        let error = sorter
            .finish::<2>(&Options::default(), 1024)
            .err()
            .ok_or("duplicate reverse-index keys must fail")?;

        assert!(matches!(
            error,
            CanonicalStoreError::BlockLoadSequenceInvalid { .. }
        ));
        Ok(())
    }
}
