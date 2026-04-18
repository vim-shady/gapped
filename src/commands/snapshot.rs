use crate::error::Result;
use crate::format::header::{FileHeader, RecordType};
use crate::format::reader::FormatReader;
use crate::format::writer::FormatWriter;
use crate::fs::walk::walk_filesystem;
use crate::model::entry::Entry;
use crate::model::path::RelativePath;
use crate::progress::Reporter;
use log::info;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

/// Load a snapshot from a file (should already be sorted by path)
pub fn load_snapshot(snapshot_path: &Path) -> Result<(Vec<Entry>, FileHeader)> {
    let file = File::open(snapshot_path)?;
    let reader = std::io::BufReader::new(file);
    let (mut format_reader, header) = FormatReader::new(reader)?;

    let mut entries = Vec::new();
    while let Some(h) = format_reader.next_record_header()? {
        if h.record_type == RecordType::SnapshotEntry {
            let payload = format_reader.read_payload(h.payload_len)?;
            let entry: Entry = rmp_serde::from_slice(&payload)?;
            entries.push(entry);
        } else {
            format_reader.skip_payload(h.payload_len)?;
        }
    }

    Ok((entries, header))
}

/// Load a snapshot from a file, returning the entries as a HashMap for O(1) lookup
pub fn load_snapshot_entries(
    snapshot_path: &Path,
) -> Result<(HashMap<RelativePath, Entry>, FileHeader)> {
    let (entries, header) = load_snapshot(snapshot_path)?;
    let map = entries
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    Ok((map, header))
}

pub fn run_snapshot(
    root_dir: &Path,
    snapshot_out: &Path,
    snapshot_in: Option<&Path>,
    compress: bool,
    reporter: &Reporter,
) -> Result<()> {
    let root_dir = super::validate_root_dir(root_dir)?;

    // Load previous snapshot if provided. The Vec is sorted by path, so walk
    // can binary-search it directly.
    let previous_entries = if let Some(snapshot_in) = snapshot_in {
        info!("Loading previous snapshot from {}", snapshot_in.display());
        let load_pb = reporter.spinner("Loading snapshot");
        let (entries, _header) = load_snapshot(snapshot_in)?;
        load_pb.finish_with_message(format!("Loaded {} entries from previous snapshot", entries.len()));
        Some(entries)
    } else {
        None
    };

    // Walk the file system
    info!("Walking filesystem under {}", root_dir.display());
    let (entries, stats) = walk_filesystem(&root_dir, previous_entries.as_deref(), reporter)?;

    info!("Writing snapshot to {}", snapshot_out.display());
    let file = File::create(snapshot_out)?;
    let buf_writer = BufWriter::new(file);

    let header = FileHeader::snapshot(&root_dir);

    let mut writer = FormatWriter::maybe_compressed(buf_writer, &header, compress)?;

    let write_pb = reporter.counter("Writing snapshot", entries.len() as u64);
    for entry in &entries {
        writer.write_snapshot_entry(entry)?;
        write_pb.inc(1);
    }
    write_pb.finish_with_message(format!("Wrote {} entries", entries.len()));

    writer.finish()?;

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
