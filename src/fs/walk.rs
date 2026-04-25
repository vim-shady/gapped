use crate::commands::snapshot::SnapshotReader;
use crate::error::Result;
use crate::fs::hash::hash_file;
use crate::fs::metadata::collect_metadata;
use crate::model::entry::{Entry, EntryKind, Metadata};
use crate::model::path::RelativePath;
use crate::progress::Reporter;
use indicatif::ProgressBar;
use log::{info, warn};
use rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator};
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
        walk_pb.inc(1);
        raw_entries.push((relative_path, kind, metadata, symlink_target));
    }
    walk_pb.finish_with_message(format!("Walked {} entries", stats.total_entries));

    // Compute hashes for files in parallel, tracking hash outcome for stats
    #[derive(Clone, Copy)]
    enum HashOutcome {
        Hashed,
        Reused,
        NotAFile,
        Error,
    }

    let hash_pb = reporter.counter("Hashing files", stats.files as u64);
    let root_owned = root.to_path_buf();
    let results: Vec<(Option<Entry>, HashOutcome)> = raw_entries
        .par_iter()
        .map(|(rel_path, kind, metadata, link_target)| {
            if *kind != EntryKind::File {
                return (
                    Some(Entry {
                        path: rel_path.clone(),
                        kind: *kind,
                        metadata: metadata.clone(),
                        hash: None,
                        symlink_target: link_target.clone(),
                    }),
                    HashOutcome::NotAFile,
                );
            }

            // Check if we can reuse an old hash
            if let Some(prev) = previous_entries
                && let Ok(idx) = prev.binary_search_by(|e| e.path.cmp(rel_path))
            {
                let prev_entry = &prev[idx];
                if prev_entry.kind == EntryKind::File
                    && prev_entry.metadata.size_and_mtime_match(metadata)
                {
                    hash_pb.inc(1);
                    return (
                        Some(Entry {
                            path: rel_path.clone(),
                            kind: *kind,
                            metadata: metadata.clone(),
                            hash: prev_entry.hash,
                            symlink_target: None,
                        }),
                        HashOutcome::Reused,
                    );
                }
            }

            let full_path = rel_path.to_full_path(&root_owned);
            let out = match hash_file(&full_path) {
                Ok(hash) => (
                    Some(Entry {
                        path: rel_path.clone(),
                        kind: *kind,
                        metadata: metadata.clone(),
                        hash: Some(hash),
                        symlink_target: link_target.clone(),
                    }),
                    HashOutcome::Hashed,
                ),
                Err(e) => {
                    warn!("Cannot hash {}: {}", full_path.display(), e);
                    (None, HashOutcome::Error)
                }
            };
            hash_pb.inc(1);
            out
        })
        .collect();

    // Collect results and stats in a single pass
    let mut result_entries: Vec<Entry> = Vec::with_capacity(results.len());
    for (opt_entry, outcome) in results {
        match outcome {
            HashOutcome::Hashed => stats.files_hashed += 1,
            HashOutcome::Reused => stats.files_hash_reused += 1,
            HashOutcome::Error => stats.errors += 1,
            HashOutcome::NotAFile => {}
        }
        if let Some(entry) = opt_entry {
            result_entries.push(entry);
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

/// Entries per rayon batch in the streaming walk. With the 8-thread worker
/// cap, 512 = 8 threads × 64 items/thread — enough to amortize rayon's
/// per-batch scheduling overhead while keeping peak resident entries bounded
/// (~150 KiB per batch) independent of total tree size.
const STREAM_BATCH_SIZE: usize = 512;

struct PendingEntry {
    rel_path: RelativePath,
    kind: EntryKind,
    metadata: Metadata,
    symlink_target: Option<PathBuf>,
    old: Option<Entry>,
    /// Old-snapshot entries that sort before this one — emitted as removals.
    removals_before: Vec<Entry>,
}

#[derive(Debug)]
pub enum WalkItem {
    Added(Entry),
    Removed(Entry),
    Both { new: Entry, old: Entry },
}

/// Streaming filesystem walk that yields sorted, hashed [`WalkItem`]s
/// without materialising the whole tree in memory. Entries are collected
/// in batches, hashed in parallel, and merge-joined against an optional
/// old [`SnapshotReader`].
pub struct WalkStream<'a> {
    iter: walkdir::IntoIter,
    root: PathBuf,
    prev: Option<SnapshotReader>,
    prev_peek: Option<Entry>,
    ready: std::vec::IntoIter<WalkItem>,
    walk_exhausted: bool,
    finished: bool,
    stats: WalkStats,
    walk_pb: ProgressBar,
    hash_pb: Option<ProgressBar>,
    reporter: &'a Reporter,
}

impl<'a> WalkStream<'a> {
    pub fn new(root: &Path, prev: Option<SnapshotReader>, reporter: &'a Reporter) -> Self {
        let walk_pb = reporter.spinner("Walking filesystem");
        let iter = WalkDir::new(root)
            .follow_links(false)
            .sort_by(|a, b| a.file_name().cmp(b.file_name()))
            .into_iter();
        Self {
            iter,
            root: root.to_path_buf(),
            prev,
            prev_peek: None,
            ready: Vec::new().into_iter(),
            walk_exhausted: false,
            finished: false,
            stats: WalkStats::default(),
            walk_pb,
            hash_pb: None,
            reporter,
        }
    }

    pub fn into_stats(self) -> WalkStats {
        self.stats
    }

    /// Advance the old-snapshot stream up to `path`. Entries before `path`
    /// are appended to `removals`. Returns the old entry at `path` if one
    /// exists; entries after `path` stay peeked.
    fn merge_up_to(
        &mut self,
        path: &RelativePath,
        removals: &mut Vec<Entry>,
    ) -> Result<Option<Entry>> {
        loop {
            if self.prev_peek.is_none() {
                let Some(reader) = self.prev.as_mut() else {
                    return Ok(None);
                };
                self.prev_peek = reader.next_entry()?;
                if self.prev_peek.is_none() {
                    return Ok(None);
                }
            }
            match self.prev_peek.as_ref().unwrap().path.cmp(path) {
                std::cmp::Ordering::Less => {
                    removals.push(self.prev_peek.take().unwrap());
                }
                std::cmp::Ordering::Equal => return Ok(self.prev_peek.take()),
                std::cmp::Ordering::Greater => return Ok(None),
            }
        }
    }

    fn drain_remaining_old(&mut self, out: &mut Vec<WalkItem>) -> Result<()> {
        if let Some(entry) = self.prev_peek.take() {
            out.push(WalkItem::Removed(entry));
        }
        if let Some(reader) = self.prev.as_mut() {
            while let Some(e) = reader.next_entry()? {
                out.push(WalkItem::Removed(e));
            }
        }
        Ok(())
    }

    fn collect_batch(&mut self, batch: &mut Vec<PendingEntry>) -> Result<()> {
        batch.clear();
        while batch.len() < STREAM_BATCH_SIZE {
            let Some(dir_entry_result) = self.iter.next() else {
                self.walk_exhausted = true;
                return Ok(());
            };
            let dir_entry = match dir_entry_result {
                Ok(v) => v,
                Err(e) => {
                    warn!("Error while walking filesystem: {}", e);
                    self.stats.errors += 1;
                    continue;
                }
            };
            let full_path = dir_entry.path();

            let rel_path = if full_path == self.root {
                RelativePath::root()
            } else {
                match RelativePath::from_full_path(full_path, &self.root) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Error while walking filesystem: {}", e);
                        self.stats.errors += 1;
                        continue;
                    }
                }
            };

            let (metadata, file_type) = match collect_metadata(full_path) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Cannot read metadata for {}: {}", full_path.display(), e);
                    self.stats.errors += 1;
                    continue;
                }
            };

            let (kind, symlink_target) = if file_type.is_file() {
                self.stats.files += 1;
                (EntryKind::File, None)
            } else if file_type.is_dir() {
                self.stats.directories += 1;
                (EntryKind::Directory, None)
            } else if file_type.is_symlink() {
                self.stats.symlinks += 1;
                match std::fs::read_link(full_path) {
                    Ok(target) => (EntryKind::Symlink, Some(target)),
                    Err(e) => {
                        warn!("Cannot read symlink {}: {}", full_path.display(), e);
                        self.stats.errors += 1;
                        continue;
                    }
                }
            } else {
                warn!("Skipping special file {}", full_path.display());
                continue;
            };

            self.stats.total_entries += 1;
            self.walk_pb.inc(1);

            let mut removals_before = Vec::new();
            let old = self.merge_up_to(&rel_path, &mut removals_before)?;

            batch.push(PendingEntry {
                rel_path,
                kind,
                metadata,
                symlink_target,
                old,
                removals_before,
            });
        }
        Ok(())
    }
}

