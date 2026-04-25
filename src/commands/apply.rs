use crate::error::{GappedError, Result};
use crate::format::header::{FileHeader, RecordType};
use crate::format::reader::FormatReader;
use crate::model::diff::{Change, ChangeKind, Diff};
use crate::model::entry::{EntryKind, Metadata};
use crate::model::path::RelativePath;
use crate::progress::Reporter;
use log::{info, warn};
use nix::unistd::{Gid, Uid};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::fs::{File, Permissions};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const READ_BUF: usize = 1024 * 1024;
const WRITE_BUF: usize = 1024 * 1024;

struct ApplyResult {
    add_count: usize,
    mod_count: usize,
    err_count: usize,
    dir_metadata_changes: Vec<(RelativePath, Metadata)>,
}

#[derive(Default)]
struct StreamResult {
    add_count: usize,
    mod_count: usize,
    err_count: usize,
}

pub fn run_apply(root_dir: &Path, diff_files: &[&Path], reporter: &Reporter) -> Result<()> {
    let root_dir = super::validate_root_dir(root_dir)?;

    // collect metadata, apply deletions and non-content changes
    let parse_pb = reporter.spinner("Reading diff metadata");
    let changes = parse_diff_metadata(diff_files)?;
    parse_pb.finish_with_message(format!("Read {} changes from diff", changes.len()));
    info!("Applying {} changes", changes.len());

    let saved_dir_mtimes = save_parent_dir_mtimes(&changes, &root_dir);
    let (del_count, mut err_count) = apply_deletions(&changes, &root_dir, reporter);
    let first_pass = apply_non_content_changes(&changes, &root_dir, reporter);
    err_count += first_pass.err_count;

    // stream file content directly to disk
    let second_pass = stream_file_contents(diff_files, &root_dir, &changes, reporter)?;
    err_count += second_pass.err_count;

    restore_directory_mtimes(
        &first_pass.dir_metadata_changes,
        saved_dir_mtimes,
        &root_dir,
    );

    eprintln!("Apply complete:");
    eprintln!("  Added: {}", first_pass.add_count + second_pass.add_count);
    eprintln!(
        "  Modified: {}",
        first_pass.mod_count + second_pass.mod_count
    );
    eprintln!("  Deleted: {}", del_count);
    if err_count > 0 {
        eprintln!("  Errors: {}", err_count);
    }
    Ok(())
}

fn check_diff_version(header: &FileHeader) -> Result<()> {
    if header.file_type == "diff" && header.version != Diff::CURRENT_VERSION {
        return Err(GappedError::InvalidFormat(format!(
            "unsupported diff schema version {} (expected {}); regenerate with current gapped",
            header.version,
            Diff::CURRENT_VERSION,
        )));
    }
    Ok(())
}

/// Read all diff files and collect change metadata. Stops at the first
/// `FileContent` record in each chunk.
pub fn parse_diff_metadata(diff_files: &[&Path]) -> Result<Vec<Change>> {
    let mut all_changes: Vec<Change> = Vec::new();

    for diff_path in diff_files {
        let file = File::open(diff_path)?;
        let reader = BufReader::with_capacity(READ_BUF, file);
        let (mut format_reader, header) = FormatReader::new(reader)?;
        check_diff_version(&header)?;

        while let Some(record) = format_reader.next_record_header()? {
            match record.record_type {
                RecordType::DiffChange => {
                    let payload = format_reader.read_payload(record.payload_len)?;
                    let change: Change = rmp_serde::from_slice(&payload)?;
                    all_changes.push(change);
                }
                // section boundary: first FileContent marks the start of the
                // content section. No further metadata records in this chunk.
                RecordType::FileContent => break,
                _ => format_reader.skip_payload(record.payload_len)?,
            }
        }
    }

    Ok(all_changes)
}

