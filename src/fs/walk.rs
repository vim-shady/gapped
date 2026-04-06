use crate::error::Result;
use crate::fs::hash::hash_file;
use crate::fs::metadata::collect_metadata;
use crate::model::entry::{Entry, EntryKind};
use crate::model::path::RelativePath;
use log::{info, warn};
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use std::collections::HashMap;
use std::path::Path;
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
// TODO: Refactor this ugly creation...
/// Walk the filesystem under 'root' and return a sorted list of entries
pub fn walk_filesystem(
    root: &Path,
    previous_entries: Option<&HashMap<RelativePath, Entry>>,
) -> Result<(Vec<Entry>, WalkStats)> {
    let mut stats = WalkStats::default();

    let mut raw_entries = Vec::new();

    for dir_entry_result in WalkDir::new(root).follow_links(false) {
        let dir_entry = match dir_entry_result {
            Ok(dir_entry) => dir_entry,
            Err(e) => {
                warn!("Error while walking filesystem: {}", e);
                stats.errors += 1;
                continue;
            }
        };

        let full_path = dir_entry.path();

        let relative_path = if full_path == root {
            RelativePath::root()
        } else {
            match RelativePath::from_full_path(full_path, root) {
                Ok(relative_path) => relative_path,
                Err(e) => {
                    warn!("Error while walking filesystem: {}", e);
                    stats.errors += 1;
                    continue;
                }
            }
        };

        let (metadata, file_type) = match collect_metadata(full_path) {
            Ok(metadata) => metadata,
            Err(e) => {
                warn!("Cannot read metadata for {}: {}", full_path.display(), e);
                stats.errors += 1;
                continue;
            }
        };

        let kind: EntryKind;
        let mut symlink_target = None;

        if file_type.is_file() {
            kind = EntryKind::File;
            stats.files += 1;
        } else if file_type.is_dir() {
            kind = EntryKind::Directory;
            stats.directories += 1;
        } else if file_type.is_symlink() {
            kind = EntryKind::Symlink;
            stats.symlinks += 1;
            match std::fs::read_link(full_path) {
                Ok(target) => symlink_target = Some(target),
                Err(e) => {
                    warn!("Cannot read symlink {}: {}", full_path.display(), e);
                    stats.errors += 1;
                    continue;
                }
            }
        } else {
            // skip special files (sockets, pipes, ...)
            warn!("Skipping special file {}", full_path.display());
            continue;
        }
        stats.total_entries += 1;
        raw_entries.push((relative_path, kind, metadata, symlink_target));
    }

    // Compute hashes for files
    let root_owned = root.to_path_buf();
    let entries: Vec<Option<Entry>> = raw_entries
        .par_iter()
        .map(|(rel_path, kind, metadata, link_target)| {
            let hash = if *kind == EntryKind::File {
                // Check if we can reuse an old hash
                if let Some(prev) = previous_entries {
                    if let Some(prev_entry) = prev.get(rel_path) {
                        if prev_entry.kind == EntryKind::File
                            && prev_entry.metadata.size_and_mtime_match(metadata)
                        {
                            return Some(Entry {
                                path: rel_path.clone(),
                                kind: kind.clone(),
                                metadata: metadata.clone(),
                                hash: prev_entry.hash,
                                symlink_target: None,
                            });
                        }
                    }
                }

                let full_path = rel_path.to_full_path(&root_owned);
                match hash_file(&full_path) {
                    Ok(hash) => Some(hash),
                    Err(e) => {
                        warn!("Cannot hash {}: {}", full_path.display(), e);
                        return None;
                    }
                }
            } else {
                None
            };

            Some(Entry {
                path: rel_path.clone(),
                kind: kind.clone(),
                metadata: metadata.clone(),
                hash,
                symlink_target: link_target.clone(),
            })
        })
        .collect();

    // Collect results, filtering out errors, count hash stats
    let mut result_entries: Vec<Entry> = Vec::with_capacity(entries.len());
    for (i, opt_entry) in entries.into_iter().enumerate() {
        match opt_entry {
            Some(entry) => {
                if entry.kind == EntryKind::File {
                    let (rel_path, _, metadata, _) = &raw_entries[i];
                    // Determine if hash was reused
                    if let Some(prev) = previous_entries {
                        if let Some(prev_entry) = prev.get(rel_path) {
                            if prev_entry.kind == EntryKind::File
                                && prev_entry.metadata.size_and_mtime_match(metadata)
                            {
                                stats.files_hash_reused += 1;
                            } else {
                                stats.files_hashed += 1;
                            }
                        } else {
                            stats.files_hashed += 1;
                        }
                    } else {
                        stats.files_hashed += 1;
                    }
                }
                result_entries.push(entry);
            }
            None => {
                stats.errors += 1;
            }
        }
    }

    // Sort by path
    result_entries.sort_by(|a, b| a.path.cmp(&b.path));

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

    Ok((result_entries, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let (entries, stats) = walk_filesystem(tmp.path(), None).unwrap();

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
        let (entries, _) = walk_filesystem(tmp.path(), None).unwrap();

        let paths: Vec<String> = entries.iter().map(|e| e.path.to_string()).collect();

        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn test_hash_reuse() {
        let tmp = setup_test_tree();
        let root = tmp.path();

        let (entries, stats) = walk_filesystem(root, None).unwrap();
        assert_eq!(stats.files_hashed, 3);
        assert_eq!(stats.files_hash_reused, 0);

        let prev_entries: HashMap<RelativePath, Entry> = entries
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();

        std::fs::write(root.join("top.txt"), "changed").unwrap();

        // Second walk - 2 reused, 1 rehashed
        let (_, stats) = walk_filesystem(root, Some(&prev_entries)).unwrap();
        assert_eq!(stats.files_hashed, 1);
        assert_eq!(stats.files_hash_reused, 2);
    }
}
