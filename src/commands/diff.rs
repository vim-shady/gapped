use crate::commands::snapshot::SnapshotReader;
use crate::error::{GappedError, Result};
use crate::format::header::{FileHeader, RECORD_HEADER_SIZE};
use crate::format::writer::FormatWriter;
use crate::fs::walk::{WalkItem, WalkStats, WalkStream};
use crate::model::diff::{AddedEntry, Change, ChangeKind, ModifiedEntry};
use crate::model::entry::{Entry, EntryKind};
use crate::parallel::{self, Chunk, ContentReader};
use crate::progress::Reporter;
use crossbeam_channel::{Receiver, Sender};
use log::info;
#[cfg(test)]
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

pub fn run_diff(
    root_dir: &Path,
    snapshot_in: &Path,
    diff_out: &Path,
    snapshot_out: &Path,
    split_size: Option<u64>,
    compress: bool,
    reporter: &Reporter,
) -> Result<()> {
    let root_dir = super::validate_root_dir(root_dir)?;

    info!("Hashing input snapshot {}", snapshot_in.display());
    let hash_pb = reporter.spinner("Hashing input snapshot");
    let source_snapshot_hash =
        crate::fs::hash::hash_file(snapshot_in).map_err(|e| GappedError::IoPath {
            path: snapshot_in.to_path_buf(),
            source: e,
        })?;
    hash_pb.finish_and_clear();

    // streaming merge-join
    info!("Opening input snapshot {}", snapshot_in.display());
    let (prev_reader, _old_header) = SnapshotReader::open(snapshot_in)?;

    let snap_file = File::create(snapshot_out)?;
    let snap_buf = BufWriter::new(snap_file);
    let snap_header = FileHeader::snapshot(&root_dir);
    let mut snap_writer = FormatWriter::maybe_compressed(snap_buf, &snap_header, compress)?;
    let snapshot_pb = reporter.spinner("Writing new snapshot");

    info!("Walking filesystem under {}", root_dir.display());
    let walk_stream = WalkStream::new(&root_dir, Some(prev_reader), reporter);
    let result = compute_changes(walk_stream, &mut snap_writer)?;

    snap_writer.finish()?;
    snapshot_pb.finish_with_message(format!(
        "Wrote {} snapshot entries",
        result.snapshot_entries
    ));

    if let Some(max_bytes) = split_size {
        write_split_diff(
            diff_out,
            &result.changes,
            source_snapshot_hash,
            &root_dir,
            max_bytes,
            compress,
            reporter,
        )?;
    } else {
        write_single_diff(
            diff_out,
            &result.changes,
            source_snapshot_hash,
            &root_dir,
            compress,
            reporter,
        )?;
    }

    eprintln!("Diff complete:");
    eprintln!("  Added: {}", result.added);
    eprintln!("  Modified: {}", result.modified);
    eprintln!("  Deleted: {}", result.removed);
    eprintln!("  Total changes: {}", result.changes.len());
    if result.stats.errors > 0 {
        eprintln!("  Walk errors: {}", result.stats.errors);
    }

    Ok(())
}

struct DiffResult {
    changes: Vec<Change>,
    added: usize,
    modified: usize,
    removed: usize,
    snapshot_entries: u64,
    stats: WalkStats,
}

/// Consume a `WalkStream`, classify each item as a `Change`, and write new
/// entries to the snapshot on the fly.
fn compute_changes<W: Write>(
    mut walk: WalkStream<'_>,
    snap_writer: &mut FormatWriter<W>,
) -> Result<DiffResult> {
    let mut changes = Vec::new();
    let (mut added, mut modified, mut removed) = (0, 0, 0);
    let mut snapshot_entries: u64 = 0;

    for item in walk.by_ref() {
        match item? {
            WalkItem::Added(new) => {
                snap_writer.write_snapshot_entry(&new)?;
                snapshot_entries += 1;
                changes.push(build_added_change(&new));
                added += 1;
            }
            WalkItem::Both { new, old } => {
                snap_writer.write_snapshot_entry(&new)?;
                snapshot_entries += 1;
                if new.kind != old.kind {
                    changes.push(build_removed_change(&old));
                    changes.push(build_added_change(&new));
                    removed += 1;
                    added += 1;
                } else if let Some(change) = compute_entry_diff(&old, &new) {
                    changes.push(change);
                    modified += 1;
                }
            }
            WalkItem::Removed(old) => {
                changes.push(build_removed_change(&old));
                removed += 1;
            }
        }
    }

    Ok(DiffResult {
        changes,
        added,
        modified,
        removed,
        snapshot_entries,
        stats: walk.into_stats(),
    })
}