/// Save mtimes of parent directories that will be implicitly touched by file operations
/// but don't have explicit metadata changes in the diff.
fn save_parent_dir_mtimes(all_changes: &[Change], root_dir: &Path) -> HashMap<PathBuf, (i64, u32)> {
    let mut dirs_with_explicit_changes: HashSet<PathBuf> = HashSet::new();
    let mut affected_parent_dirs: HashSet<PathBuf> = HashSet::new();

    for change in all_changes {
        let full_path = change.path.to_full_path(root_dir);

        if let Some(parent_dir) = full_path.parent() {
            affected_parent_dirs.insert(parent_dir.to_path_buf());
        }

        match &change.kind {
            ChangeKind::Added(added) if added.entry.kind == EntryKind::Directory => {
                dirs_with_explicit_changes.insert(full_path);
            }
            ChangeKind::Modified(modified) => {
                if modified.new_metadata.is_some() {
                    let is_dir = full_path
                        .symlink_metadata()
                        .map(|meta| meta.file_type().is_dir())
                        .unwrap_or(false);
                    if is_dir {
                        dirs_with_explicit_changes.insert(full_path);
                    }
                }
            }
            _ => {}
        }
    }

    let mut saved_dir_mtimes: HashMap<PathBuf, (i64, u32)> = HashMap::new();
    for dir_path in &affected_parent_dirs {
        if !dirs_with_explicit_changes.contains(dir_path)
            && let Ok(meta) = dir_path.symlink_metadata()
            && meta.file_type().is_dir()
        {
            saved_dir_mtimes.insert(dir_path.clone(), (meta.mtime(), meta.mtime_nsec() as u32));
        }
    }
    saved_dir_mtimes
}

/// Apply all deletion changes (deepest paths first). Returns (delete_count, err_count).
fn apply_deletions(all_changes: &[Change], root_dir: &Path, reporter: &Reporter) -> (usize, usize) {
    let mut deletions: Vec<&Change> = all_changes
        .iter()
        .filter(|c| matches!(c.kind, ChangeKind::Removed(_)))
        .collect();
    deletions.sort_by(|a, b| b.path.depth().cmp(&a.path.depth()));

    let pb = reporter.counter("Applying deletions", deletions.len() as u64);
    let mut delete_count = 0;
    let mut err_count = 0;
    for change in deletions {
        let full_path = change.path.to_full_path(root_dir);
        if let ChangeKind::Removed(entry_kind) = &change.kind {
            let result = match entry_kind {
                EntryKind::Directory => fs::remove_dir(&full_path),
                EntryKind::File | EntryKind::Symlink => fs::remove_file(&full_path),
            };
            match result {
                Ok(_) => delete_count += 1,
                Err(e) => {
                    err_count += 1;
                    warn!("Failed to delete {}: {}", full_path.display(), e);
                }
            }
        }
        pb.inc(1);
    }
    pb.finish_with_message(format!("Deleted {} entries", delete_count));
    (delete_count, err_count)
}

/// Apply additions and modifications that don't require file content (directories,
/// symlinks, metadata-only changes). File content writes are handled by
/// `stream_file_contents`.
fn apply_non_content_changes(
    all_changes: &[Change],
    root_dir: &Path,
    reporter: &Reporter,
) -> ApplyResult {
    let mut items: Vec<&Change> = all_changes
        .iter()
        .filter(|c| matches!(c.kind, ChangeKind::Added(_) | ChangeKind::Modified(_)))
        .collect();
    items.sort_by(|a, b| a.path.depth().cmp(&b.path.depth()));

    let pb = reporter.counter("Applying changes", items.len() as u64);
    let mut add_count = 0;
    let mut mod_count = 0;
    let mut err_count = 0;
    let mut dir_metadata_changes: Vec<(RelativePath, Metadata)> = Vec::new();

    for change in &items {
        pb.inc(1);
        let full_path = change.path.to_full_path(root_dir);
        match &change.kind {
            ChangeKind::Added(added) => match added.entry.kind {
                EntryKind::Directory => {
                    if let Err(e) = fs::create_dir_all(&full_path) {
                        warn!("Failed to create directory {}: {}", full_path.display(), e);
                        err_count += 1;
                        continue;
                    }
                    set_metadata(&full_path, &added.entry.metadata);
                    dir_metadata_changes.push((change.path.clone(), added.entry.metadata.clone()));
                    add_count += 1;
                }
                EntryKind::File if added.has_content => {
                    // handled by the content pass
                }
                EntryKind::File => {
                    add_count += 1;
                }
                EntryKind::Symlink => {
                    if let Some(target) = &added.entry.symlink_target {
                        if let Err(e) = symlink(target, &full_path) {
                            warn!("Failed to create symlink {}: {}", full_path.display(), e);
                            err_count += 1;
                            continue;
                        }
                        set_symlink_ownership(&full_path, &added.entry.metadata);
                        set_mtime(
                            &full_path,
                            added.entry.metadata.mtime_sec,
                            added.entry.metadata.mtime_nsec,
                        );
                    }
                    add_count += 1;
                }
            },
            ChangeKind::Modified(modified) => {
                if modified.has_content {
                    // handled by the content pass
                    continue;
                }
                if let Some(new_target) = &modified.new_symlink_target {
                    let _ = fs::remove_file(&full_path);
                    if let Err(e) = symlink(new_target, &full_path) {
                        warn!("Failed to update symlink {}: {}", full_path.display(), e);
                        err_count += 1;
                        continue;
                    }
                }
                if let Some(new_metadata) = &modified.new_metadata {
                    let file_type = full_path.symlink_metadata().map(|m| m.file_type()).ok();
                    if file_type.is_some_and(|ft| ft.is_symlink()) {
                        set_symlink_ownership(&full_path, new_metadata);
                        set_mtime(&full_path, new_metadata.mtime_sec, new_metadata.mtime_nsec);
                    } else {
                        set_metadata(&full_path, new_metadata);
                        if file_type.is_some_and(|ft| ft.is_dir()) {
                            dir_metadata_changes.push((change.path.clone(), new_metadata.clone()));
                        }
                    }
                }
                mod_count += 1;
            }
            _ => unreachable!(),
        }
    }
    pb.finish_with_message(format!("Applied {} add/modify changes", items.len()));

    ApplyResult {
        add_count,
        mod_count,
        err_count,
        dir_metadata_changes,
    }
}

