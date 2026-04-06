use crate::commands::snapshot::hash_snapshot_file;
use crate::error::GappedError;
use crate::error::Result;
use crate::format::header::FileHeader;
use crate::format::writer::FormatWriter;
use crate::model::diff::{AddedEntry, Change, ChangeKind, Diff, ModifiedEntry};
use crate::model::entry::{Entry, EntryKind};
use crate::model::snapshot::Snapshot;
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
        return Err(GappedError::RootNotFound(root_dir.to_path_buf()));
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
        chunk_index: None,
        more_chunks: None,
    };

    let mut writer: FormatWriter<BufWriter<File>> = if compress {
        FormatWriter::new_compressed(buf_writer, &header)?
    } else {
        FormatWriter::new(buf_writer, &header)?
    };

    for entry in entries {
        writer.write_snapshot_entry(entry)?;
    }

    writer.finish()?;
    Ok(())
}

/// Write split diff files
fn write_split_diff(
    diff_out: &Path,
    changes: &Vec<Change>,
    source_snapshot_hash: [u8; 32],
    root_dir: &Path,
    max_bytes: u64,
    compress: bool,
) -> Result<()> {
    let diff_out_str = diff_out.to_string_lossy();
    let mut chunk_index: u32 = 0;
    let mut change_index = 0;

    while change_index < changes.len() {
        let chunk_path = format!("{}.{:03}", diff_out_str, chunk_index + 1);
        let file = File::create(&chunk_path)?;
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
            chunk_index: Some(chunk_index),
            more_chunks: None,
        };
        let mut writer: FormatWriter<BufWriter<File>> = if compress {
            FormatWriter::new_compressed(buf_writer, &header)?
        } else {
            FormatWriter::new(buf_writer, &header)?
        };

        // write changes until size limit
        while change_index < changes.len() {
            let change = &changes[change_index];
            writer.write_diff_change(change)?;

            // write file content if needed
            match &change.kind {
                ChangeKind::Added(added) if added.has_content => {
                    let full_path = change.path.to_full_path(root_dir);
                    let content = std::fs::read(&full_path).unwrap_or_default();
                    writer.write_file_content(&content)?;
                }
                ChangeKind::Modified(modified) if modified.has_content => {
                    let full_path = change.path.to_full_path(root_dir);
                    let content = std::fs::read(&full_path).unwrap_or_default();
                    writer.write_file_content(&content)?;
                }
                _ => {}
            }
            change_index += 1;

            // check if size limit exceeded
            if writer.bytes_written() >= max_bytes && change_index < changes.len() {
                break;
            }
        }

        writer.finish()?;
        chunk_index += 1;
    }

    info!("Wrote {} diff chunks", chunk_index);
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

    let header = FileHeader {
        file_type: "diff".to_string(),
        version: Diff::CURRENT_VERSION,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        source_snapshot_hash: Some(source_snapshot_hash),
        root_dir: None,
        chunk_index: None,
        more_chunks: None,
    };

    let mut writer = if compress {
        FormatWriter::new_compressed(buf_writer, &header)?
    } else {
        FormatWriter::new(buf_writer, &header)?
    };

    write_changes(&mut writer, changes, root_dir)?;

    writer.finish()?;
    Ok(())
}

// TODO: refactor this mess
/// Write changes to a format writer, incl. file content
fn write_changes<W: std::io::Write>(
    writer: &mut FormatWriter<W>,
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

    let metadata_changed = !old.metadata.matches(&new.metadata);
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

// Unit Tests for compute_diff
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
    use crate::format::reader::{FormatReader, Record};
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
            for record in fr.read_all_records().unwrap() {
                if matches!(record, Record::DiffChange(_)) {
                    total += 1;
                }
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

        std::thread::sleep(std::time::Duration::from_millis(1000));
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
        std::thread::sleep(std::time::Duration::from_millis(1000));
        for i in 0..10 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'b'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(4096), false).unwrap();

        // base path itself should NOT exist...
        assert!(!diff_base.exists());

        let chunks = detect_diff_files(&diff_base);
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
        std::thread::sleep(std::time::Duration::from_millis(1000));
        for i in 0..15 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'b'; 1024]).unwrap();
        }
        fs::write(source.join("extra.txt"), b"new").unwrap();
        fs::remove_file(source.join("f_000.dat")).unwrap();

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(2048), false).unwrap();

        let chunks = detect_diff_files(&diff_base);
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

        std::thread::sleep(std::time::Duration::from_millis(1000));
        for i in 0..6 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'z'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(3072), false).unwrap();

        let chunks = detect_diff_files(&diff_base);
        assert!(chunks.len() > 1);

        for (i, chunk) in chunks.iter().enumerate() {
            let file = File::open(chunk).unwrap();
            let (_, header) = FormatReader::new(BufReader::new(file)).unwrap();
            assert_eq!(header.file_type, "diff");
            assert_eq!(header.chunk_index, Some(i as u32));
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

        std::thread::sleep(std::time::Duration::from_millis(1000));
        for i in 0..8 {
            fs::write(source.join(format!("f_{:03}.dat", i)), vec![b'c'; 2048]).unwrap();
        }

        let diff_base = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(&source, &snap1, &diff_base, &snap2, Some(2048), true).unwrap();

        let chunks = detect_diff_files(&diff_base);
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