impl<'a> Iterator for WalkStream<'a> {
    type Item = Result<WalkItem>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.ready.next() {
                return Some(Ok(item));
            }
            if self.finished {
                self.walk_pb
                    .finish_with_message(format!("Walked {} entries", self.stats.total_entries));
                if let Some(pb) = self.hash_pb.take() {
                    pb.finish_and_clear();
                }
                return None;
            }

            if !self.walk_exhausted {
                let mut batch = Vec::with_capacity(STREAM_BATCH_SIZE);
                if let Err(e) = self.collect_batch(&mut batch) {
                    return Some(Err(e));
                }
                if batch.is_empty() {
                    // walkdir produced nothing new; either it has hit EOF
                    // (walk_exhausted is set), or we filtered everything.
                    // Loop: next iteration drains remaining old entries.
                    continue;
                }
                if self.hash_pb.is_none() && batch.iter().any(|p| p.kind == EntryKind::File) {
                    self.hash_pb = Some(self.reporter.spinner("Hashing files"));
                }

                let root = self.root.clone();
                let hash_pb = self.hash_pb.clone();
                let hashed: Vec<_> = batch
                    .into_par_iter()
                    .map(|p| hash_pending(p, &root, hash_pb.as_ref()))
                    .collect();

                let mut ready: Vec<WalkItem> = Vec::with_capacity(hashed.len());
                for (removals, entry) in hashed {
                    for old in removals {
                        ready.push(WalkItem::Removed(old));
                    }
                    match entry {
                        Some((new, old)) => {
                            if new.kind == EntryKind::File {
                                self.stats.files_hashed += 1;
                            }
                            ready.push(match old {
                                Some(old) => WalkItem::Both { new, old },
                                None => WalkItem::Added(new),
                            });
                        }
                        None => self.stats.errors += 1,
                    }
                }
                self.ready = ready.into_iter();
            } else {
                let mut tail = Vec::new();
                if let Err(e) = self.drain_remaining_old(&mut tail) {
                    return Some(Err(e));
                }
                self.ready = tail.into_iter();
                self.finished = true;
            }
        }
    }
}

