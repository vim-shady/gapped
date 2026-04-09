use crate::error::{GappedError, Result};
use crate::format::header::{FileHeader, MAGIC, MAGIC_COMPRESSED, RecordType};
use std::io::{BufReader, Read, Write};

/// Header of a record
pub struct RecordHeader {
    pub record_type: RecordType,
    pub payload_len: u64,
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

    /// Read the next record header. Returns `None` at end-of-records (EOR).
    /// After calling this, one must consume the payload via `read_payload`,
    /// `skip_payload`, or `copy_payload_to` before calling this again.
    pub fn next_record_header(&mut self) -> Result<Option<RecordHeader>> {
        if self.finished {
            return Ok(None);
        }

        // Read record length + type byte
        let mut len_bytes = [0u8; 8];
        self.inner.read_exact(&mut len_bytes)?;
        let mut type_byte = [0u8; 1];
        self.inner.read_exact(&mut type_byte)?;

        // Check for EOR
        if len_bytes == [0u8; 8] && type_byte[0] == 0 {
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
        let payload_len = u64::from_le_bytes(len_bytes);

        self.hasher.update(&len_bytes);
        self.hasher.update(&type_byte);

        Ok(Some(RecordHeader {
            record_type,
            payload_len,
        }))
    }

    /// Read a payload into memory and update the hasher.
    pub fn read_payload(&mut self, len: u64) -> Result<Vec<u8>> {
        let mut payload = vec![0u8; len as usize];
        self.inner.read_exact(&mut payload)?;
        self.hasher.update(&payload);
        Ok(payload)
    }

    /// Skip a payload without materializing it.
    pub fn skip_payload(&mut self, len: u64) -> Result<()> {
        self.copy_payload_to(len, &mut std::io::sink())
    }

    /// Stream a payload to a writer without materializing it and update the hasher.
    pub fn copy_payload_to<W: Write>(&mut self, len: u64, dest: &mut W) -> Result<()> {
        let mut buf = [0u8; 64 * 1024];
        let mut remaining = len;
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            self.inner.read_exact(&mut buf[..to_read])?;
            self.hasher.update(&buf[..to_read]);
            dest.write_all(&buf[..to_read])?;
            remaining -= to_read as u64;
        }
        Ok(())
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}