/// Re-open diff files and stream file content to disk.
///
/// Single-threaded: read diff records sequentially and, for each
/// `FileContent` record, stream bytes straight into the target's tempfile
/// via a per-record `BufWriter`. When all expected bytes for a change have
/// been written, persist the tempfile and apply metadata. Per-file memory
/// is bounded by `WRITE_BUF`. A single file can span multiple records
/// (and diff-chunk boundaries) — the tempfile stays open across them.
fn stream_file_contents(
    diff_files: &[&Path],
    root_dir: &Path,
    changes: &[Change],
    reporter: &Reporter,
) -> Result<StreamResult> {
    let content_count = changes.iter().filter(|c| c.has_content()).count() as u64;
    let pb = reporter.counter("Writing file content", content_count);
    let mut result = StreamResult::default();

    let mut queue: VecDeque<(Change, u64)> = VecDeque::new();
    let mut active: Option<ActiveWrite> = None;

    for diff_path in diff_files {
        let file = File::open(diff_path)?;
        let reader = BufReader::with_capacity(READ_BUF, file);
        let (mut format_reader, header) = FormatReader::new(reader)?;
        check_diff_version(&header)?;

        while let Some(record) = format_reader.next_record_header()? {
            match record.record_type {
                RecordType::DiffChange => {
                    let payload = format_reader.read_payload(record.payload_len)?;
                    let change: Change = rmp_serde::from_slice(&payload)?;
                    if change.has_content() {
                        let size = expected_content_size(&change);
                        queue.push_back((change, size));
                    }
                }
                RecordType::FileContent => {
                    if active.is_none() {
                        let (change, size) = queue.pop_front().ok_or_else(|| {
                            GappedError::InvalidFormat(
                                "FileContent record with no pending change".into(),
                            )
                        })?;
                        active = Some(ActiveWrite::new(change, size, root_dir)?);
                    }
                    let state = active.as_mut().unwrap();
                    state.stream_record(&mut format_reader, record.payload_len)?;
                    if state.remaining == 0 {
                        let ActiveWrite { change, temp, .. } = active.take().unwrap();
                        let is_add = matches!(&change.kind, ChangeKind::Added(_));
                        if finish_active(&change, temp, root_dir) {
                            if is_add {
                                result.add_count += 1;
                            } else {
                                result.mod_count += 1;
                            }
                        } else {
                            result.err_count += 1;
                        }
                        pb.inc(1);
                    }
                }
                _ => format_reader.skip_payload(record.payload_len)?,
            }
        }
    }

    if active.is_some() || !queue.is_empty() {
        return Err(GappedError::InvalidFormat(
            "diff ended with unresolved content records".into(),
        ));
    }

    pb.finish_with_message(format!("Wrote {} files", content_count));
    Ok(result)
}

