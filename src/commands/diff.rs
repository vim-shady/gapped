use crate::error::{GappedError, Result};
use crate::format::header::FileHeader;
use crate::format::writer::FormatWriter;
use crate::model::diff::{AddedEntry, Change, ChangeKind, ModifiedEntry};
use crate::model::entry::{Entry, EntryKind};
use log::info;
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

// FileContent record header: 8-byte length + 1-byte type tag
const RECORD_HEADER: u64 = 9;

pub fn run_diff(
    root_dir: &Path,
    snapshot_in: &Path,
    diff_out: &Path,
    snapshot_out: &Path,
    split_size: Option<u64>,
    compress: bool,
) -> Result<()> {
    let root_dir = super::validate_root_dir(root_dir)?;

    // Compute hash of input snapshot
    info!("Hashing input snapshot {}", snapshot_in.display());
    let source_snapshot_hash =
        crate::fs::hash::hash_file(snapshot_in).map_err(|e| GappedError::IoPath {
            path: snapshot_in.to_path_buf(),
            source: e,
        })?;

    // Load the input snapshot (sorted by path) so walk can binary-search it
    // for hash reuse.
    info!("Loading input snapshot {}", snapshot_in.display());
    let (old_entries, _old_header) = crate::commands::snapshot::load_snapshot(snapshot_in)?;

    // Walk current filesystem
    info!("Walking filesystem under {}", root_dir.display());
    let (new_entries, stats) = crate::fs::walk::walk_filesystem(&root_dir, Some(&old_entries))?;

    // Compute diff
    info!("Computing diff");
    let changes = compute_diff(&old_entries, &new_entries, &root_dir)?;

    let (mut added_count, mut modified_count, mut removed_count) = (0, 0, 0);
    for change in &changes {
        match &change.kind {
            ChangeKind::Added(_) => added_count += 1,
            ChangeKind::Modified(_) => modified_count += 1,
            ChangeKind::Removed(_) => removed_count += 1,
        }
    }

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

    let header = FileHeader::snapshot(root_dir);

    let mut writer = FormatWriter::maybe_compressed(buf_writer, &header, compress)?;

    for entry in entries {
        writer.write_snapshot_entry(entry)?;
    }

    writer.finish()?;
    Ok(())
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

    let header = FileHeader::diff(source_snapshot_hash, None);
    let mut writer = FormatWriter::maybe_compressed(buf_writer, &header, compress)?;

    // Section 1: all DiffChange records.
    for change in changes {
        writer.write_diff_change(change)?;
    }

    // Section 2: FileContent records for each content-bearing change, in the
    // same order. Pairing is by position across the whole diff.
    for change in changes.iter().filter(|c| c.has_content()) {
        write_full_content(&mut writer, change, root_dir)?;
    }

    writer.finish()?;
    Ok(())
}

/// Open the file backing `change` and stream its full content as one
/// `FileContent` record. Used by `write_single_diff`, where the chunk has
/// unbounded size and never needs fragmenting.
fn write_full_content<W: std::io::Write>(
    writer: &mut FormatWriter<W>,
    change: &Change,
    root_dir: &Path,
) -> Result<()> {
    let full_path = change.path.to_full_path(root_dir);
    let file = File::open(&full_path).map_err(|e| GappedError::IoPath {
        path: full_path.clone(),
        source: e,
    })?;
    let size = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    writer.write_file_content_from_reader(&mut reader, size)?;
    Ok(())
}

/// An open file staged for FileContent output. If a chunk fills up mid-file,
/// the remaining bytes are carried over to the next chunk via the same reader.
struct PartialFile {
    reader: BufReader<File>,
    remaining: u64,
}

impl PartialFile {
    fn open(change: &Change, root_dir: &Path) -> Result<Self> {
        let full_path = change.path.to_full_path(root_dir);
        let file = File::open(&full_path).map_err(|e| GappedError::IoPath {
            path: full_path.clone(),
            source: e,
        })?;
        let remaining = file.metadata()?.len();
        Ok(Self {
            reader: BufReader::new(file),
            remaining,
        })
    }
}