fn write_single_diff(
    diff_out: &Path,
    changes: &[Change],
    source_snapshot_hash: [u8; 16],
    root_dir: &Path,
    compress: bool,
    reporter: &Reporter,
) -> Result<()> {
    let file = File::create(diff_out)?;
    let buf_writer = BufWriter::new(file);

    let header = FileHeader::diff(source_snapshot_hash, None);
    let mut writer = FormatWriter::maybe_compressed(buf_writer, &header, compress)?;

    // Section 1: all DiffChange records.
    let meta_pb = reporter.counter("Writing diff metadata", changes.len() as u64);
    for change in changes {
        writer.write_diff_change(change)?;
        meta_pb.inc(1);
    }
    meta_pb.finish_with_message(format!("Wrote {} diff records", changes.len()));

    // Section 2: FileContent records, in change-list order. Reader threads
    // prefetch the next files while the writer streams the current one; the
    // writer itself stays serial so the zstd encoder + rolling hash see bytes
    // in order.
    let content_count = changes.iter().filter(|c| c.has_content()).count() as u64;
    let content_pb = reporter.counter("Writing diff content", content_count);
    let mut pool = PrefetchPool::new(content_paths(changes, root_dir));
    while let Some(mut reader) = pool.next()? {
        let size = reader.remaining();
        writer.write_file_content_from_reader(&mut reader, size)?;
        content_pb.inc(1);
    }
    pool.finish()?;
    content_pb.finish_with_message(format!("Wrote {} content files", content_count));

    writer.finish()?;
    Ok(())
}

fn content_paths(changes: &[Change], root_dir: &Path) -> Vec<PathBuf> {
    changes
        .iter()
        .filter(|c| c.has_content())
        .map(|c| c.path.to_full_path(root_dir))
        .collect()
}

/// Write split diff files. Each chunk is a self-contained
/// `[DiffChange...][FileContent...]` pair. When a content record exceeds
/// the chunk's remaining budget, it is split across chunks: the current chunk
/// takes what fits, the next chunk begins with the continuation. This preserves
/// the per-chunk "all metadata, then all content" invariant that lets
/// `parse_diff_metadata` stop at the first FileContent record.
fn write_split_diff(
    diff_out: &Path,
    changes: &[Change],
    source_snapshot_hash: [u8; 16],
    root_dir: &Path,
    max_bytes: u64,
    compress: bool,
    reporter: &Reporter,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }

    let diff_out_str = diff_out.to_string_lossy();
    let mut chunk_number: u32 = 1;
    let mut writer =
        create_chunk_writer(&diff_out_str, chunk_number, source_snapshot_hash, compress)?;

    let mut pool = PrefetchPool::new(content_paths(changes, root_dir));

    let meta_pb = reporter.counter("Writing diff metadata", changes.len() as u64);
    let content_count = changes.iter().filter(|c| c.has_content()).count() as u64;
    let content_pb = reporter.counter("Writing diff content", content_count);

    // dc_cursor points at the next change whose DiffChange still needs to be
    // written. fc_cursor points at the next change whose content still
    // needs to be written. Invariant: fc_cursor <= dc_cursor
    let mut dc_cursor = 0usize;
    let mut fc_cursor = 0usize;
    let mut partial: Option<ContentReader> = None;

    loop {
        // Drain any content carried from the previous chunk first. While a
        // file is straddling, no new DiffChanges may be introduced in this
        // chunk
        if let Some(reader) = partial.as_mut() {
            write_content_fragment(&mut writer, reader, max_bytes)?;
            if reader.remaining() == 0 {
                partial = None;
                fc_cursor += 1;
                content_pb.inc(1);
            } else {
                writer.finish()?;
                chunk_number += 1;
                writer = create_chunk_writer(
                    &diff_out_str,
                    chunk_number,
                    source_snapshot_hash,
                    compress,
                )?;
                continue;
            }
        }

        // section 1 (DiffChange): batch as many fit in the chunk
        while dc_cursor < changes.len() && writer.bytes_written() < max_bytes {
            writer.write_diff_change(&changes[dc_cursor])?;
            dc_cursor += 1;
            meta_pb.inc(1);
        }

        // section 2 (FileContent): emit content for committed changes in
        // [fc_cursor, dc_cursor). Stop when the chunk fills; the straddled
        // file - if any - is carried over in 'partial'.
        while fc_cursor < dc_cursor {
            if !changes[fc_cursor].has_content() {
                fc_cursor += 1;
                continue;
            }
            if writer.bytes_written() + RECORD_HEADER_SIZE >= max_bytes {
                break; // no room even for an empty FC record — roll chunk.
            }

            let mut reader = pool
                .next()?
                .expect("pool drained before fc_cursor caught up");
            write_content_fragment(&mut writer, &mut reader, max_bytes)?;
            if reader.remaining() == 0 {
                fc_cursor += 1;
                content_pb.inc(1);
            } else {
                partial = Some(reader);
                break;
            }
        }

        let done = dc_cursor >= changes.len() && fc_cursor >= changes.len() && partial.is_none();
        if done {
            break;
        }

        writer.finish()?;
        chunk_number += 1;
        writer = create_chunk_writer(&diff_out_str, chunk_number, source_snapshot_hash, compress)?;
    }

    writer.finish()?;
    pool.finish()?;
    meta_pb.finish_with_message(format!("Wrote {} diff records", changes.len()));
    content_pb.finish_with_message(format!("Wrote {} content files", content_count));
    info!("Wrote {} diff chunks", chunk_number);
    Ok(())
}

