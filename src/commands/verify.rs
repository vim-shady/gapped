use crate::commands::apply::parse_diff_metadata;
use crate::commands::simulate::simulate_apply;
use crate::commands::snapshot::load_snapshot_entries;
use crate::error::{GappedError, Result};
use crate::fs::walk::walk_filesystem;
use crate::model::entry::Entry;
use crate::model::path::RelativePath;
use crate::progress::Reporter;
use log::info;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Execute the verify command.
/// Simulates applying the diff to the current filesystem state and checks the result
/// against the target snapshot.
pub fn run_verify(
    root_dir: &Path,
    diff_files: &[&Path],
    snapshot_path: &Path,
    reporter: &Reporter,
) -> Result<()> {
    let root_dir = super::validate_root_dir(root_dir)?;

    info!("Loading snapshot from {}", snapshot_path.display());
    let load_pb = reporter.spinner("Loading target snapshot");
    let (target_entries, _) = load_snapshot_entries(snapshot_path)?;
    load_pb.finish_with_message(format!("Loaded {} entries from target snapshot", target_entries.len()));

    info!("Walk filesystem at {}", root_dir.display());
    let (current_entries_vec, _) = walk_filesystem(&root_dir, None, reporter)?;

    let mut simulated: HashMap<RelativePath, Entry> = current_entries_vec
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();

    let parse_pb = reporter.spinner("Reading diff metadata");
    let changes = parse_diff_metadata(diff_files)?;
    parse_pb.finish_with_message(format!("Read {} changes from diff", changes.len()));
    info!("Simulating applying diff ({} changes)", changes.len());

    let sim_pb = reporter.spinner("Simulating apply");
    let implicit_dirs = simulate_apply(&mut simulated, &changes);
    sim_pb.finish_and_clear();

    let cmp_pb = reporter.spinner("Comparing entries");
    let discrepancies = compare_entries(&simulated, &target_entries, &implicit_dirs);
    cmp_pb.finish_and_clear();

    for msg in &discrepancies {
        eprintln!("{}", msg);
    }

    if discrepancies.is_empty() {
        eprintln!("Verify complete: simulated state matches target snapshot");
        Ok(())
    } else {
        eprintln!("Verify failed: {} discrepancies found", discrepancies.len());
        Err(GappedError::VerificationFailed(discrepancies.len()))
    }
}

/// Compare simulated entries against target entries. Returns discrepancy messages.
///
/// `implicit_dirs` is the set of directories that `apply` will leave alone
/// (parent dirs touched by child changes but without an explicit entry in the
/// diff). For those, the metadata comparison is skipped — apply preserves the
/// target's existing mtime, so any drift between target and snapshot is
/// expected and not actionable.
fn compare_entries(
    simulated: &HashMap<RelativePath, Entry>,
    target: &HashMap<RelativePath, Entry>,
    implicit_dirs: &HashSet<RelativePath>,
) -> Vec<String> {
    let mut discrepancies = Vec::new();

    for (path, target_entry) in target {
        match simulated.get(path) {
            None => {
                discrepancies.push(format!(
                    "MISSING: {} (in snapshot but not in simulated state)",
                    path
                ));
            }
            Some(simulated_entry) => {
                if simulated_entry.kind != target_entry.kind {
                    discrepancies.push(format!(
                        "KIND MISMATCH: {} (expected {:?}, got {:?})",
                        path, target_entry.kind, simulated_entry.kind
                    ));
                }
                if !implicit_dirs.contains(path)
                    && !simulated_entry.metadata.matches(&target_entry.metadata)
                {
                    discrepancies.push(format!("METADATA MISMATCH: {}", path));
                    discrepancies.push(format!("  simulated: {:?}", simulated_entry.metadata));
                    discrepancies.push(format!("  target:    {:?}", target_entry.metadata));
                }
                if simulated_entry.hash != target_entry.hash {
                    discrepancies.push(format!("HASH MISMATCH: {}", path));
                }
                if simulated_entry.symlink_target != target_entry.symlink_target {
                    discrepancies.push(format!("SYMLINK MISMATCH: {}", path));
                }
            }
        }
    }

    for path in simulated.keys() {
        if !target.contains_key(path) {
            discrepancies.push(format!(
                "EXTRA: {} (in simulated state but not in snapshot)",
                path
            ));
        }
    }

    discrepancies
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

    use crate::test_util::copy_tree;

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
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        // modify source
        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..8 {
            fs::write(source.join(format!("f_{:02}.txt", i)), vec![b'b'; 2048]).unwrap();
        }

        // small split size -> multiple chunks
        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff_base,
            &snap2,
            Some(3072),
            false,
            &Reporter::hidden(),
        )
        .unwrap();

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
        run_verify(&target, &chunk_refs, &snap2, &Reporter::hidden())
            .expect("verify should succeed");
    }

    #[test]
    fn test_run_verify_detects_discrepancy_from_split_chunks() {
        let tmp = TempDir::new().unwrap();
        let (target, chunks, snap2) = build_split_scenario(&tmp);

        fs::write(target.join("extra.txt"), b"unexpected").unwrap();

        let chunk_refs: Vec<&Path> = chunks.iter().map(|p: &PathBuf| p.as_path()).collect();
        let result = run_verify(&target, &chunk_refs, &snap2, &Reporter::hidden());
        assert!(result.is_err(), "verify should fail on discrepancy");
    }

    /// Drifted parent-dir mtimes on the target should not be reported as
    /// METADATA MISMATCH if the diff doesn't carry an explicit change for
    /// that dir — apply restores the dir's pre-existing mtime, so verify
    /// must mirror that and stay quiet.
    #[test]
    fn test_run_verify_skips_implicit_parent_dir_mtime_drift() {
        use nix::sys::stat::{utimensat, UtimensatFlags};
        use nix::sys::time::TimeSpec;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");

        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub/a.txt"), b"v1").unwrap();
        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        // in-place write does not bump source/sub/'s mtime, so the diff
        // will only contain a Modified change for a.txt — no change for sub.
        fs::write(source.join("sub/a.txt"), b"v2-longer").unwrap();

        let diff = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff,
            &snap2,
            None,
            false,
            &Reporter::hidden(),
        )
        .unwrap();

        // Externally bump target/sub's mtime far away from snap2's value.
        // Without the implicit-dir handling, verify would surface this drift
        // as a METADATA MISMATCH even though apply will leave sub alone.
        let drifted = TimeSpec::new(1_000_000_000, 0);
        let omit = TimeSpec::UTIME_OMIT;
        utimensat(
            None,
            &target.join("sub"),
            &omit,
            &drifted,
            UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();

        run_verify(&target, &[diff.as_path()], &snap2, &Reporter::hidden())
            .expect("verify must ignore drift on dirs apply will not touch");
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
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(source.join("only.txt"), b"v2-longer").unwrap();

        let diff = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff,
            &snap2,
            None,
            false,
            &Reporter::hidden(),
        )
        .unwrap();

        run_verify(&target, &[diff.as_path()], &snap2, &Reporter::hidden())
            .expect("single-file verify should pass");
    }
}
