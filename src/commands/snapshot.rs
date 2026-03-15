use crate::format::header::FileHeader;
use crate::format::reader::{FormatReader, JsonFormatReader, Record};
use crate::format::writer::{FormatWriter, JsonFormatWriter};
use crate::fs::walk::walk_filesystem;
use crate::model::entry::Entry;
use crate::model::path::RelativePath;
use crate::model::snapshot::Snapshot;
use anyhow::Result;
use log::info;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// Load a snapshot from a file, returning the entries as a HashMap for O(1) lookup
pub fn load_snapshot_entries(
    snapshot_path: &Path,
) -> Result<(HashMap<RelativePath, Entry>, FileHeader)> {
    let file = File::open(snapshot_path)?;
    let reader = std::io::BufReader::new(file);
    let (mut json_reader, header) = JsonFormatReader::new(reader)?;

    let mut entries: HashMap<RelativePath, Entry> = HashMap::new();
    let records = json_reader.read_all_records()?;
    for record in records {
        if let Record::SnapshotEntry(entry) = record {
            entries.insert(entry.path.clone(), entry);
        }
    }
    Ok((entries, header))
}

/// Load a snapshot from a file (should already be sorted by path)
pub fn load_snapshot(snapshot_path: &Path) -> Result<(Vec<Entry>, FileHeader)> {
    let file = File::open(snapshot_path)?;
    let reader = std::io::BufReader::new(file);
    let (mut json_reader, header) = JsonFormatReader::new(reader)?;

    let mut entries = Vec::new();
    let records = json_reader.read_all_records()?;
    for record in records {
        if let Record::SnapshotEntry(entry) = record {
            entries.push(entry);
        }
    }

    Ok((entries, header))
}

pub fn run_snapshot(
    root_dir: &Path,
    snapshot_out: &Path,
    snapshot_in: Option<&Path>,
    compress: bool,
) -> Result<()> {
    // Validate root_dir
    if !root_dir.is_dir() {
        return Err(anyhow::anyhow!("Root directory does not exist"));
    }

    let root_dir = root_dir.canonicalize()?;

    // Load previous snapshot if provided
    let previous_entries = if let Some(snapshot_in) = snapshot_in {
        info!("Loading previous snapshot from {}", snapshot_in.display());
        let (entries, _header) = load_snapshot_entries(snapshot_in)?;
        info!("Loaded {} entries from previous snapshot", entries.len());
        Some(entries)
    } else {
        None
    };

    // Walk the file system
    info!("Walking filesystem under {}", root_dir.display());
    let (entries, stats) = walk_filesystem(&root_dir, previous_entries.as_ref())?;

    info!("Writing snapshot to {}", snapshot_out.display());
    let file = File::create(snapshot_out)?;
    let buf_writer = BufWriter::new(file);

    let header = FileHeader {
        file_type: "snapshot".to_string(),
        version: Snapshot::CURRENT_VERSION,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        root_dir: Some(root_dir.to_string_lossy().to_string()),
    };

    let mut writer = if compress {
        todo!() // Todo: implement compression
    } else {
        JsonFormatWriter::new(buf_writer, &header)?
    };

    for entry in &entries {
        writer.write_snapshot_entry(entry)?;
    }

    // Report statistics
    eprintln!("Snapshot complete:");
    eprintln!("  Total entries: {}", stats.total_entries);
    eprintln!("  Directories: {}", stats.directories);
    eprintln!("  Symlinks: {}", stats.symlinks);
    if stats.errors > 0 {
        eprintln!("  Errors/warnings: {}", stats.errors);
    }

    Ok(())
}

pub fn hash_snapshot_file(snapshot_path: &Path) -> Result<[u8; 32]> {
    crate::fs::hash::hash_file(snapshot_path)
        .map_err(|e| anyhow::anyhow!("Failed to hash snapshot file: {}", e))
}