/// Create a new chunk writer for split diffs.
fn create_chunk_writer(
    diff_out_str: &str,
    chunk_number: u32,
    source_snapshot_hash: [u8; 16],
    compress: bool,
) -> Result<FormatWriter<BufWriter<File>>> {
    let path = format!("{}.{:03}", diff_out_str, chunk_number);
    let file = File::create(&path)?;
    let buf = BufWriter::new(file);
    let header = FileHeader::diff(source_snapshot_hash, Some(chunk_number));
    FormatWriter::maybe_compressed(buf, &header, compress)
}

/// Stream up to one chunk's bytes from `reader` into a FileContent
/// record. Advances the reader, caller uses `reader.remaining() == 0` to
/// detect completion.
///
/// If `max_bytes` is smaller than the record overhead, the budget underflows
/// to 0 — in that case the whole remaining payload is written to guarantee
/// forward progress.
fn write_content_fragment<W: Write>(
    writer: &mut FormatWriter<W>,
    reader: &mut ContentReader,
    max_bytes: u64,
) -> Result<()> {
    let budget = max_bytes.saturating_sub(writer.bytes_written() + RECORD_HEADER_SIZE);
    let remaining = reader.remaining();
    let fragment = remaining.min(if budget > 0 { budget } else { remaining });
    writer.write_file_content_from_reader(reader, fragment)?;
    Ok(())
}

/// A job dispatched to a prefetch worker.
struct ReadJob {
    path: PathBuf,
    size_tx: Sender<io::Result<u64>>,
    chunk_tx: Sender<Chunk>,
}

/// One file queued for prefetching. `size_rx` resolves to the file
/// size (or an open error). `chunk_rx` streams the payload afterward.
struct Pending {
    path: PathBuf,
    size_rx: Receiver<io::Result<u64>>,
    chunk_rx: Receiver<Chunk>,
}

