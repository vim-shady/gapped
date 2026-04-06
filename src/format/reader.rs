use crate::error::{GappedError, Result};
use crate::format::header::{FileHeader, MAGIC, MAGIC_COMPRESSED, RecordType};
use crate::model::diff::Change;
use crate::model::entry::Entry;
use std::io::{BufReader, Read};

/// Record read from binary format
pub enum Record {
    SnapshotEntry(Entry),
    DiffChange(Change),
    FileContent(Vec<u8>),
}

/// Streaming reader for gapped file format
pub struct FormatReader {
    inner: Box<dyn Read>,
    hasher: blake3::Hasher,
    finished: bool,
}

impl FormatReader {
    pub fn new<R: Read + 'static>(mut inner: R) -> Result<(Self, FileHeader)> {
        let mut hasher = blake3::Hasher::new();

        // Read magic bytes
        let mut magic = [0u8; 9];
        inner.read_exact(&mut magic)?;
        hasher.update(&magic);

        let compressed = if &magic == MAGIC {
            false
        } else if &magic == MAGIC_COMPRESSED {
            true
        } else {
            return Err(GappedError::InvalidFormat(format!(
                "Invalid magic bytes: {:?}",
                magic
            )));
        };

        let mut reader: Box<dyn Read> = if compressed {
            let buf_reader = BufReader::new(inner);
            let decoder = zstd::stream::read::Decoder::new(buf_reader)?;
            Box::new(decoder)
        } else {
            Box::new(inner)
        };

        // Read header length
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)?;
        hasher.update(&len_bytes);
        let header_len = u32::from_le_bytes(len_bytes) as usize;

        // Read header payload
        let mut header_bytes = vec![0u8; header_len];
        reader.read_exact(&mut header_bytes)?;
        hasher.update(&header_bytes);

        let header: FileHeader = rmp_serde::from_slice(&header_bytes)?;

        Ok((
            FormatReader {
                inner: reader,
                hasher,
                finished: false,
            },
            header,
        ))
    }

    /// Read the next record. Retruns `None` if the end of the file has been reached.
    pub fn next_record(&mut self) -> Result<Option<Record>> {
        if self.finished {
            return Ok(None);
        }

        // Read record length
        let mut len_bytes = [0u8; 4];
        self.inner.read_exact(&mut len_bytes)?;

        // Read type byte
        let mut type_byte = [0u8; 1];
        self.inner.read_exact(&mut type_byte)?;

        // Check for EOR
        if len_bytes == [0u8; 4] && type_byte[0] == 0 {
            self.hasher.update(&len_bytes);
            self.hasher.update(&type_byte);

            // Read and verify checksum
            let mut checksum_bytes = [0u8; 32];
            self.inner.read_exact(&mut checksum_bytes)?;

            let expected_hash = self.hasher.finalize();
            if expected_hash.as_bytes() != &checksum_bytes {
                return Err(GappedError::ChecksumMismatch {
                    expected: Self::hex_encode(expected_hash.as_bytes()),
                    got: Self::hex_encode(&checksum_bytes),
                });
            }
            self.finished = true;
            return Ok(None);
        }
        let record_type = RecordType::from_u8(type_byte[0]).ok_or_else(|| {
            GappedError::InvalidFormat(format!("Unknown record type: {:?}", type_byte[0]))
        })?;

        let payload_len = u32::from_le_bytes(len_bytes) as usize;

        // Update hashser with length + type
        self.hasher.update(&len_bytes);
        self.hasher.update(&type_byte);

        // Read payload
        let mut payload = vec![0u8; payload_len];
        self.inner.read_exact(&mut payload)?;
        self.hasher.update(&payload);

        let record = match record_type {
            RecordType::SnapshotEntry => {
                let entry: Entry = rmp_serde::from_slice(&payload)?;
                Record::SnapshotEntry(entry)
            }
            RecordType::DiffChange => {
                let change: Change = rmp_serde::from_slice(&payload)?;
                Record::DiffChange(change)
            }
            RecordType::FileContent => Record::FileContent(payload),
        };

        Ok(Some(record))
    }

    pub fn read_all_records(&mut self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        while let Some(record) = self.next_record()? {
            records.push(record);
        }
        Ok(records)
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}
