use crate::commands::snapshot::hash_snapshot_file;
use crate::format::header::FileHeader;
use crate::format::writer::{FormatWriter, JsonFormatWriter};
use crate::model::diff::{AddedEntry, Change, ChangeKind, Diff, ModifiedEntry};
use crate::model::entry::{Entry, EntryKind};
use crate::model::snapshot::Snapshot;
use anyhow::Result;
use log::info;
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

pub fn run_diff(
    root_dir: &Path,
    snapshot_in: &Path,
    diff_out: &Path,
    snapshot_out: &Path,
    split_size: Option<u64>,
    compress: bool,
) -> Result<()> {
    // Validate root dir
    if !root_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Root directory does not exist: {}",
            root_dir.display()
        ));
    }

    let root_dir = root_dir.canonicalize()?;

    // Compute hash of input snapshot
    info!("Hashing input snapshot {}", snapshot_in.display());
    let source_snapshot_hash = hash_snapshot_file(snapshot_in)?;

    // Load the input snapshot
    info!("Loading input snapshot {}", snapshot_in.display());
    let (old_entries, _old_header) = crate::commands::snapshot::load_snapshot(snapshot_in)?;

    // Load old snapshot as HashMap for hash reuse
    let (old_entries_map, _) = crate::commands::snapshot::load_snapshot_entries(snapshot_in)?;

    // Walk current filesystem
    info!("Walking filesystem under {}", root_dir.display());
    let (new_entries, stats) = crate::fs::walk::walk_filesystem(&root_dir, Some(&old_entries_map))?;

    // Compute diff
    info!("Computing diff");
    let changes = compute_diff(&old_entries, &new_entries, &root_dir)?;

    let added_count = changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Added(_)))
        .count();
    let modified_count = changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Modified(_)))
        .count();
    let removed_count = changes
        .iter()
        .filter(|change| matches!(change.kind, ChangeKind::Removed(_)))
        .count();

    // Write diff file(s)
    if let Some(max_bytes) = split_size {
        write_split_diff(
            diff_out,
            &changes,
            source_snapshot_hash,
            &root_dir,
            max_bytes,
            compress,
        )?;
    } else {
        write_single_diff(
            diff_out,
            &changes,
            source_snapshot_hash,
            &root_dir,
            compress,
        )?;
    }

    // Write new snapshot
    info!("Writing new snapshot to {}", snapshot_out.display());
    write_snapshot(snapshot_out, &new_entries, &root_dir, compress)?;

    // Report stats
    eprintln!("Diff complete:");
    eprintln!("  Added: {}", added_count);
    eprintln!("  Modified: {}", modified_count);
    eprintln!("  Deleted: {}", removed_count);
    eprintln!("  Total changes: {}", changes.len());
    if stats.errors > 0 {
        eprintln!("  Walk errors: {}", stats.errors);
    }

    Ok(())
}

/// Write snapshot to file
fn write_snapshot(
    snapshot_out: &Path,
    entries: &[Entry],
    root_dir: &Path,
    compress: bool,
) -> Result<()> {
    let file = File::create(snapshot_out)?;
    let buf_writer = BufWriter::new(file);

    let header = FileHeader {
        file_type: "snapshot".to_string(),
        version: Snapshot::CURRENT_VERSION,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        source_snapshot_hash: None,
        root_dir: Some(root_dir.to_string_lossy().into_owned()),
    };

    let mut writer: JsonFormatWriter<BufWriter<File>> = if compress {
        todo!()
    } else {
        JsonFormatWriter::new(buf_writer, &header)?
    };

    for entry in entries {
        writer.write_snapshot_entry(entry)?;
    }

    Ok(())
}

fn write_split_diff(
    p0: &Path,
    p1: &Vec<Change>,
    p2: [u8; 32],
    p3: &PathBuf,
    p4: u64,
    p5: bool,
) -> Result<()> {
    todo!()
}

fn write_single_diff(
    diff_out: &Path,
    changes: &[Change],
    source_snapshot_hash: [u8; 32],
    root_dir: &Path,
    compress: bool,
) -> Result<()> {
    let file = File::create(diff_out)?;
    let buf_writer = BufWriter::new(file);

    let header = FileHeader {
        file_type: "diff".to_string(),
        version: Diff::CURRENT_VERSION,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        source_snapshot_hash: Some(source_snapshot_hash),
        root_dir: None,
    };

    let mut writer = if compress {
        todo!()
    } else {
        JsonFormatWriter::new(buf_writer, &header)?
    };

    write_changes(&mut writer, changes, root_dir)?;

    Ok(())
}