/// Write split diff files. Each chunk is a self-contained
/// `[DiffChange...][FileContent...]` pair. When a FileContent record exceeds
/// the chunk's remaining budget, it is split across chunks: the current chunk
/// takes what fits, the next chunk begins with the continuation (and has no
/// DiffChange records until the straddled file is drained). This preserves
/// the per-chunk "all metadata, then all content" invariant that lets
/// `parse_diff_metadata` stop at the first FileContent record.
fn write_split_diff(
    diff_out: &Path,
    changes: &[Change],
    source_snapshot_hash: [u8; 32],
    root_dir: &Path,
    max_bytes: u64,
    compress: bool,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }

    let diff_out_str = diff_out.to_string_lossy();
    let mut chunk_number: u32 = 1;
    let mut writer =
        create_chunk_writer(&diff_out_str, chunk_number, source_snapshot_hash, compress)?;

    // `dc_cursor` points at the next change whose DiffChange still needs to be
    // written; `fc_cursor` points at the next change whose FileContent still
    // needs to be written. Invariant: `fc_cursor <= dc_cursor`.
    let mut dc_cursor = 0usize;
    let mut fc_cursor = 0usize;
    let mut partial: Option<PartialFile> = None;

    loop {
        // drain any spread content from the previous chunk first. While a
        // file is straddling, no new DiffChanges may be introduced in this
        // chunk — the position-based pairing would break.
        if let Some(pf) = partial.as_mut() {
            write_content_fragment(&mut writer, pf, max_bytes)?;
            if pf.remaining == 0 {
                partial = None;
                fc_cursor += 1;
            } else {
                // chunk filled mid-file; roll and continue draining
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

        // Section 1 (DiffChange): batch as many as will fit in the chunk.
        while dc_cursor < changes.len() && writer.bytes_written() < max_bytes {
            writer.write_diff_change(&changes[dc_cursor])?;
            dc_cursor += 1;
        }

        // Section 2 (FileContent): emit content for committed changes in
        // `[fc_cursor, dc_cursor)`. Stop when the chunk fills; the straddled
        // file, if any, is carried over in `partial`.
        while fc_cursor < dc_cursor {
            if !changes[fc_cursor].has_content() {
                fc_cursor += 1;
                continue;
            }
            if writer.bytes_written() + RECORD_HEADER >= max_bytes {
                break; // no room even for an empty FC record — roll chunk.
            }

            let mut pf = PartialFile::open(&changes[fc_cursor], root_dir)?;
            write_content_fragment(&mut writer, &mut pf, max_bytes)?;
            if pf.remaining == 0 {
                fc_cursor += 1;
            } else {
                partial = Some(pf);
                break;
            }
        }

        let done =
            dc_cursor >= changes.len() && fc_cursor >= changes.len() && partial.is_none();
        if done {
            break;
        }

        writer.finish()?;
        chunk_number += 1;
        writer =
            create_chunk_writer(&diff_out_str, chunk_number, source_snapshot_hash, compress)?;
    }

    writer.finish()?;
    info!("Wrote {} diff chunks", chunk_number);
    Ok(())
}

/// Create a new chunk writer for split diffs.
fn create_chunk_writer(
    diff_out_str: &str,
    chunk_number: u32,
    source_snapshot_hash: [u8; 32],
    compress: bool,
) -> Result<FormatWriter<BufWriter<File>>> {
    let path = format!("{}.{:03}", diff_out_str, chunk_number);
    let file = File::create(&path)?;
    let buf = BufWriter::new(file);
    let header = FileHeader::diff(source_snapshot_hash, Some(chunk_number));
    FormatWriter::maybe_compressed(buf, &header, compress)
}

/// Write at most one `FileContent` record from `pf` into the current chunk,
/// respecting `max_bytes`. Updates `pf.remaining`; the caller uses
/// `pf.remaining == 0` to detect completion.
///
/// If `max_bytes` is smaller than the chunk overhead the budget underflows
/// to 0; in that case the whole remaining fragment is written to guarantee
/// forward progress.
fn write_content_fragment<W: std::io::Write>(
    writer: &mut FormatWriter<W>,
    pf: &mut PartialFile,
    max_bytes: u64,
) -> Result<()> {
    let budget = max_bytes.saturating_sub(writer.bytes_written() + RECORD_HEADER);
    let fragment = pf.remaining.min(if budget > 0 { budget } else { pf.remaining });
    writer.write_file_content_from_reader(&mut pf.reader, fragment)?;
    pf.remaining -= fragment;
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

    fn make_file(path: &Path, size: u64, mtime_sec: i64, hash: Option<[u8; 32]>) -> Entry {
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

    fn dummy_hash(byte: u8) -> [u8; 32] {
        [byte; 32]
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
        run_snapshot(&source, &snap1, None, false).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(source.join("f_000.dat"), b"updated").unwrap();

        let diff = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff, &snap2, None, false).unwrap();

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
        run_snapshot(&source, &snap1, None, false).unwrap();

        // modify every file so the diff contains changes for every file
        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..10 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'b'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(4096), false).unwrap();

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
        run_snapshot(&source, &snap1, None, false).unwrap();

        // 15 modifications + 1 addition + 1 removal = 17 changes
        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..15 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'b'; 1024]).unwrap();
        }
        fs::write(source.join("extra.txt"), b"new").unwrap();
        fs::remove_file(source.join("f_000.dat")).unwrap();

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(2048), false).unwrap();

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
        run_snapshot(&source, &snap1, None, false).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..6 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'z'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(3072), false).unwrap();

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
        run_snapshot(&source, &snap1, None, false).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..8 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'c'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(2048), true).unwrap();

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(chunks.len() > 1);

        let total = count_diff_changes(&chunks);
        assert_eq!(total, 8);
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
