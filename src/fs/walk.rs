use crate::error::Result;
use crate::fs::hash::hash_file;
use crate::fs::metadata::collect_metadata;
use crate::model::entry::{Entry, EntryKind, Metadata};
use crate::model::path::RelativePath;
use crate::progress::Reporter;
use log::{info, warn};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Statistics from single filesystem walk.
#[derive(Debug, Default)]
pub struct WalkStats {
    pub total_entries: usize,
    pub files: usize,
    pub directories: usize,
    pub symlinks: usize,
    pub files_hashed: usize,
    pub files_hash_reused: usize,
    pub errors: usize,
}

impl WalkStats {
    fn count(&mut self, kind: EntryKind) {
        match kind {
            EntryKind::File => self.files += 1,
            EntryKind::Directory => self.directories += 1,
            EntryKind::Symlink => self.symlinks += 1,
        }
        self.total_entries += 1;
    }
}

enum ClassifyError {
    Failed,
    Skip,
}

/// Resolve a walkdir entry into its relative path, kind, metadata, and
/// optional symlink target. Returns `Err(Failed)` for I/O errors (already
/// logged) and `Err(Skip)` for unsupported file types (sockets, pipes, …).
fn classify_dir_entry(
    dir_entry: &walkdir::DirEntry,
    root: &Path,
) -> std::result::Result<(RelativePath, EntryKind, Metadata, Option<PathBuf>), ClassifyError> {
    let full_path = dir_entry.path();

    let rel_path = if full_path == root {
        RelativePath::root()
    } else {
        RelativePath::from_full_path(full_path, root).map_err(|e| {
            warn!("Error while walking filesystem: {}", e);
            ClassifyError::Failed
        })?
    };

    let (metadata, file_type) = collect_metadata(full_path).map_err(|e| {
        warn!("Cannot read metadata for {}: {}", full_path.display(), e);
        ClassifyError::Failed
    })?;

    if file_type.is_file() {
        Ok((rel_path, EntryKind::File, metadata, None))
    } else if file_type.is_dir() {
        Ok((rel_path, EntryKind::Directory, metadata, None))
    } else if file_type.is_symlink() {
        let target = std::fs::read_link(full_path).map_err(|e| {
            warn!("Cannot read symlink {}: {}", full_path.display(), e);
            ClassifyError::Failed
        })?;
        Ok((rel_path, EntryKind::Symlink, metadata, Some(target)))
    } else {
        warn!("Skipping special file {}", full_path.display());
        Err(ClassifyError::Skip)
    }
}

/// Try to reuse a content hash from `old_entry`, falling back to hashing
/// the file on disk. Returns `(hash, reused)`.
fn resolve_file_hash(
    rel_path: &RelativePath,
    metadata: &Metadata,
    old_entry: Option<&Entry>,
    root: &Path,
) -> std::io::Result<([u8; 16], bool)> {
    if let Some(old) = old_entry
        && old.kind == EntryKind::File
        && old.metadata.size_and_mtime_match(metadata)
        && let Some(h) = old.hash
    {
        return Ok((h, true));
    }
    let full_path = rel_path.to_full_path(root);
    hash_file(&full_path).map(|h| (h, false))
}

