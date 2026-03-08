use crate::format::header::FileHeader;
use crate::model::entry::Entry;
use anyhow::Result;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read};

pub enum Record {
    SnapshotEntry(Entry),
}

pub trait FormatReader {
    fn next_record(&mut self) -> Result<Option<Record>>;

    fn read_all_records(&mut self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        while let Some(record) = self.next_record()? {
            records.push(record);
        }
        Ok(records)
    }
}

pub struct JsonFormatReader<R: Read> {
    reader: BufReader<R>,
    finished: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum JsonRecord {
    #[serde(rename = "header")]
    Header(FileHeader),
    #[serde(rename = "snapshot_entry")]
    SnapshotEntry(Entry),
}

impl<R: Read> JsonFormatReader<R> {
    pub fn new(inner: R) -> Result<(Self, FileHeader)> {
        let mut reader = BufReader::new(inner);

        let mut line = String::new();
        reader.read_line(&mut line)?;

        if line.is_empty() {
            return Err(anyhow::anyhow!("Empty file"));
        }

        let record: JsonRecord = serde_json::from_str(&line.trim())?;
        let header = match record {
            JsonRecord::Header(header) => header,
            _ => return Err(anyhow::anyhow!("Invalid file format")),
        };

        Ok((
            JsonFormatReader {
                reader,
                finished: false,
            },
            header,
        ))
    }
}

impl<R: Read> FormatReader for JsonFormatReader<R> {
    fn next_record(&mut self) -> Result<Option<Record>> {
        if self.finished {
            return Ok(None);
        }

        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line)?;
        if bytes_read == 0 || line.trim().is_empty() {
            self.finished = true;
            return Ok(None);
        }

        let record: JsonRecord = serde_json::from_str(&line.trim())?;
        match record {
            JsonRecord::Header(_) => Err(anyhow::anyhow!("Unexpected header record in body")),
            JsonRecord::SnapshotEntry(entry) => Ok(Some(Record::SnapshotEntry(entry))),
        }
    }
}
