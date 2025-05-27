use csv::{Writer, WriterBuilder};
use serde::Serialize;
use std::fs::File;
use std::path::PathBuf;

use crate::SimulationError;

/// Implements a writer that will write records in batches to the file provided.
pub struct BatchedWriter {
    batch_size: u32,
    counter: u32,
    writer: Writer<File>,
}

impl BatchedWriter {
    /// Creates a new writer and the results file that output will be written to.
    pub fn new(
        directory: PathBuf,
        file_name: String,
        batch_size: u32,
    ) -> Result<BatchedWriter, SimulationError> {
        if batch_size == 0 {
            return Err(SimulationError::FileError);
        }

        let file = directory.join(file_name);

        let writer = WriterBuilder::new()
            .from_path(file)
            .map_err(SimulationError::CsvError)?;

        Ok(BatchedWriter {
            batch_size,
            counter: 0,
            writer,
        })
    }

    /// Adds an item to the batch to be written, flushing to disk if the batch size has been reached.
    pub fn queue<S: Serialize>(&mut self, record: S) -> Result<(), SimulationError> {
        // If there's an error serializing an input, flush and exit with an error.
        self.writer.serialize(record).map_err(|e| {
            // If we encounter another errors (when we've already failed to serialize), we just log because we've
            // already experienced a critical error.
            if let Err(e) = self.write(true) {
                log::error!("Error flushing to disk: {e}");
            }

            SimulationError::CsvError(e)
        })?;

        // Otherwise increment counter and flush if we've reached batch size.
        self.counter = self.counter % self.batch_size + 1;
        self.write(false)
    }

    /// Writes the contents of the batched writer to disk. Will result in a write if force is true _or_ the batch is
    /// full.
    pub fn write(&mut self, force: bool) -> Result<(), SimulationError> {
        if force || self.batch_size == self.counter {
            self.counter = 0;
            return self
                .writer
                .flush()
                .map_err(|e| SimulationError::CsvError(e.into()));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
    struct TestRecord {
        id: u32,
    }

    fn read_csv_contents(file_path: &PathBuf) -> Vec<TestRecord> {
        let mut reader = csv::Reader::from_path(file_path).unwrap();
        reader.deserialize().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn test_basic_write_and_flush_on_batch_size() {
        let dir = tempdir().unwrap();
        let file_name = "test_basic_write_and_flush_on_batch_size.csv".to_string();
        let file_path = dir.path().join(&file_name);
        let mut writer = BatchedWriter::new(dir.path().to_path_buf(), file_name, 2).unwrap();

        let rec1 = TestRecord { id: 1 };
        let rec2 = TestRecord { id: 2 };

        writer.queue(rec1).unwrap();
        writer.queue(rec2).unwrap();

        assert!(file_path.exists(), "File should exist after flush");
        let records = read_csv_contents(&file_path);
        assert_eq!(records, vec![rec1, rec2]);

        // Queuing a record that doesn't hit our batch limit shouldn't flush.
        let rec3 = TestRecord { id: 3 };

        writer.queue(rec3).unwrap();
        let records = read_csv_contents(&file_path);
        assert_eq!(records, vec![rec1, rec2]);

        // Force flushing should write even if batch isn't hit.
        writer.write(true).unwrap();
        let records = read_csv_contents(&file_path);
        assert_eq!(records, vec![rec1, rec2, rec3]);
    }

    #[test]
    fn test_forced_flush() {
        let dir = tempdir().unwrap();
        let file_name = "test_forced_flush.csv".to_string();
        let file_path = dir.path().join(&file_name);
        let mut writer = BatchedWriter::new(dir.path().to_path_buf(), file_name, 10).unwrap();

        let rec = TestRecord { id: 1 };
        writer.queue(rec).unwrap();

        writer.write(true).unwrap();
        assert!(file_path.exists(), "File should exist after forced flush");
        let records = read_csv_contents(&file_path);
        assert_eq!(records, vec![rec]);
    }

    #[test]
    fn test_zero_batch_size() {
        let dir = tempdir().unwrap();
        let file_name = "test_zero_batch_size_no_auto_flush.csv".to_string();
        assert!(BatchedWriter::new(dir.path().to_path_buf(), file_name, 0).is_err());
    }

    /// Create a record that can't be serialized.
    struct BadRecord {}

    impl Serialize for BadRecord {
        fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("intentional failure"))
        }
    }

    #[test]
    fn test_serialization_error() {
        let dir = tempdir().unwrap();
        let file_name = "test_serialization_error.csv".to_string();
        let file_path = dir.path().join(&file_name);
        let mut writer = BatchedWriter::new(dir.path().to_path_buf(), file_name, 2).unwrap();

        let rec = TestRecord { id: 1 };
        writer.queue(rec).unwrap();

        let bad = BadRecord {};
        let result = writer.queue(bad);
        assert!(result.is_err());

        writer.write(true).unwrap();
        assert!(file_path.exists(), "File should exist after forced flush");
        let records = read_csv_contents(&file_path);
        assert_eq!(records, vec![rec]);
    }
}
