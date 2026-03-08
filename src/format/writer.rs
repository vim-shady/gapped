use crate::format::header::FileHeader;
use crate::model::entry::Entry;
use anyhow::Result;
use serde::Serialize;
use std::io::Write;

pub trait FormatWriter<W: Write> {
    fn write_snapshot_entry(&mut self, entry: &Entry) -> Result<()>;
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
    pub fn write_line(w: &mut W, record: &JsonRecord) -> Result<u64> {
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
}
