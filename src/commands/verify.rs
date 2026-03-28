use crate::commands::snapshot::load_snapshot_entries;
use crate::format::reader::{FormatReader, Record};
use crate::fs::walk::walk_filesystem;
use crate::model::diff::{Change, ChangeKind};
use crate::model::entry::Entry;
use crate::model::path::RelativePath;
use anyhow::Result;
use log::info;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Execute the verify command.
/// Simulates applying the diff to the current filesystem state and checks the result
/// against the target snapshot.
pub fn run_verify(root_dir: &Path, diff_file: &Path, snapshot_path: &Path) -> Result<()> {
    if !root_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Root directory {} does not exist",
            root_dir.display()
        ));
    }

    let root_dir = root_dir.canonicalize()?;

    info!("Loading snapshot from {}", snapshot_path.display());
    let (target_entries, _) = load_snapshot_entries(snapshot_path)?;

    info!("Walk filesystem at {}", root_dir.display());
    let (current_entries_vec, _) = walk_filesystem(&root_dir, None)?;

    // Build a map of current entries
    let mut simulated: HashMap<RelativePath, Entry> = current_entries_vec
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();

    // Load and parse all diff changes
    let mut all_changes: Vec<Change> = Vec::new();

    info!("Loading diff from {}", diff_file.display());
    let file = File::open(diff_file)?;
    let reader = BufReader::new(file);
    let (mut format_reader, _header) = FormatReader::new(reader)?;

    let records = format_reader.read_all_records()?;

    for record in records {
        if let Record::DiffChange(change) = record {
            all_changes.push(change);
        }
    }

    // Simulate applying the diff
    info!("Simulating applying diff ({} changes)", all_changes.len());
    for change in &all_changes {
        match &change.kind {
            ChangeKind::Removed(_) => {
                simulated.remove(&change.path);
            }
            ChangeKind::Added(added) => {
                simulated.insert(change.path.clone(), added.entry.clone());
            }
            ChangeKind::Modified(modified) => {
                if let Some(existing) = simulated.get_mut(&change.path) {
                    if let Some(new_metadata) = &modified.new_metadata {
                        existing.metadata = new_metadata.clone();
                    }
                    if let Some(new_hash) = &modified.new_hash {
                        existing.hash = Some(*new_hash);
                    }
                    if let Some(new_symlink_target) = &modified.new_symlink_target {
                        existing.symlink_target = Some(new_symlink_target.clone());
                    }
                }
            }
        }
    }

    // Compare simulated entries with target entries
    let mut discrepancies = 0;

    // Check each entry in target snapshot
    for (path, target_entry) in &target_entries {
        match simulated.get(path) {
            None => {
                eprintln!("MISSING: {} (in snapshot but not in simulated state)", path);
                discrepancies += 1;
            }
            Some(simulated_entry) => {
                if simulated_entry.kind != target_entry.kind {
                    eprintln!(
                        "KIND MISMATCH: {} (expected {:?}, got {:?})",
                        path, target_entry.kind, simulated_entry.kind
                    );
                    discrepancies += 1;
                }
                if simulated_entry.metadata != target_entry.metadata {
                    eprintln!("METADATA MISMATCH: {}", path);
                    eprintln!("  simulated: {:?}", simulated_entry.metadata);
                    eprintln!("  target:    {:?}", target_entry.metadata);
                    discrepancies += 1;
                }
                if simulated_entry.hash != target_entry.hash {
                    eprintln!("HASH MISMATCH: {}", path);
                    discrepancies += 1;
                }
                if simulated_entry.symlink_target != target_entry.symlink_target {
                    eprintln!("SYMLINK MISMATCH: {}", path);
                    discrepancies += 1;
                }
            }
        }
    }

    // CHeck for extra entries not in the snapshot
    for (path, _) in &simulated {
        if !target_entries.contains_key(path) {
            eprintln!("EXTRA: {} (in simulated state but not in snapshot)", path);
            discrepancies += 1;
        }
    }

    if discrepancies == 0 {
        eprintln!("Verify complete: simulated state matches target snapshot");
        Ok(())
    } else {
        eprintln!("Verify failed: {} discrepancies found", discrepancies);
        Err(anyhow::anyhow!("Verify failed"))
    }
}