// TODO: refactor this mess
/// Write changes to a format writer, incl. file content
fn write_changes<W: std::io::Write>(
    writer: &mut dyn FormatWriter<W>,
    changes: &[Change],
    root_dir: &Path,
) -> Result<()> {
    for change in changes {
        writer.write_diff_change(change)?;

        match &change.kind {
            ChangeKind::Added(added) if added.has_content => {
                let full_path = change.path.to_full_path(root_dir);
                match File::open(&full_path) {
                    Ok(file) => {
                        let meta_data = file.metadata()?;
                        let size = meta_data.len();
                        let mut reader = BufReader::new(file);
                        writer.write_file_content_from_reader(&mut reader, size)?;
                    }
                    Err(e) => {
                        log::warn!("Cannot read file {}: {}", full_path.display(), e);
                        // Write empty file content
                        writer.write_file_content(&[])?;
                    }
                }
            }
            ChangeKind::Modified(modified) if modified.has_content => {
                let full_path = change.path.to_full_path(root_dir);
                match File::open(&full_path) {
                    Ok(file) => {
                        let meta_data = file.metadata()?;
                        let size = meta_data.len();
                        let mut reader = BufReader::new(file);
                        writer.write_file_content_from_reader(&mut reader, size)?;
                    }
                    Err(e) => {
                        log::warn!("Cannot read file {}: {}", full_path.display(), e);
                        writer.write_file_content(&[])?;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn compute_diff(
    old_entries: &[Entry],
    new_entries: &[Entry],
    _root_dir: &Path,
) -> Result<Vec<Change>> {
    let mut diff = Vec::new();
    let mut old_iter = old_entries.iter().peekable();
    let mut new_iter = new_entries.iter().peekable();

    loop {
        match (old_iter.peek(), new_iter.peek()) {
            (Some(old), Some(new)) => match old.path.cmp(&new.path) {
                Ordering::Less => {
                    diff.push(build_removed_change(old));
                    old_iter.next();
                }
                Ordering::Greater => {
                    diff.push(build_added_change(new));
                    new_iter.next();
                }
                Ordering::Equal => {
                    if old.kind != new.kind {
                        diff.push(build_removed_change(old));
                        diff.push(build_added_change(new));
                    } else if let Some(change) = compute_entry_diff(old, new) {
                        diff.push(change);
                    }
                    old_iter.next();
                    new_iter.next();
                }
            },
            (Some(old), None) => {
                diff.push(build_removed_change(old));
                old_iter.next();
            }
            (None, Some(new)) => {
                diff.push(build_added_change(new));
                new_iter.next();
            }
            (None, None) => break,
        }
    }

    Ok(diff)
}

/// Compare two entire of the same kind and produce a change if they differ
fn compute_entry_diff(old: &Entry, new: &Entry) -> Option<Change> {
    debug_assert!(old.kind == new.kind);

    let metadata_changed = old.metadata != new.metadata;
    let hash_changed = old.hash != new.hash;
    let symlink_target_changed = old.symlink_target != new.symlink_target;

    if !metadata_changed && !hash_changed && !symlink_target_changed {
        return None;
    }

    let modified = ModifiedEntry {
        new_metadata: if metadata_changed {
            Some(new.metadata.clone())
        } else {
            None
        },
        new_hash: if hash_changed { new.hash } else { None },
        has_content: hash_changed && new.kind == EntryKind::File,
        new_symlink_target: if symlink_target_changed {
            new.symlink_target.clone()
        } else {
            None
        },
    };

    Some(Change {
        path: new.path.clone(),
        kind: ChangeKind::Modified(modified),
    })
}

fn build_removed_change(entry: &Entry) -> Change {
    Change {
        path: entry.path.clone(),
        kind: ChangeKind::Removed(entry.kind.clone()),
    }
}

fn build_added_change(entry: &Entry) -> Change {
    Change {
        path: entry.path.clone(),
        kind: ChangeKind::Added(AddedEntry {
            entry: entry.clone(),
            has_content: entry.kind == EntryKind::File,
        }),
    }
}