/// An in-progress write of a single file's content. Holds the tempfile open
/// across potentially many `FileContent` records — a large file may span
/// split-diff chunk boundaries.
struct ActiveWrite {
    change: Change,
    temp: tempfile::NamedTempFile,
    remaining: u64,
}

impl ActiveWrite {
    fn new(change: Change, size: u64, root_dir: &Path) -> Result<Self> {
        let full_path = change.path.to_full_path(root_dir);
        let parent = full_path.parent().unwrap_or(Path::new("."));
        let temp = tempfile::NamedTempFile::new_in(parent).map_err(GappedError::Io)?;
        Ok(Self {
            change,
            temp,
            remaining: size,
        })
    }

    /// Copy `payload_len` bytes from the diff into the tempfile via a
    /// short-lived BufWriter. Flushing at the record boundary keeps the
    /// tempfile's file cursor consistent for any subsequent record.
    fn stream_record(
        &mut self,
        format_reader: &mut FormatReader,
        payload_len: u64,
    ) -> Result<()> {
        let mut writer = BufWriter::with_capacity(WRITE_BUF, self.temp.as_file_mut());
        format_reader.copy_payload_to(payload_len, &mut writer)?;
        writer.flush()?;
        self.remaining = self.remaining.saturating_sub(payload_len);
        Ok(())
    }
}

/// Finalise a completed tempfile: rename into place and stamp metadata.
fn finish_active(
    change: &Change,
    temp: tempfile::NamedTempFile,
    root_dir: &Path,
) -> bool {
    let full_path = change.path.to_full_path(root_dir);
    if let Err(e) = temp.persist(&full_path) {
        warn!("Failed to persist file {}: {}", full_path.display(), e);
        return false;
    }
    match &change.kind {
        ChangeKind::Added(added) => set_metadata(&full_path, &added.entry.metadata),
        ChangeKind::Modified(modified) => {
            if let Some(new_metadata) = &modified.new_metadata {
                set_metadata(&full_path, new_metadata);
            }
        }
        _ => {}
    }
    true
}

/// Expected file-content size for a content change. Relies on the
/// `ModifiedEntry` invariant: `has_content` implies `new_metadata.is_some()`.
fn expected_content_size(change: &Change) -> u64 {
    match &change.kind {
        ChangeKind::Added(added) if added.has_content => added.entry.metadata.size,
        ChangeKind::Modified(modified) if modified.has_content => modified
            .new_metadata
            .as_ref()
            .map(|m| m.size)
            .expect("has_content implies new_metadata is Some"),
        _ => 0,
    }
}

/// Set directory mtimes: first for dirs with explicit changes (deepest first),
/// then restore saved parent dir mtimes (deepest first).
fn restore_directory_mtimes(
    dir_metadata_changes: &[(RelativePath, Metadata)],
    saved_dir_mtimes: HashMap<PathBuf, (i64, u32)>,
    root_dir: &Path,
) {
    let mut sorted_explicit: Vec<&(RelativePath, Metadata)> = dir_metadata_changes.iter().collect();
    sorted_explicit.sort_by(|a, b| b.0.depth().cmp(&a.0.depth()));
    for (path, metadata) in sorted_explicit {
        let full_path = path.to_full_path(root_dir);
        set_mtime(&full_path, metadata.mtime_sec, metadata.mtime_nsec);
    }

    let mut saved_dirs: Vec<_> = saved_dir_mtimes.into_iter().collect();
    saved_dirs.sort_by(|a, b| {
        let depth_a = a.0.components().count();
        let depth_b = b.0.components().count();
        depth_b.cmp(&depth_a)
    });
    for (dir_path, (mtime_sec, mtime_nsec)) in &saved_dirs {
        set_mtime(dir_path, *mtime_sec, *mtime_nsec);
    }
}

/// Set metadata for a file or directory
fn set_metadata(path: &Path, metadata: &Metadata) {
    if let Err(e) = fs::set_permissions(path, Permissions::from_mode(metadata.permissions)) {
        warn!("Failed to set permissions for {}: {}", path.display(), e);
    }
    set_ownership(path, metadata);
    set_mtime(path, metadata.mtime_sec, metadata.mtime_nsec);
}