/// Prefetches file content in parallel, handing out `ContentReader`s in
/// change-list order. At most `n_workers` files are streamed concurrently,
/// each bounded to `CHUNK_DEPTH` buffered chunks
struct PrefetchPool {
    paths: Vec<PathBuf>,
    in_flight: VecDeque<Pending>,
    next_spawn: usize,
    max_in_flight: usize,
    job_tx: Option<Sender<ReadJob>>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl PrefetchPool {
    fn new(paths: Vec<PathBuf>) -> Self {
        let n_workers = parallel::worker_count();
        let (job_tx, job_rx) = crossbeam_channel::bounded::<ReadJob>(n_workers);

        let handles: Vec<_> = (0..n_workers)
            .map(|_| {
                let job_rx = job_rx.clone();
                thread::spawn(move || {
                    for job in job_rx.iter() {
                        stream_file(job);
                    }
                })
            })
            .collect();
        drop(job_rx);

        let mut pool = Self {
            paths,
            in_flight: VecDeque::new(),
            next_spawn: 0,
            max_in_flight: n_workers,
            job_tx: Some(job_tx),
            handles,
        };
        pool.top_up();
        pool
    }

    /// Dispatch jobs up to the concurrency cap.
    fn top_up(&mut self) {
        let Some(tx) = self.job_tx.as_ref() else {
            return;
        };
        while self.in_flight.len() < self.max_in_flight && self.next_spawn < self.paths.len() {
            let path = self.paths[self.next_spawn].clone();
            self.next_spawn += 1;
            let (size_tx, size_rx) = crossbeam_channel::bounded(1);
            let (chunk_tx, chunk_rx) = parallel::chunk_channel();
            if tx
                .send(ReadJob {
                    path: path.clone(),
                    size_tx,
                    chunk_tx,
                })
                .is_err()
            {
                break;
            }
            self.in_flight.push_back(Pending {
                path,
                size_rx,
                chunk_rx,
            });
        }
    }

    /// Pop the next file in queue order, blocking until its size is known.
    /// Returns `Ok(None)` once every queued path has been handed out.
    fn next(&mut self) -> Result<Option<ContentReader>> {
        let Some(pending) = self.in_flight.pop_front() else {
            return Ok(None);
        };
        self.top_up();
        let size = pending.size_rx.recv().map_err(|_| {
            GappedError::WorkerPoolFailure("prefetch worker exited before reporting size")
        })?;
        let size = size.map_err(|source| GappedError::IoPath {
            path: pending.path,
            source,
        })?;
        Ok(Some(ContentReader::new(pending.chunk_rx, size)))
    }

    /// Close job channel and join all worker threads.
    fn finish(mut self) -> Result<()> {
        self.job_tx.take();
        for handle in self.handles.drain(..) {
            parallel::join_worker(handle, "diff reader thread panicked")?;
        }
        Ok(())
    }
}

/// Open the file, report its size, then stream the payload as `CHUNK_SIZE`
/// chunks through `chunk_tx`. Stops after exactly `size` bytes.
fn stream_file(job: ReadJob) {
    let ReadJob {
        path,
        size_tx,
        chunk_tx,
    } = job;

    let opened = File::open(&path).and_then(|f| {
        let size = f.metadata()?.len();
        Ok((f, size))
    });

    let (file, size) = match opened {
        Ok(v) => {
            if size_tx.send(Ok(v.1)).is_err() {
                return;
            }
            v
        }
        Err(e) => {
            let _ = size_tx.send(Err(e));
            return;
        }
    };

    let mut reader = BufReader::with_capacity(parallel::CHUNK_SIZE, file);
    let mut remaining = size;
    while remaining > 0 {
        let want = parallel::CHUNK_SIZE.min(remaining as usize);
        let mut buf = vec![0u8; want];
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = chunk_tx.send(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file truncated while streaming",
                )));
                return;
            }
            Ok(n) => {
                buf.truncate(n);
                if chunk_tx.send(Ok(buf)).is_err() {
                    return;
                }
                remaining -= n as u64;
            }
            Err(e) => {
                let _ = chunk_tx.send(Err(e));
                return;
            }
        }
    }
}