fn hash_pending(
    p: PendingEntry,
    root: &Path,
    hash_pb: Option<&ProgressBar>,
) -> (Vec<Entry>, Option<(Entry, Option<Entry>)>) {
    let hash = if p.kind == EntryKind::File {
        let reused = p
            .old
            .as_ref()
            .filter(|o| o.kind == EntryKind::File && o.metadata.size_and_mtime_match(&p.metadata))
            .and_then(|o| o.hash);
        let hash = match reused {
            Some(h) => Some(h),
            None => {
                let full_path = p.rel_path.to_full_path(root);
                match hash_file(&full_path) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        warn!("Cannot hash {}: {}", full_path.display(), e);
                        return (p.removals_before, None);
                    }
                }
            }
        };
        if let Some(pb) = hash_pb {
            pb.inc(1);
        }
        hash
    } else {
        None
    };

    let new = Entry {
        path: p.rel_path,
        kind: p.kind,
        metadata: p.metadata,
        hash,
        symlink_target: p.symlink_target,
    };
    (p.removals_before, Some((new, p.old)))
}

/// Convenience wrapper used by walk tests: drives `WalkStream` to
/// completion and collects new-tree entries into a `Vec` for simple
/// assertions. Not used by production code — the diff path consumes
/// `WalkStream` directly.
#[cfg(test)]
pub fn walk_filesystem_streaming(
    root: &Path,
    prev: Option<SnapshotReader>,
    reporter: &Reporter,
) -> Result<(Vec<Entry>, WalkStats)> {
    let mut stream = WalkStream::new(root, prev, reporter);
    let mut entries = Vec::new();
    for item in stream.by_ref() {
        match item? {
            WalkItem::Added(e) | WalkItem::Both { new: e, .. } => entries.push(e),
            WalkItem::Removed(_) => {}
        }
    }
    Ok((entries, stream.into_stats()))
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

    /// Streaming walk must produce the same sorted entries as the classic
    /// Vec-returning one. Any divergence would silently corrupt diffs.
    #[test]
    fn test_stream_matches_classic() {
        let tmp = setup_test_tree();
        let root = tmp.path();

        let (classic, _) = walk_filesystem(root, None, &Reporter::hidden()).unwrap();
        let (streamed, _) = walk_filesystem_streaming(root, None, &Reporter::hidden()).unwrap();

        assert_eq!(classic.len(), streamed.len(), "entry count mismatch");
        for (a, b) in classic.iter().zip(streamed.iter()) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.metadata, b.metadata);
            assert_eq!(a.hash, b.hash);
            assert_eq!(a.symlink_target, b.symlink_target);
        }
    }

    /// Merge-join via `WalkStream`: when old and new trees share some
    /// entries and differ on others, the stream must emit `Added`,
    /// `Removed`, and `Both` items in path-sorted order. This is the
    /// single contract run_diff depends on for its memory-bounded
    /// streaming diff.
    #[test]
    fn test_stream_merge_join_emits_added_removed_both() {
        use crate::commands::snapshot::{run_snapshot, SnapshotReader};
        let tmp = setup_test_tree();
        let root = tmp.path();
        let snap_dir = TempDir::new().unwrap();
        let snap_path = snap_dir.path().join("snap");
        run_snapshot(root, &snap_path, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Mutate tree: remove one file, add one file, modify one file.
        std::fs::remove_file(root.join("a/nested.txt")).unwrap();
        std::fs::write(root.join("brand_new.txt"), "fresh").unwrap();
        std::fs::write(root.join("top.txt"), "modified").unwrap();

        let (reader, _) = SnapshotReader::open(&snap_path).unwrap();
        let reporter = Reporter::hidden();
        let stream = WalkStream::new(root, Some(reader), &reporter);
        let items: Vec<WalkItem> = stream.collect::<Result<Vec<_>>>().unwrap();

        // Project to (path, label) so ordering + classification assertions
        // are legible.
        let labeled: Vec<(String, &'static str)> = items
            .iter()
            .map(|it| match it {
                WalkItem::Added(e) => (e.path.to_string(), "A"),
                WalkItem::Removed(e) => (e.path.to_string(), "R"),
                WalkItem::Both { new, .. } => (new.path.to_string(), "B"),
            })
            .collect();

        // Must be overall path-sorted.
        let mut sorted = labeled.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(labeled, sorted, "WalkStream output not path-sorted");

        // Specific classifications we must see.
        assert!(
            labeled
                .iter()
                .any(|(p, l)| p == "a/nested.txt" && *l == "R")
        );
        assert!(
            labeled
                .iter()
                .any(|(p, l)| p == "brand_new.txt" && *l == "A")
        );
        assert!(labeled.iter().any(|(p, l)| p == "top.txt" && *l == "B"));
    }

    /// Streamed walk with an old `SnapshotReader` should re-use hashes for
    /// unchanged files just like the binary-search path, with the added
    /// benefit of not loading the old snapshot into memory.
    #[test]
    fn test_stream_hash_reuse_via_snapshot_reader() {
        use crate::commands::snapshot::{run_snapshot, SnapshotReader};
        let tmp = setup_test_tree();
        let root = tmp.path();
        // Snapshot file kept OUTSIDE the walked tree so the walk sees the
        // same three files both times.
        let snap_dir = TempDir::new().unwrap();
        let snap_path = snap_dir.path().join("snap");
        run_snapshot(root, &snap_path, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(root.join("top.txt"), "modified").unwrap();

        // Capture pre-modification hashes to compare against.
        let (pre, _) = walk_filesystem(root, None, &Reporter::hidden()).unwrap();
        let pre_nested_hash = pre
            .iter()
            .find(|e| e.path.to_string() == "a/nested.txt")
            .and_then(|e| e.hash)
            .unwrap();

        let (reader, _) = SnapshotReader::open(&snap_path).unwrap();
        let (streamed, _) =
            walk_filesystem_streaming(root, Some(reader), &Reporter::hidden()).unwrap();

        let files: Vec<_> = streamed
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .collect();
        assert_eq!(files.len(), 3);
        for e in files {
            assert!(e.hash.is_some(), "hash missing for {}", e.path);
        }
        // Untouched file: hash must match pre-modification (reused from old snapshot).
        let nested = streamed
            .iter()
            .find(|e| e.path.to_string() == "a/nested.txt")
            .unwrap();
        assert_eq!(
            nested.hash.unwrap(),
            pre_nested_hash,
            "a/nested.txt hash should have been reused from the old snapshot"
        );
    }
}