/// Set ownership on a path (following symlinks)
fn set_ownership(path: &Path, metadata: &Metadata) {
    if let Err(e) = nix::unistd::chown(
        path,
        Some(Uid::from_raw(metadata.uid)),
        Some(Gid::from_raw(metadata.gid)),
    ) {
        warn!("Failed to set ownership for {}: {}", path.display(), e);
    }
}

/// Set mtime on a path (not following symlinks)
fn set_mtime(path: &Path, mtime_sec: i64, mtime_nsec: u32) {
    use nix::sys::stat::UtimensatFlags;
    use nix::sys::time::TimeSpec;

    let atime = TimeSpec::UTIME_OMIT; // leave unchanged
    let mtime = TimeSpec::new(mtime_sec, mtime_nsec as i64);

    if let Err(e) =
        nix::sys::stat::utimensat(None, path, &atime, &mtime, UtimensatFlags::NoFollowSymlink)
    {
        warn!("Failed to set mtime for {}: {}", path.display(), e);
    }
}

/// Set ownership on a symlink (lchown)
fn set_symlink_ownership(path: &Path, metadata: &Metadata) {
    if let Err(e) = nix::unistd::fchownat(
        None,
        path,
        Some(Uid::from_raw(metadata.uid)),
        Some(Gid::from_raw(metadata.gid)),
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    ) {
        warn!(
            "Failed to set symlink ownership for {}: {}",
            path.display(),
            e
        );
    }
}

