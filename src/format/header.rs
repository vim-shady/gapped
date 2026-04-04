use serde::{Deserialize, Serialize};

/// Magic bytes identifying a gapped file
pub const MAGIC: &[u8; 9] = b"GAPPED\x00\x01\x00";

/// Magic bytes for identifying a zstd-compressed gapped file
pub const MAGIC_COMPRESSED: &[u8; 9] = b"GAPPEDZ01";

/// End of record marker
pub const EOR: [u8; 5] = [0u8; 5];

/// Record types in binary format
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// File content record
    SnapshotEntry = 1,
    /// Diff change metadata
    DiffChange = 2,
    /// Raw file content following a DiffChange or SnapshotEntry
    FileContent = 3,
}

impl RecordType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(RecordType::SnapshotEntry),
            2 => Some(RecordType::DiffChange),
            3 => Some(RecordType::FileContent),
            _ => None,
        }
    }
}

/// File header stored after magic bytes
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileHeader {
    /// "snapshot" or "diff"
    pub file_type: String,
    /// Version of the format
    pub version: u32,
    pub created_at: i64,
    /// For diff files: hash of source snapshot
    pub source_snapshot_hash: Option<[u8; 32]>,
    /// For snapshots: informational root directory
    pub root_dir: Option<String>,
    /// For split diffs: chunk index (0-based)
    pub chunk_index: Option<u32>,
    pub more_chunks: Option<bool>,
}
