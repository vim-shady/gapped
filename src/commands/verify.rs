use crate::commands::snapshot::load_snapshot_entries;
use crate::error::{GappedError, Result};
use crate::format::reader::{FormatReader, Record};
use crate::fs::walk::walk_filesystem;
use crate::model::diff::{Change, ChangeKind};
use crate::model::entry::Entry;
use crate::model::path::RelativePath;
use log::info;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Execute the verify command.
/// Simulates applying the diff to the current filesystem state and checks the result
/// against the target snapshot.
pub fn run_verify(root_dir: &Path, diff_files: &[&Path], snapshot_path: &Path) -> Result<()> {
    let root_dir = super::validate_root_dir(root_dir)?;

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

    for diff_path in diff_files {
        info!("Loading diff from {}", diff_path.display());
        let file = File::open(diff_path)?;
        let reader = BufReader::new(file);
        let (mut format_reader, _header) = FormatReader::new(reader)?;

        let records = format_reader.read_all_records()?;

        for record in records {
            if let Record::DiffChange(change) = record {
                all_changes.push(change);
            }
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
                if !simulated_entry.metadata.matches(&target_entry.metadata) {
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
        Err(GappedError::VerificationFailed(discrepancies))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::apply::detect_diff_files;
    use crate::commands::diff::run_diff;
    use crate::commands::snapshot::run_snapshot;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn copy_tree(src: &Path, dst: &Path) {
        use nix::sys::stat::UtimensatFlags;
        use nix::sys::time::TimeSpec;
        use std::os::unix::fs::MetadataExt;

        fn set_mtime_from(path: &Path, src_meta: &fs::Metadata) {
            let atime = TimeSpec::UTIME_OMIT;
            let mtime = TimeSpec::new(src_meta.mtime(), src_meta.mtime_nsec() as i64);
            nix::sys::stat::utimensat(None, path, &atime, &mtime, UtimensatFlags::NoFollowSymlink)
                .unwrap();
        }

        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let ft = entry.file_type().unwrap();
            if ft.is_dir() {
                copy_tree(&src_path, &dst_path);
            } else if ft.is_file() {
                fs::copy(&src_path, &dst_path).unwrap();
                let m = fs::metadata(&src_path).unwrap();
                set_mtime_from(&dst_path, &m);
            }
        }
        // set the directory's mtime last
        let src_meta = fs::metadata(src).unwrap();
        set_mtime_from(dst, &src_meta);
    }

    // build a scenario with split diff chunks and return the pieces needed for verify.
    fn build_split_scenario(tmp: &TempDir) -> (PathBuf, Vec<PathBuf>, PathBuf) {
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir(&source).unwrap();

        // seed source
        for i in 0..8 {
            fs::write(source.join(format!("f_{:02}.txt", i)), vec![b'a'; 1024]).unwrap();
        }

        // target is a copy of the initial source
        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false).unwrap();

        // modify source
        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..8 {
            fs::write(source.join(format!("f_{:02}.txt", i)), vec![b'b'; 2048]).unwrap();
        }

        // small split size -> multiple chunks
        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(3072), false).unwrap();

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(chunks.len() > 1, "expected split chunks");

        (target, chunks, snap2)
    }

    #[test]
    fn test_run_verify_with_split_chunks_succeeds() {
        let tmp = TempDir::new().unwrap();
        let (target, chunks, snap2) = build_split_scenario(&tmp);

        let chunk_refs: Vec<&Path> = chunks.iter().map(|p: &PathBuf| p.as_path()).collect();
        // Applying these chunks to `target` (currently == original source)
        // should yield the state captured in snap2.
        run_verify(&target, &chunk_refs, &snap2).expect("verify should succeed");
    }

    #[test]
    fn test_run_verify_detects_discrepancy_from_split_chunks() {
        let tmp = TempDir::new().unwrap();
        let (target, chunks, snap2) = build_split_scenario(&tmp);

        fs::write(target.join("extra.txt"), b"unexpected").unwrap();

        let chunk_refs: Vec<&Path> = chunks.iter().map(|p: &PathBuf| p.as_path()).collect();
        let result = run_verify(&target, &chunk_refs, &snap2);
        assert!(result.is_err(), "verify should fail on discrepancy");
    }

    #[test]
    fn test_run_verify_single_diff_still_works() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("only.txt"), b"v1").unwrap();
        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(source.join("only.txt"), b"v2-longer").unwrap();

        let diff = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff, &snap2, None, false).unwrap();

        run_verify(&target, &[diff.as_path()], &snap2).expect("single-file verify should pass");
    }
}