/// Detect diff files: given a path like "diff.gapped", look for
/// "diff.gapped.001, "diff.gapped.002", etc.
pub fn detect_diff_files(diff_path: &Path) -> Result<Vec<PathBuf>> {
    let path_str = diff_path.to_string_lossy();

    if diff_path.exists() {
        return Ok(vec![diff_path.to_path_buf()]);
    }
    let mut chunks = Vec::new();
    let mut i = 1u32;
    loop {
        let chunk_path = PathBuf::from(format!("{}.{:03}", path_str, i));
        if chunk_path.exists() {
            chunks.push(chunk_path);
            i += 1;
        } else {
            break;
        }
    }
    let after_gap = PathBuf::from(format!("{}.{:03}", path_str, i + 1));
    if after_gap.exists() {
        return Err(GappedError::InvalidFormat(format!(
            "Diff chunk sequence has a gap: {}.{:03} is missing but {}.{:03} exists",
            path_str,
            i,
            path_str,
            i + 1,
        )));
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::diff::run_diff;
    use crate::commands::snapshot::run_snapshot;
    use std::fs::{self, File};
    use tempfile::TempDir;

    use crate::test_util::copy_tree;

    #[test]
    fn test_detect_diff_files_single_file() {
        let tmp = TempDir::new().unwrap();
        let diff_path = tmp.path().join("diff.gapped");
        File::create(&diff_path).unwrap();

        let chunks = detect_diff_files(&diff_path).unwrap();
        assert_eq!(chunks, vec![diff_path]);
    }

    #[test]
    fn test_detect_diff_files_split_chunks() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("diff.gapped");
        File::create(tmp.path().join("diff.gapped.001")).unwrap();
        File::create(tmp.path().join("diff.gapped.002")).unwrap();
        File::create(tmp.path().join("diff.gapped.003")).unwrap();

        let chunks = detect_diff_files(&base).unwrap();
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn test_detect_diff_files_none() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("diff.gapped");
        let chunks = detect_diff_files(&base).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_detect_diff_files_errors_on_gap() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("diff.gapped");
        File::create(tmp.path().join("diff.gapped.001")).unwrap();
        File::create(tmp.path().join("diff.gapped.002")).unwrap();
        File::create(tmp.path().join("diff.gapped.004")).unwrap();

        let result = detect_diff_files(&base);
        assert!(result.is_err(), "should error on gap in chunk sequence");
    }

    // E2E: snapshot → diff with split_size → apply the detected chunks
    #[test]
    fn test_run_apply_from_split_chunks() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir(&source).unwrap();

        for i in 0..12 {
            fs::write(source.join(format!("file_{:02}.txt", i)), vec![b'a'; 1024]).unwrap();
        }

        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..12 {
            fs::write(source.join(format!("file_{:02}.txt", i)), vec![b'b'; 2048]).unwrap();
        }
        fs::write(source.join("new.txt"), b"brand new\n").unwrap();
        fs::remove_file(source.join("file_00.txt")).unwrap();

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

        let chunks = detect_diff_files(&diff_base).unwrap();
        assert!(chunks.len() > 1);

        let chunk_refs: Vec<&Path> = chunks.iter().map(|p| p.as_path()).collect();
        run_apply(&target, &chunk_refs, &Reporter::hidden()).unwrap();

        assert!(!target.join("file_00.txt").exists());
        assert_eq!(fs::read(target.join("new.txt")).unwrap(), b"brand new\n");
        for i in 1..12 {
            let content = fs::read(target.join(format!("file_{:02}.txt", i))).unwrap();
            assert_eq!(content, vec![b'b'; 2048]);
        }
    }

    // Mixed file sizes — exercises multi-record payloads on split diffs and
    // single-record payloads on smaller files through the same code path.
    #[test]
    fn test_stream_file_contents_mixed_sizes() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir(&source).unwrap();

        // One file forced above a typical split chunk → will span records.
        let big_size = 5 * 1024 * 1024;
        fs::write(source.join("big.bin"), vec![b'X'; big_size]).unwrap();
        for i in 0..20 {
            fs::write(source.join(format!("s_{:02}.bin", i)), vec![b'a'; 4096]).unwrap();
        }

        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(source.join("big.bin"), vec![b'Y'; big_size]).unwrap();
        for i in 0..20 {
            fs::write(source.join(format!("s_{:02}.bin", i)), vec![b'b'; 4096]).unwrap();
        }

        let diff = tmp.path().join("diff.gapped");
        let snap2 = tmp.path().join("snap2");
        run_diff(
            &source,
            &snap1,
            &diff,
            &snap2,
            Some(1024 * 1024),
            false,
            &Reporter::hidden(),
        )
        .unwrap();

        let chunks = detect_diff_files(&diff).unwrap();
        let chunk_refs: Vec<&Path> = chunks.iter().map(|p| p.as_path()).collect();
        run_apply(&target, &chunk_refs, &Reporter::hidden()).unwrap();

        assert_eq!(fs::read(target.join("big.bin")).unwrap(), vec![b'Y'; big_size]);
        for i in 0..20 {
            assert_eq!(
                fs::read(target.join(format!("s_{:02}.bin", i))).unwrap(),
                vec![b'b'; 4096]
            );
        }
    }

    /// Pass 1 (`parse_diff_metadata`) stops at the first FileContent record,
    /// so corruption inside content is invisible to it. A full apply drains
    /// the diff to the EOR checksum and must detect the corruption.
    #[test]
    fn test_pass1_stops_at_section_boundary() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir(&source).unwrap();

        const N: usize = 5;
        for i in 0..N {
            fs::write(source.join(format!("f_{:02}.bin", i)), vec![b'a'; 1024]).unwrap();
        }
        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..N {
            fs::write(source.join(format!("f_{:02}.bin", i)), vec![b'b'; 2048]).unwrap();
        }

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

        let mut bytes = fs::read(&diff).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        fs::write(&diff, &bytes).unwrap();

        let changes = parse_diff_metadata(&[diff.as_path()]).unwrap();
        assert!(changes.len() >= N);

        let result = run_apply(&target, &[diff.as_path()], &Reporter::hidden());
        assert!(result.is_err(), "full apply must reject corrupted diff");
    }

    #[test]
    fn test_run_apply_from_compressed_split_chunks() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir(&source).unwrap();

        for i in 0..8 {
            fs::write(source.join(format!("f_{:02}.bin", i)), vec![b'x'; 2048]).unwrap();
        }

        copy_tree(&source, &target);

        let snap1 = tmp.path().join("snap1");
        run_snapshot(&source, &snap1, None, false, &Reporter::hidden()).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        for i in 0..8 {
            fs::write(source.join(format!("f_{:02}.bin", i)), vec![b'y'; 2048]).unwrap();
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

        let chunk_refs: Vec<&Path> = chunks.iter().map(|p| p.as_path()).collect();
        run_apply(&target, &chunk_refs, &Reporter::hidden()).unwrap();

        for i in 0..8 {
            let content = fs::read(target.join(format!("f_{:02}.bin", i))).unwrap();
            assert_eq!(content, vec![b'y'; 2048]);
        }
    }
}
