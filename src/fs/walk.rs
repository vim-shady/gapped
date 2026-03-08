use crate::fs::metadata::collect_metadata;
use crate::model::entry::{Entry, EntryKind};
use crate::model::path::RelativePath;
use anyhow::Result;
use std::path::Path;
use walkdir::WalkDir;

#[derive(Debug, Default)]
pub struct WalkStats {
    pub total_entries: usize,
    pub files: usize,
    pub directories: usize,
    pub symlinks: usize,
    pub errors: usize,
}

/// Walk the filesystem under 'root' and return a sorted list of entries
pub fn walk_filesystem(root: &Path) -> Result<(Vec<Entry>, WalkStats)> {
    let mut stats = WalkStats::default();

    let mut raw_entries = Vec::new();

    for dir_entry_result in WalkDir::new(root).follow_links(false) {
        let dir_entry = match dir_entry_result {
            Ok(dir_entry) => dir_entry,
            Err(e) => {
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
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            }
        };

        let (metadata, file_type) = match collect_metadata(full_path) {
            Ok(metadata) => metadata,
            Err(_) => {
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
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            }
        } else {
            // skip special files (sockets, pipes, ...)
            continue;
        }
        stats.total_entries += 1;
        raw_entries.push((relative_path, kind, metadata, symlink_target));
    }

    let mut result_entries: Vec<Entry> = raw_entries
        .into_iter()
        .map(|(relative_path, kind, metadata, symlink_target)| Entry {
            path: relative_path,
            kind,
            metadata,
            hash: None,
            symlink_target,
        })
        .collect();

    result_entries.sort_by(|a, b| a.path.cmp(&b.path));
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
        let (entries, stats) = walk_filesystem(tmp.path()).unwrap();

        assert_eq!(stats.directories, 4);
        assert_eq!(stats.files, 3);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.total_entries, 8);
        assert_eq!(stats.errors, 0);
        assert_eq!(entries.len(), 8);
    }

    #[test]
    fn test_entries_sorted_by_path() {
        let tmp = setup_test_tree();
        let (entries, _) = walk_filesystem(tmp.path()).unwrap();

        let paths: Vec<String> = entries.iter().map(|e| e.path.to_string()).collect();

        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }
}
