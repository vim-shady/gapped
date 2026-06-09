use crate::error::{GappedError, Result};
use crate::format::header::{
    FileHeader, MAGIC, MAGIC_COMPRESSED, MAGIC_LEN, RECORD_LEN_SIZE, RECORD_TYPE_SIZE, RecordType,
};
use std::io::{BufReader, Read, Write};

pub struct RecordHeader {
    pub record_type: RecordType,
    pub payload_len: u64,
}

pub struct FormatReader {
    inner: Box<dyn Read>,
    finished: bool,
}

impl FormatReader {
    pub fn new<R: Read + 'static>(mut inner: R) -> Result<(Self, FileHeader)> {
        let mut magic = [0u8; MAGIC_LEN];
        inner.read_exact(&mut magic)?;

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
            Box::new(zstd::stream::read::Decoder::new(BufReader::new(inner))?)
        } else {
            Box::new(inner)
        };

        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)?;

        let mut header_bytes = vec![0u8; u32::from_le_bytes(len_bytes) as usize];
        reader.read_exact(&mut header_bytes)?;

        let header: FileHeader = rmp_serde::from_slice(&header_bytes)?;

        Ok((
            FormatReader {
                inner: reader,
                finished: false,
            },
            header,
        ))
    }

    /// Read the next record header. Returns `None` at EOR.
    /// After calling this, one must consume the payload via `read_payload`,
    /// `skip_payload`, or `copy_payload_to` before calling this again.
    pub fn next_record_header(&mut self) -> Result<Option<RecordHeader>> {
        if self.finished {
            return Ok(None);
        }

        let mut len_bytes = [0u8; RECORD_LEN_SIZE];
        let mut type_byte = [0u8; RECORD_TYPE_SIZE];
        self.inner.read_exact(&mut len_bytes)?;
        self.inner.read_exact(&mut type_byte)?;

        if len_bytes == [0u8; RECORD_LEN_SIZE] && type_byte[0] == 0 {
            self.finished = true;
            return Ok(None);
        }

        let record_type = RecordType::from_u8(type_byte[0]).ok_or_else(|| {
            GappedError::InvalidFormat(format!("Unknown record type: {:?}", type_byte[0]))
        })?;

        Ok(Some(RecordHeader {
            record_type,
            payload_len: u64::from_le_bytes(len_bytes),
        }))
    }

    pub fn read_payload(&mut self, len: u64) -> Result<Vec<u8>> {
        let mut payload = vec![0u8; len as usize];
        self.inner.read_exact(&mut payload)?;
        Ok(payload)
    }

    pub fn skip_payload(&mut self, len: u64) -> Result<()> {
        self.copy_payload_to(len, &mut std::io::sink())
    }

    pub fn copy_payload_to<W: Write>(&mut self, len: u64, dest: &mut W) -> Result<()> {
        let mut limited = (&mut self.inner).take(len);
        std::io::copy(&mut limited, dest)?;
        Ok(())
    }
}
