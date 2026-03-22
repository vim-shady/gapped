use crate::format::header::FileHeader;
use crate::model::diff::Change;
use crate::model::entry::Entry;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::Serialize;
use std::io::{Read, Write};

pub trait FormatWriter<W: Write> {
    fn write_snapshot_entry(&mut self, entry: &Entry) -> Result<()>;

    fn write_diff_change(&mut self, change: &Change) -> Result<()>;

    /// Write raw file content from a reader, streaming it
    fn write_file_content_from_reader(&mut self, reader: &mut dyn Read, size: u64) -> Result<()>;

    fn write_file_content(&mut self, content: &[u8]) -> Result<()>;
}

pub struct JsonFormatWriter<W: Write> {
    inner: W,
    bytes_written: u64,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum JsonRecord<'a> {
    #[serde(rename = "header")]
    Header(&'a FileHeader),
    #[serde(rename = "snapshot_entry")]
    SnapshotEntry(&'a Entry),
    #[serde(rename = "diff_change")]
    DiffChange { change: &'a Change },
    #[serde(rename = "file_content")]
    FileContent { data: &'a str },
}

impl<W: Write> JsonFormatWriter<W> {
    pub fn new(mut inner: W, header: &FileHeader) -> Result<Self> {
        let mut bytes_written = 0u64;
        bytes_written += Self::write_line(&mut inner, &JsonRecord::Header(header))?;
        Ok(JsonFormatWriter {
            inner,
            bytes_written,
        })
    }

    fn write_line(w: &mut W, record: &JsonRecord) -> Result<u64> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        w.write_all(&line)?;
        Ok(line.len() as u64)
    }
}

impl<W: Write> FormatWriter<W> for JsonFormatWriter<W> {
    fn write_snapshot_entry(&mut self, entry: &Entry) -> Result<()> {
        self.bytes_written += Self::write_line(&mut self.inner, &JsonRecord::SnapshotEntry(entry))?;
        Ok(())
    }

    fn write_diff_change(&mut self, change: &Change) -> Result<()> {
        self.bytes_written +=
            Self::write_line(&mut self.inner, &JsonRecord::DiffChange { change })?;
        Ok(())
    }

    fn write_file_content_from_reader(&mut self, reader: &mut dyn Read, _size: u64) -> Result<()> {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        self.write_file_content(&buf)
    }

    fn write_file_content(&mut self, content: &[u8]) -> Result<()> {
        let encoded = BASE64.encode(content);
        self.bytes_written +=
            Self::write_line(&mut self.inner, &JsonRecord::FileContent { data: &encoded })?;
        Ok(())
    }
}
