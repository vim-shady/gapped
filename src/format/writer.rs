use crate::error::{GappedError, Result};
use crate::format::header::{EOR, FileHeader, MAGIC, MAGIC_COMPRESSED, RecordType};
use crate::model::diff::Change;
use crate::model::entry::Entry;
use std::io;
use std::io::Write;

/// Streaming writer for gapped file format
/// Writes records one at a time, computing a checksum for everything written
pub struct FormatWriter<W: Write> {
    inner: WriterInner<W>,
    hasher: blake3::Hasher,
    bytes_written: u64,
}

enum WriterInner<W: Write> {
    Plain(W),
    Compressed(zstd::stream::write::Encoder<'static, W>),
}

impl<W: Write> WriterInner<W> {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            WriterInner::Plain(w) => w.write_all(buf),
            WriterInner::Compressed(w) => w.write_all(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            WriterInner::Plain(w) => w.flush(),
            WriterInner::Compressed(w) => w.flush(),
        }
    }
}

impl<W: Write> FormatWriter<W> {
    /// Create a new FormatWriter and write magic bytes + header
    pub fn new(inner: W, header: &FileHeader) -> Result<Self> {
        Self::new_impl(inner, header, false)
    }

    /// Create a new compressed FormatWriter
    pub fn new_compressed(inner: W, header: &FileHeader) -> Result<Self> {
        Self::new_impl(inner, header, true)
    }

    /// Create a FormatWriter, optionally compressed
    pub fn maybe_compressed(inner: W, header: &FileHeader, compress: bool) -> Result<Self> {
        Self::new_impl(inner, header, compress)
    }

    fn new_impl(mut inner: W, header: &FileHeader, compress: bool) -> Result<Self> {
        let mut hasher = blake3::Hasher::new();

        // Write magic bytes (uncompressed, so readers can detect format)
        let magic = if compress { MAGIC_COMPRESSED } else { MAGIC };
        inner.write_all(magic)?;
        hasher.update(magic);

        // Write with compression
        let mut writer_inner = if compress {
            let encoder = zstd::stream::write::Encoder::new(inner, 3)?;
            WriterInner::Compressed(encoder)
        } else {
            WriterInner::Plain(inner)
        };

        // Serialize and write header
        let header_bytes = rmp_serde::to_vec(header)?;
        let header_len = (header_bytes.len() as u32).to_le_bytes();
        writer_inner.write_all(&header_len)?;
        hasher.update(&header_len);
        writer_inner.write_all(&header_bytes)?;
        hasher.update(&header_bytes);

        let bytes_written =
            magic.len() as u64 + header_len.len() as u64 + header_bytes.len() as u64;
        Ok(FormatWriter {
            inner: writer_inner,
            hasher,
            bytes_written,
        })
    }

    /// Write a snapshot entry record.
    pub fn write_snapshot_entry(&mut self, entry: &Entry) -> Result<()> {
        let payload = rmp_serde::to_vec(entry)?;
        self.write_record(RecordType::SnapshotEntry, &payload)?;
        Ok(())
    }

    /// Write a diff change record.
    pub fn write_diff_change(&mut self, change: &Change) -> Result<()> {
        let payload = rmp_serde::to_vec(change)?;
        self.write_record(RecordType::DiffChange, &payload)?;
        Ok(())
    }

    /// Write raw file content.
    pub fn write_file_content(&mut self, content: &[u8]) -> Result<()> {
        self.write_record(RecordType::FileContent, content)?;
        Ok(())
    }

    /// Write raw file content streaming from a reader.
    pub fn write_file_content_from_reader<R: io::Read>(
        &mut self,
        reader: &mut R,
        size: u64,
    ) -> Result<()> {
        let len_bytes = size.to_le_bytes();
        let type_byte = [RecordType::FileContent as u8];

        self.inner.write_all(&len_bytes)?;
        self.hasher.update(&len_bytes);
        self.inner.write_all(&type_byte)?;
        self.hasher.update(&type_byte);
        self.bytes_written += 9;

        let mut buf = [0u8; 64 * 1024];
        let mut remaining = size;
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            let n = reader.read(&mut buf[..to_read])?;
            if n == 0 {
                return Err(GappedError::InvalidFormat(format!(
                    "Unexpected EOF: expected {} more bytes of file content",
                    remaining
                )));
            }
            self.inner.write_all(&buf[..n])?;
            self.hasher.update(&buf[..n]);
            self.bytes_written += n as u64;
            remaining -= n as u64;
        }

        Ok(())
    }

    /// Finalize the file by writing EOR marker and checksum
    pub fn finish(mut self) -> Result<W> {
        self.inner.write_all(&EOR)?;
        self.hasher.update(&EOR);

        let hash = self.hasher.finalize();
        self.inner.write_all(hash.as_bytes())?;
        self.inner.flush()?;

        // Finish compression
        match self.inner {
            WriterInner::Plain(writer) => Ok(writer),
            WriterInner::Compressed(encoder) => {
                let writer = encoder.finish()?;
                Ok(writer)
            }
        }
    }

    /// Write a single record
    fn write_record(&mut self, record_type: RecordType, payload: &[u8]) -> Result<()> {
        let len_bytes = (payload.len() as u64).to_le_bytes();
        let type_byte = [record_type as u8];

        self.inner.write_all(&len_bytes)?;
        self.hasher.update(&len_bytes);
        self.inner.write_all(&type_byte)?;
        self.hasher.update(&type_byte);
        self.bytes_written += 9;

        self.inner.write_all(payload)?;
        self.hasher.update(payload);
        self.bytes_written += payload.len() as u64;

        Ok(())
    }

    /// Get approximate number of bytes written
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}