/// Compare two entries of the same kind and produce a change if they differ
fn compute_entry_diff(old: &Entry, new: &Entry) -> Option<Change> {
    debug_assert!(old.kind == new.kind);

    let metadata_changed = !old.metadata.matches(&new.metadata);
    let hash_changed = old.hash != new.hash;
    let symlink_target_changed = old.symlink_target != new.symlink_target;

    if !metadata_changed && !hash_changed && !symlink_target_changed {
        return None;
    }

    let has_content = hash_changed && new.kind == EntryKind::File;
    let modified = ModifiedEntry {
        // Always carry new_metadata when content changes — the apply reader
        // needs size to pair FileContent bytes with this change
        new_metadata: if metadata_changed || has_content {
            Some(new.metadata.clone())
        } else {
            None
        },
        new_hash: if hash_changed { new.hash } else { None },
        has_content,
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
        kind: ChangeKind::Removed(entry.kind),
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

/// Reference merge-join used only by the unit tests. this function exists to keep the
/// test suite able to exercise the merge logic on small
/// snapshots without needing a real filesystem walk.
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::entry::{Entry, EntryKind, Metadata};
    use crate::model::path::RelativePath;
    use std::path::PathBuf;

    fn make_metadata(size: u64, mtime_sec: i64, permissions: u32) -> Metadata {
        Metadata {
            size,
            mtime_sec,
            mtime_nsec: 0,
            permissions,
            uid: 1000,
            gid: 1000,
        }
    }

    fn make_file(path: &Path, size: u64, mtime_sec: i64, hash: Option<[u8; 16]>) -> Entry {
        Entry {
            path: RelativePath::new(path).unwrap(),
            kind: EntryKind::File,
            metadata: make_metadata(size, mtime_sec, 0o644),
            hash,
            symlink_target: None,
        }
    }

    fn make_dir(path: &Path, mtime: i64) -> Entry {
        Entry {
            path: RelativePath::new(path).unwrap(),
            kind: EntryKind::Directory,
            metadata: make_metadata(0, mtime, 0o755),
            hash: None,
            symlink_target: None,
        }
    }

    fn make_symlink(path: &Path, target: &str) -> Entry {
        Entry {
            path: RelativePath::new(path).unwrap(),
            kind: EntryKind::Symlink,
            metadata: make_metadata(0, 0, 0o777),
            hash: None,
            symlink_target: Some(PathBuf::from(target)),
        }
    }

    fn dummy_hash(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn summarize(changes: &[Change]) -> Vec<(&RelativePath, &str)> {
        changes
            .iter()
            .map(|c| {
                let kind_str = match c.kind {
                    ChangeKind::Added(_) => "A",
                    ChangeKind::Modified(_) => "M",
                    ChangeKind::Removed(_) => "R",
                };
                (&c.path, kind_str)
            })
            .collect()
    }

    #[test]
    fn test_identical_snapshots_produce_no_changes() {
        let entries = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("a.txt"), 100, 1000, Some(dummy_hash(1))),
            make_dir(Path::new("sub"), 1000),
            make_file(Path::new("sub/b.txt"), 200, 1000, Some(dummy_hash(2))),
        ];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&entries, &entries, &root).unwrap();
        assert!(
            changes.is_empty(),
            "Identical snapshots should produce no changes"
        );
    }

    #[test]
    fn test_empty_old_snapshot_all_added() {
        let old: Vec<Entry> = vec![];
        let new = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("hello.txt"), 5, 1000, Some(dummy_hash(1))),
        ];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .all(|c| matches!(c.kind, ChangeKind::Added(_)))
        );
    }

    #[test]
    fn test_empty_new_snapshot_all_removed() {
        let old = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("hello.txt"), 5, 1000, Some(dummy_hash(1))),
        ];
        let new: Vec<Entry> = vec![];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .all(|c| matches!(c.kind, ChangeKind::Removed(_)))
        );
    }

    #[test]
    fn test_file_added() {
        let old = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("a.txt"), 10, 1000, Some(dummy_hash(1))),
        ];
        let new = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("a.txt"), 10, 1000, Some(dummy_hash(1))),
            make_file(Path::new("b.txt"), 20, 2000, Some(dummy_hash(2))),
        ];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].path,
            RelativePath::new(Path::new("b.txt")).unwrap()
        );
        assert!(matches!(changes[0].kind, ChangeKind::Added(_)));

        // Added file should have has_content = true
        if let ChangeKind::Added(ref added) = changes[0].kind {
            assert!(added.has_content, "Added file should have content");
        }
    }

    #[test]
    fn test_file_removed() {
        let old = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("a.txt"), 10, 1000, Some(dummy_hash(1))),
            make_file(Path::new("b.txt"), 20, 2000, Some(dummy_hash(2))),
        ];
        let new = vec![
            make_dir(Path::new("."), 1000),
            make_file(Path::new("a.txt"), 10, 1000, Some(dummy_hash(1))),
        ];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].path,
            RelativePath::new(Path::new("b.txt")).unwrap()
        );
        assert!(matches!(
            changes[0].kind,
            ChangeKind::Removed(EntryKind::File)
        ));
    }

    #[test]
    fn test_file_content_changed() {
        let old = vec![make_file(
            Path::new("a.txt"),
            100,
            1000,
            Some(dummy_hash(1)),
        )];
        let new = vec![make_file(
            Path::new("a.txt"),
            150,
            2000,
            Some(dummy_hash(2)),
        )];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 1);
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert!(m.has_content, "Content change should include file content");
            assert!(m.new_hash.is_some(), "Should carry new hash");
            assert!(
                m.new_metadata.is_some(),
                "Metadata also changed (size, mtime)"
            );
        } else {
            panic!("Expected Modified change");
        }
    }

    #[test]
    fn test_metadata_only_change_no_content() {
        let old = vec![make_file(
            Path::new("a.txt"),
            100,
            1000,
            Some(dummy_hash(1)),
        )];
        let new = vec![make_file(
            Path::new("a.txt"),
            100,
            2000,
            Some(dummy_hash(1)),
        )]; // same hash, diff mtime

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 1);
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert!(
                !m.has_content,
                "Metadata-only change must NOT include content"
            );
            assert!(
                m.new_hash.is_none(),
                "Hash unchanged → new_hash should be None"
            );
            assert!(m.new_metadata.is_some(), "Should carry updated metadata");
        } else {
            panic!("Expected Modified change");
        }
    }

    #[test]
    fn test_permission_only_change() {
        let mut old_entry = make_file(Path::new("script.sh"), 50, 1000, Some(dummy_hash(1)));
        old_entry.metadata.permissions = 0o644;

        let mut new_entry = make_file(Path::new("script.sh"), 50, 1000, Some(dummy_hash(1)));
        new_entry.metadata.permissions = 0o755;

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&[old_entry], &[new_entry], &root).unwrap();

        assert_eq!(changes.len(), 1);
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert!(
                !m.has_content,
                "Permission-only change must NOT include content"
            );
            assert!(m.new_metadata.is_some());
        } else {
            panic!("Expected Modified change");
        }
    }

    #[test]
    fn test_symlink_target_changed() {
        let old = vec![make_symlink(Path::new("link"), "/old/target")];
        let new = vec![make_symlink(Path::new("link"), "/new/target")];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 1);
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert_eq!(
                m.new_symlink_target.as_deref(),
                Some(Path::new("/new/target"))
            );
            assert!(!m.has_content, "Symlinks never have file content");
        } else {
            panic!("Expected Modified change");
        }
    }

    #[test]
    fn test_type_change_file_to_dir() {
        let old = vec![make_file(
            Path::new("thing"),
            100,
            1000,
            Some(dummy_hash(1)),
        )];
        let new = vec![make_dir(Path::new("thing"), 2000)];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        // Type change should produce a Remove (old type) then Add (new type)
        assert_eq!(changes.len(), 2);
        assert!(matches!(
            changes[0].kind,
            ChangeKind::Removed(EntryKind::File)
        ));
        assert!(matches!(changes[1].kind, ChangeKind::Added(_)));
        assert_eq!(changes[0].path, changes[1].path);
    }

    #[test]
    fn test_type_change_dir_to_symlink() {
        let old = vec![make_dir(Path::new("thing"), 1000)];
        let new = vec![make_symlink(Path::new("thing"), "/somewhere")];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(matches!(
            changes[0].kind,
            ChangeKind::Removed(EntryKind::Directory)
        ));
        assert!(matches!(changes[1].kind, ChangeKind::Added(_)));
    }

    #[test]
    fn test_type_change_symlink_to_file() {
        let old = vec![make_symlink(Path::new("thing"), "/target")];
        let new = vec![make_file(Path::new("thing"), 50, 2000, Some(dummy_hash(1)))];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(matches!(
            changes[0].kind,
            ChangeKind::Removed(EntryKind::Symlink)
        ));
        assert!(matches!(changes[1].kind, ChangeKind::Added(_)));

        // The added file should have content
        if let ChangeKind::Added(ref added) = changes[1].kind {
            assert!(added.has_content);
        }
    }

    #[test]
    fn test_mixed_changes() {
        let old = vec![
            make_file(Path::new("a.txt"), 10, 1000, Some(dummy_hash(1))), // will be removed
            make_file(Path::new("b.txt"), 20, 1000, Some(dummy_hash(2))), // unchanged
            make_file(Path::new("c.txt"), 30, 1000, Some(dummy_hash(3))), // will be modified
        ];
        let new = vec![
            make_file(Path::new("b.txt"), 20, 1000, Some(dummy_hash(2))), // unchanged
            make_file(Path::new("c.txt"), 35, 2000, Some(dummy_hash(4))), // modified
            make_file(Path::new("d.txt"), 40, 3000, Some(dummy_hash(5))), // added
        ];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        let summary = summarize(&changes);
        assert_eq!(summary.len(), 3);

        // Check each change (order follows sorted path merge-join)
        assert_eq!(
            summary[0].0,
            &RelativePath::new(Path::new("a.txt")).unwrap()
        );
        assert_eq!(summary[0].1, "R");

        assert_eq!(
            summary[1].0,
            &RelativePath::new(Path::new("c.txt")).unwrap()
        );
        assert_eq!(summary[1].1, "M");

        assert_eq!(
            summary[2].0,
            &RelativePath::new(Path::new("d.txt")).unwrap()
        );
        assert_eq!(summary[2].1, "A");
    }

    #[test]
    fn test_ownership_change() {
        let mut old_entry = make_file(Path::new("owned.txt"), 100, 1000, Some(dummy_hash(1)));
        old_entry.metadata.uid = 1000;
        old_entry.metadata.gid = 1000;

        let mut new_entry = make_file(Path::new("owned.txt"), 100, 1000, Some(dummy_hash(1)));
        new_entry.metadata.uid = 0;
        new_entry.metadata.gid = 0;

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&[old_entry], &[new_entry], &root).unwrap();

        assert_eq!(changes.len(), 1);
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert!(
                !m.has_content,
                "Ownership change should not include content"
            );
            assert!(m.new_metadata.is_some());
        } else {
            panic!("Expected Modified change");
        }
    }

    #[test]
    fn test_directory_mtime_changed() {
        let old = vec![make_dir(Path::new("mydir"), 1000)];
        let new = vec![make_dir(Path::new("mydir"), 2000)];

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 1);
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert!(m.new_metadata.is_some());
            assert!(!m.has_content, "Directories never have content");
        } else {
            panic!("Expected Modified change");
        }
    }

    // --- Split diff writer tests ---

    use crate::commands::apply::detect_diff_files;
    use crate::commands::snapshot::run_snapshot;
    use crate::format::header::RecordType;
    use crate::format::reader::FormatReader;
    use std::fs;
    use std::io::BufReader;
    use tempfile::TempDir;

    // count change records across diff files
    fn count_diff_changes(chunks: &[PathBuf]) -> usize {
        let mut total = 0;
        for chunk in chunks {
            let file = File::open(chunk).unwrap();
            let reader = BufReader::new(file);
            let (mut fr, _header) = FormatReader::new(reader).unwrap();
            while let Some(header) = fr.next_record_header().unwrap() {
                if header.record_type == RecordType::DiffChange {
                    total += 1;
                }
                fr.skip_payload(header.payload_len).unwrap();
            }
        }
        total
    }

    // seed source dir with files and write payloaf
    fn seed_source(source: &Path, count: usize, payload: &[u8]) {
        fs::create_dir_all(source).unwrap();
        for i in 0..count {
            fs::write(source.join(format!("f_{:03}.dat", i)), payload).unwrap();
        }
    }

    #[test]
    fn test_run_diff_writes_single_file() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("src");
        seed_source(&source, 3, b"hello");

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(source.join("f_000.dat"), b"updated").unwrap();

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

        assert!(diff.exists(), "single diff file should be written");
        assert!(
            !tmp.path().join("diff.gapped.001").exists(),
            "no chunk file should be created without split_size"
        );
    }

    #[test]
    fn test_run_diff_with_split_produces_multiple_files() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("src");
        seed_source(&source, 10, &vec![b'a'; 1024]);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        // modify every file so the diff contains changes for every file
        thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..10 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'b'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff_base,
            &snap2,
            Some(4096),
            false,
            &Reporter::hidden(),
        )
        .unwrap();

        // base path itself should NOT exist...
        assert!(!diff_base.exists());

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(
            chunks.len() > 1,
            "expected multiple chunks, got {}",
            chunks.len()
        );

        for (i, chunk) in chunks.iter().enumerate() {
            let expected = tmp.path().join(format!("diff.gapped.{:03}", i + 1));
            assert_eq!(chunk, &expected);
        }
    }

    #[test]
    fn test_run_diff_split_preserves_all_changes() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("src");
        seed_source(&source, 15, &vec![b'a'; 512]);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        // 15 modifications + 1 addition + 1 removal = 17 changes
        thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..15 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'b'; 1024]).unwrap();
        }
        fs::write(source.join("extra.txt"), b"new").unwrap();
        fs::remove_file(source.join("f_000.dat")).unwrap();

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff_base,
            &snap2,
            Some(2048),
            false,
            &Reporter::hidden(),
        )
        .unwrap();

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(chunks.len() > 1);

        // 14 modified files (f_001..f_014) + 1 added + 1 removed + 1 modified
        // root dir (its mtime changes when files are added/removed) = 17.
        // (f_000 was rewritten then deleted → single Removed change.)
        let total_changes = count_diff_changes(&chunks);
        assert_eq!(total_changes, 17);
    }

    #[test]
    fn test_run_diff_split_chunk_headers_have_chunk_index() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("src");
        seed_source(&source, 6, &vec![b'a'; 1024]);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..6 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'z'; 2048]).unwrap();
        }

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
        assert!(chunks.len() > 1);

        for (i, chunk) in chunks.iter().enumerate() {
            let file = File::open(chunk).unwrap();
            let (_, header) = FormatReader::new(BufReader::new(file)).unwrap();
            assert_eq!(header.file_type, "diff");
            assert_eq!(header.chunk_index, Some((i + 1) as u32));
            assert!(header.source_snapshot_hash.is_some());
        }
    }

    #[test]
    fn test_run_diff_split_compressed() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("src");
        seed_source(&source, 8, &vec![b'a'; 1024]);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..8 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'c'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff_base,
            &snap2,
            Some(2048),
            true,
            &Reporter::hidden(),
        )
        .unwrap();

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(chunks.len() > 1);

        let total = count_diff_changes(&chunks);
        assert_eq!(total, 8);
    }

    // saturates the reader pool with a content-heavy diff and round-trips
    // it through `run_apply` to confirm order preservation: if a worker's
    // result were matched to the wrong change, the reconstructed files
    // would show mismatched content.
    #[test]
    fn test_parallel_diff_content_read() {
        use crate::commands::apply::run_apply;
        use crate::test_util::copy_tree;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("src");
        let target = tmp.path().join("tgt");
        fs::create_dir(&source).unwrap();

        const N: usize = 60;
        for i in 0..N {
            let fill = (i as u8).wrapping_mul(19).wrapping_add(3);
            fs::write(source.join(format!("f_{:03}.bin", i)), vec![fill; 3000]).unwrap();
        }
        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        // mdify every file with distinct content to test the parallel
        // read + in-order write path under split-chunks.
        thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..N {
            let fill = (i as u8).wrapping_mul(31).wrapping_add(7);
            fs::write(source.join(format!("f_{:03}.bin", i)), vec![fill; 4096]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff_base,
            &snap2,
            Some(8192),
            false,
            &Reporter::hidden(),
        )
        .unwrap();

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(chunks.len() > 1, "expected multi-chunk split diff");

        let chunk_refs: Vec<&Path> = chunks.iter().map(|p| p.as_path()).collect();
        run_apply(&target, &chunk_refs, &Reporter::hidden()).unwrap();

        for i in 0..N {
            let expected = vec![(i as u8).wrapping_mul(31).wrapping_add(7); 4096];
            let actual = fs::read(target.join(format!("f_{:03}.bin", i))).unwrap();
            assert_eq!(actual, expected, "content mismatch for file {}", i);
        }
    }

    #[test]
    fn test_large_scale_few_changes() {
        let count = 1000;
        let old: Vec<Entry> = (0..count)
            .map(|i| {
                make_file(
                    Path::new(&format!("file_{:04}.txt", i)),
                    100,
                    1000,
                    Some(dummy_hash(1)),
                )
            })
            .collect();

        let mut new = old.clone();
        // Modify file 42
        new[42].metadata.mtime_sec = 9999;
        new[42].hash = Some(dummy_hash(2));
        // Modify file 999 (metadata only)
        new[999].metadata.permissions = 0o600;

        let root = PathBuf::from("/dummy");
        let changes = compute_diff(&old, &new, &root).unwrap();

        assert_eq!(changes.len(), 2);

        // file_0042: content changed
        assert_eq!(
            changes[0].path,
            RelativePath::new(Path::new("file_0042.txt")).unwrap()
        );
        if let ChangeKind::Modified(ref m) = changes[0].kind {
            assert!(m.has_content);
        }

        // file_0999: metadata only
        assert_eq!(
            changes[1].path,
            RelativePath::new(Path::new("file_0999.txt")).unwrap()
        );
        if let ChangeKind::Modified(ref m) = changes[1].kind {
            assert!(!m.has_content);
        }
    }
}