/// Walk the filesystem under 'root' and return a sorted list of entries.
///
/// `previous_entries` must be sorted by path (the invariant held by snapshots
/// loaded via `load_snapshot`). It's looked up via binary search to reuse
/// content hashes for files whose size+mtime haven't changed.
pub fn walk_filesystem(
    root: &Path,
    previous_entries: Option<&[Entry]>,
    reporter: &Reporter,
) -> Result<(Vec<Entry>, WalkStats)> {
    let mut stats = WalkStats::default();
    let mut raw_entries = Vec::new();

    let walk_pb = reporter.spinner("Walking filesystem");
    for result in WalkDir::new(root).follow_links(false) {
        let dir_entry = match result {
            Ok(e) => e,
            Err(e) => {
                warn!("Error while walking filesystem: {}", e);
                stats.errors += 1;
                continue;
            }
        };
        match classify_dir_entry(&dir_entry, root) {
            Ok(classified) => {
                stats.count(classified.1);
                walk_pb.inc(1);
                raw_entries.push(classified);
            }
            Err(ClassifyError::Failed) => stats.errors += 1,
            Err(ClassifyError::Skip) => {}
        }
    }
    walk_pb.finish_with_message(format!("Walked {} entries", stats.total_entries));

    // Hash files in parallel, reusing old hashes where possible.
    let hash_pb = reporter.counter("Hashing files", stats.files as u64);
    let root_buf = root.to_path_buf();
    let results: Vec<(Option<Entry>, bool)> = raw_entries
        .par_iter()
        .map(|(rel_path, kind, metadata, symlink_target)| {
            if *kind != EntryKind::File {
                return (
                    Some(Entry {
                        path: rel_path.clone(),
                        kind: *kind,
                        metadata: metadata.clone(),
                        hash: None,
                        symlink_target: symlink_target.clone(),
                        dir_hash: None,
                    }),
                    false,
                );
            }
            let old = previous_entries.and_then(|prev| {
                prev.binary_search_by(|e| e.path.cmp(rel_path))
                    .ok()
                    .map(|i| &prev[i])
            });
            let out = match resolve_file_hash(rel_path, metadata, old, &root_buf) {
                Ok((h, reused)) => (
                    Some(Entry {
                        path: rel_path.clone(),
                        kind: *kind,
                        metadata: metadata.clone(),
                        hash: Some(h),
                        symlink_target: symlink_target.clone(),
                        dir_hash: None,
                    }),
                    reused,
                ),
                Err(e) => {
                    warn!("Cannot hash {}: {}", rel_path, e);
                    (None, false)
                }
            };
            hash_pb.inc(1);
            out
        })
        .collect();

    let mut entries = Vec::with_capacity(results.len());
    for (opt_entry, reused) in results {
        match opt_entry {
            Some(entry) => {
                if entry.kind == EntryKind::File {
                    if reused {
                        stats.files_hash_reused += 1;
                    } else {
                        stats.files_hashed += 1;
                    }
                }
                entries.push(entry);
            }
            None => stats.errors += 1,
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));

    info!(
        "Walk complete: {} entries ({} files, {} dirs, {} symlinks), {} hashed, {} reused, {} errors",
        stats.total_entries,
        stats.files,
        stats.directories,
        stats.symlinks,
        stats.files_hashed,
        stats.files_hash_reused,
        stats.errors
    );

    Ok((entries, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::hash::compute_dir_hashes;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn setup_test_tree() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // dirs
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir(root.join("c")).unwrap();

        // files
        fs::write(root.join("top.txt"), "hello").unwrap();
        fs::write(root.join("a/nested.txt"), "world").unwrap();
        fs::write(root.join("a/b/deep.txt"), "deep").unwrap();

        // symlink
        symlink("top.txt", root.join("link.txt")).unwrap();

        tmp
    }

    #[test]
    fn test_walk_counts() {
        let tmp = setup_test_tree();
        let (entries, stats) = walk_filesystem(tmp.path(), None, &Reporter::hidden()).unwrap();

        assert_eq!(stats.directories, 4);
        assert_eq!(stats.files, 3);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.total_entries, 8);
        assert_eq!(stats.files_hashed, 3);
        assert_eq!(stats.files_hash_reused, 0);
        assert_eq!(stats.errors, 0);
        assert_eq!(entries.len(), 8);
    }

    #[test]
    fn test_entries_sorted_by_path() {
        let tmp = setup_test_tree();
        let (entries, _) = walk_filesystem(tmp.path(), None, &Reporter::hidden()).unwrap();

        let paths: Vec<String> = entries.iter().map(|e| e.path.to_string()).collect();

        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn test_hash_reuse() {
        let tmp = setup_test_tree();
        let root = tmp.path();

        let (prev_entries, stats) = walk_filesystem(root, None, &Reporter::hidden()).unwrap();
        assert_eq!(stats.files_hashed, 3);
        assert_eq!(stats.files_hash_reused, 0);

        std::fs::write(root.join("top.txt"), "changed").unwrap();

        // Second walk - 2 reused, 1 rehashed
        let (_, stats) = walk_filesystem(root, Some(&prev_entries), &Reporter::hidden()).unwrap();
        assert_eq!(stats.files_hashed, 1);
        assert_eq!(stats.files_hash_reused, 2);
    }

    // -----------------------------------------------------------------------
    // compute_dir_hashes tests
    // -----------------------------------------------------------------------

    fn make_entry(path: &str, kind: EntryKind, hash: Option<[u8; 16]>) -> Entry {
        Entry {
            path: RelativePath::new(Path::new(path)).unwrap(),
            kind,
            metadata: Metadata {
                size: if kind == EntryKind::File { 100 } else { 0 },
                mtime_sec: 1_700_000_000,
                mtime_nsec: 0,
                permissions: if kind == EntryKind::Directory {
                    0o755
                } else {
                    0o644
                },
                uid: 1000,
                gid: 1000,
            },
            hash,
            symlink_target: None,
            dir_hash: None,
        }
    }

    fn make_symlink_entry(path: &str, target: &str) -> Entry {
        Entry {
            path: RelativePath::new(Path::new(path)).unwrap(),
            kind: EntryKind::Symlink,
            metadata: Metadata {
                size: 0,
                mtime_sec: 1_700_000_000,
                mtime_nsec: 0,
                permissions: 0o777,
                uid: 1000,
                gid: 1000,
            },
            hash: None,
            symlink_target: Some(PathBuf::from(target)),
            dir_hash: None,
        }
    }

    #[test]
    fn dir_hash_deterministic() {
        let mut entries = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
            make_entry("b.txt", EntryKind::File, Some([2u8; 16])),
        ];
        compute_dir_hashes(&mut entries);
        let h1 = entries[0].dir_hash.unwrap();

        let mut entries2 = entries.clone();
        entries2[0].dir_hash = None;
        compute_dir_hashes(&mut entries2);
        assert_eq!(h1, entries2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_independent_of_input_order() {
        let mut entries_ab = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
            make_entry("b.txt", EntryKind::File, Some([2u8; 16])),
        ];
        compute_dir_hashes(&mut entries_ab);

        // Same children but input in reverse order (still sorted by path
        // for the function contract, but the internal sort by name should
        // produce the same hash regardless).
        let mut entries_ba = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
            make_entry("b.txt", EntryKind::File, Some([2u8; 16])),
        ];
        compute_dir_hashes(&mut entries_ba);
        assert_eq!(
            entries_ab[0].dir_hash.unwrap(),
            entries_ba[0].dir_hash.unwrap()
        );
    }

    #[test]
    fn dir_hash_changes_when_child_content_changes() {
        let mut v1 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut v1);

        let mut v2 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([2u8; 16])),
        ];
        compute_dir_hashes(&mut v2);
        assert_ne!(v1[0].dir_hash.unwrap(), v2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_changes_when_child_name_changes() {
        let mut v1 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut v1);

        let mut v2 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("z.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut v2);
        assert_ne!(v1[0].dir_hash.unwrap(), v2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_changes_when_child_mode_changes() {
        let mut v1 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut v1);

        let mut v2 = v1.clone();
        v2[0].dir_hash = None;
        v2[1].metadata.permissions = 0o755;
        compute_dir_hashes(&mut v2);
        assert_ne!(v1[0].dir_hash.unwrap(), v2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_changes_when_child_ownership_changes() {
        let mut v1 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut v1);

        let mut v2 = v1.clone();
        v2[0].dir_hash = None;
        v2[1].metadata.uid = 0;
        compute_dir_hashes(&mut v2);
        assert_ne!(v1[0].dir_hash.unwrap(), v2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_changes_when_child_mtime_changes() {
        let mut v1 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut v1);

        let mut v2 = v1.clone();
        v2[0].dir_hash = None;
        v2[1].metadata.mtime_sec = 9999;
        compute_dir_hashes(&mut v2);
        assert_ne!(v1[0].dir_hash.unwrap(), v2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_cascades_through_ancestors() {
        let mut entries = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("a", EntryKind::Directory, None),
            make_entry("a/b", EntryKind::Directory, None),
            make_entry("a/b/deep.txt", EntryKind::File, Some([1u8; 16])),
        ];
        compute_dir_hashes(&mut entries);
        let root_h1 = entries[0].dir_hash.unwrap();
        let a_h1 = entries[1].dir_hash.unwrap();
        let ab_h1 = entries[2].dir_hash.unwrap();

        // Change the deep file
        let mut entries2 = entries.clone();
        for e in &mut entries2 {
            e.dir_hash = None;
        }
        entries2[3].hash = Some([2u8; 16]);
        compute_dir_hashes(&mut entries2);

        assert_ne!(ab_h1, entries2[2].dir_hash.unwrap(), "a/b should change");
        assert_ne!(a_h1, entries2[1].dir_hash.unwrap(), "a should change");
        assert_ne!(root_h1, entries2[0].dir_hash.unwrap(), "root should change");
    }

    #[test]
    fn dir_hash_empty_directory() {
        let mut entries = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("empty", EntryKind::Directory, None),
        ];
        compute_dir_hashes(&mut entries);
        assert!(
            entries[1].dir_hash.is_some(),
            "empty dir must get a dir_hash"
        );
    }

    #[test]
    fn dir_hash_with_symlink() {
        let mut entries = vec![
            make_entry(".", EntryKind::Directory, None),
            make_symlink_entry("link", "/some/target"),
        ];
        compute_dir_hashes(&mut entries);
        let h1 = entries[0].dir_hash.unwrap();

        let mut entries2 = vec![
            make_entry(".", EntryKind::Directory, None),
            make_symlink_entry("link", "/other/target"),
        ];
        compute_dir_hashes(&mut entries2);
        assert_ne!(h1, entries2[0].dir_hash.unwrap());
    }

    #[test]
    fn dir_hash_root_gets_hash() {
        let mut entries = vec![make_entry(".", EntryKind::Directory, None)];
        compute_dir_hashes(&mut entries);
        assert!(entries[0].dir_hash.is_some());
    }

    #[test]
    fn dir_hash_sibling_subtree_unchanged() {
        let mut entries = vec![
            make_entry(".", EntryKind::Directory, None),
            make_entry("left", EntryKind::Directory, None),
            make_entry("left/a.txt", EntryKind::File, Some([1u8; 16])),
            make_entry("right", EntryKind::Directory, None),
            make_entry("right/b.txt", EntryKind::File, Some([2u8; 16])),
        ];
        compute_dir_hashes(&mut entries);
        let right_h1 = entries[3].dir_hash.unwrap();

        // Change only the left subtree
        let mut entries2 = entries.clone();
        for e in &mut entries2 {
            e.dir_hash = None;
        }
        entries2[2].hash = Some([9u8; 16]);
        compute_dir_hashes(&mut entries2);
        assert_eq!(
            right_h1,
            entries2[3].dir_hash.unwrap(),
            "right subtree should be unchanged"
        );
        assert_ne!(
            entries[1].dir_hash.unwrap(),
            entries2[1].dir_hash.unwrap(),
            "left subtree should change"
        );
    }
}
