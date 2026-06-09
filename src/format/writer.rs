use crate::error::{GappedError, Result};
use crate::format::header::{FileHeader, RecordType, EOR, MAGIC, MAGIC_COMPRESSED};
use crate::model::diff::Change;
use crate::model::entry::Entry;
use std::io::{self, Read, Write};

pub struct FormatWriter<W: Write> {
    inner: WriterInner<W>,
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
    pub fn maybe_compressed(inner: W, header: &FileHeader, compress: bool) -> Result<Self> {
        let magic = if compress { MAGIC_COMPRESSED } else { MAGIC };
        let mut raw_inner = inner;
        raw_inner.write_all(magic)?;

        let mut writer_inner = if compress {
            let encoder = zstd::stream::write::Encoder::new(raw_inner, 3)?;
            WriterInner::Compressed(encoder)
        } else {
            WriterInner::Plain(raw_inner)
        };

        let header_bytes = rmp_serde::to_vec(header)?;
        let header_len = (header_bytes.len() as u32).to_le_bytes();
        writer_inner.write_all(&header_len)?;
        writer_inner.write_all(&header_bytes)?;

        let bytes_written =
            magic.len() as u64 + header_len.len() as u64 + header_bytes.len() as u64;
        Ok(FormatWriter {
            inner: writer_inner,
            bytes_written,
        })
    }

    fn write(&mut self, buf: &[u8]) -> Result<()> {
        self.inner.write_all(buf)?;
        self.bytes_written += buf.len() as u64;
        Ok(())
    }

    pub fn write_snapshot_entry(&mut self, entry: &Entry) -> Result<()> {
        let payload = rmp_serde::to_vec(entry)?;
        self.write_record(RecordType::SnapshotEntry, &payload)
    }

    pub fn write_diff_change(&mut self, change: &Change) -> Result<()> {
        let payload = rmp_serde::to_vec(change)?;
        self.write_record(RecordType::DiffChange, &payload)
    }

    pub fn write_file_content_from_reader<R: Read>(
        &mut self,
        reader: &mut R,
        size: u64,
    ) -> Result<()> {
        self.write(&size.to_le_bytes())?;
        self.write(&[RecordType::FileContent as u8])?;

        let copied = io::copy(&mut reader.take(size), &mut CountWriter(&mut self.inner))?;
        if copied != size {
            return Err(GappedError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("FileContent short by {} bytes", size - copied),
            )));
        }
        self.bytes_written += copied;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.inner.write_all(&EOR)?;
        self.inner.flush()?;

        match self.inner {
            WriterInner::Plain(writer) => Ok(writer),
            WriterInner::Compressed(encoder) => Ok(encoder.finish()?),
        }
    }

    fn write_record(&mut self, record_type: RecordType, payload: &[u8]) -> Result<()> {
        self.write(&(payload.len() as u64).to_le_bytes())?;
        self.write(&[record_type as u8])?;
        self.write(payload)
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

struct CountWriter<'a, W: Write>(&'a mut WriterInner<W>);

impl<W: Write> Write for CountWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
