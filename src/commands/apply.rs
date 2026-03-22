use crate::format::reader::{FormatReader, JsonFormatReader, Record};
use crate::model::diff::{Change, ChangeKind};
use crate::model::entry::{EntryKind, Metadata};
use anyhow::Result;
use log::{info, warn};
use nix::unistd::{Gid, Uid};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::{symlink_metadata, File, Permissions};
use std::io::BufReader;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn run_apply(root_dir: &Path, diff_path: &Path) -> Result<()> {
    if !root_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Root directory {} does not exist",
            root_dir.display()
        ));
    }

    let root_dir = root_dir.canonicalize()?;

    let mut all_changes: Vec<(Change, Option<Vec<u8>>)> = Vec::new();

    info!("Loading changes from {}", diff_path.display());
    let file = File::open(diff_path)?;
    let reader = BufReader::new(file);
    let (mut format_reader, _header) = JsonFormatReader::new(reader)?;

    let records = format_reader.read_all_records()?;

    let mut pending_change: Option<Change> = None;
    for record in records {
        match record {
            Record::DiffChange(change) => {
                if let Some(previous_change) = pending_change.take() {
                    all_changes.push((previous_change, None));
                }
                let has_content = match &change.kind {
                    ChangeKind::Added(added) => added.has_content,
                    ChangeKind::Modified(modified) => modified.has_content,
                    ChangeKind::Removed(_) => false,
                };
                if has_content {
                    pending_change = Some(change);
                } else {
                    all_changes.push((change, None));
                }
            }
            Record::FileContent(content) => {
                if let Some(change) = pending_change.take() {
                    all_changes.push((change, Some(content)));
                } else {
                    warn!("Unexpected FileContent record without preceding change");
                }
            }
            Record::SnapshotEntry(_) => {
                warn!("Unexpected SnapshotEntry in diff file");
            }
        }
    }
    // Flush any trailing change that expected content but didn't get it
    if let Some(change) = pending_change.take() {
        all_changes.push((change, None));
    }

    info!("Applying {} changes", all_changes.len());

    // Collect all directory paths that have explicit metadata changes in the diff
    // Collet all parent directories that will be affected by file operations
    let mut dirs_with_explicit_changes: HashSet<PathBuf> = HashSet::new();
    let mut affected_parent_dirs: HashSet<PathBuf> = HashSet::new();

    for (change, _) in &all_changes {
        let full_path = change.path.to_full_path(&root_dir);

        // Track paretn directories that will be affected by file operations
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
                        .symlink_metadata() // Don't follow symlinks
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

    // Save mtimes of all affected parent directories that DON'T have explicit changes
    let mut saved_dir_mtimes: HashMap<PathBuf, (i64, u32)> = HashMap::new();
    for dir_path in &affected_parent_dirs {
        if !dirs_with_explicit_changes.contains(dir_path) {
            if let Ok(meta) = dir_path.symlink_metadata() {
                if meta.file_type().is_dir() {
                    saved_dir_mtimes
                        .insert(dir_path.clone(), (meta.mtime(), meta.mtime_nsec() as u32));
                }
            }
        }
    }

    // Separate changes into deletions, additions, and modifications
    let mut deletions: Vec<&(Change, Option<Vec<u8>>)> = Vec::new();
    let mut additions_and_modifications: Vec<&(Change, Option<Vec<u8>>)> = Vec::new();
    let mut dir_metadata_changes: Vec<(&Change, &Metadata)> = Vec::new();

    for item in &all_changes {
        match &item.0.kind {
            ChangeKind::Removed(_) => deletions.push(item),
            ChangeKind::Added(_) | ChangeKind::Modified(_) => {
                additions_and_modifications.push(item)
            }
        }
    }

    // Deletions (deepest paths first)
    deletions.sort_by(|a, b| b.0.path.depth().cmp(&a.0.path.depth()));

    let mut delete_count = 0;
    let mut err_count = 0;
    for (change, _) in deletions {
        let full_path = change.path.to_full_path(&root_dir);
        match &change.kind {
            ChangeKind::Removed(entry_kind) => {
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
            _ => unreachable!(),
        }
    }

    // Additions and modifications (shallow first)
    additions_and_modifications.sort_by(|a, b| a.0.path.depth().cmp(&b.0.path.depth()));

    let mut add_count = 0;
    let mut mod_count = 0;

    for (change, content) in &additions_and_modifications {
        let full_path = change.path.to_full_path(&root_dir);
        match &change.kind {
            ChangeKind::Added(added) => {
                match added.entry.kind {
                    EntryKind::Directory => {
                        if let Err(e) = fs::create_dir_all(&full_path) {
                            warn!("Failed to create directory {}: {}", full_path.display(), e);
                            err_count += 1;
                            continue;
                        }
                        set_metadata(&full_path, &added.entry.metadata);
                        dir_metadata_changes.push((change, &added.entry.metadata));
                    }
                    EntryKind::File => {
                        if let Some(content) = content {
                            if let Err(e) = write_file_atomic(&full_path, content) {
                                warn!("Failed to write file {}: {}", full_path.display(), e);
                                err_count += 1;
                                continue;
                            }
                        }
                        set_metadata(&full_path, &added.entry.metadata);
                    }
                    EntryKind::Symlink => {
                        if let Some(target) = &added.entry.symlink_target {
                            if let Err(e) = symlink(target, &full_path) {
                                warn!("Failed to create symlink {}: {}", full_path.display(), e);
                                err_count += 1;
                                continue;
                            }
                            set_symlink_ownership(&full_path, &added.entry.metadata);
                        }
                    }
                }
                add_count += 1;
            }
            ChangeKind::Modified(modified) => {
                if modified.has_content {
                    if let Some(content) = content {
                        if let Err(e) = write_file_atomic(&full_path, content) {
                            warn!("Failed to write file {}: {}", full_path.display(), e);
                            err_count += 1;
                            continue;
                        }
                    }
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
                    let is_symlink = full_path
                        .symlink_metadata()
                        .map(|meta| meta.file_type().is_symlink())
                        .unwrap_or(false);
                    if is_symlink {
                        set_symlink_ownership(&full_path, new_metadata);
                    } else {
                        let is_dir = full_path
                            .symlink_metadata()
                            .map(|meta| meta.file_type().is_dir())
                            .unwrap_or(false);
                        set_metadata(&full_path, new_metadata);
                        if is_dir {
                            dir_metadata_changes.push((change, new_metadata));
                        }
                    }
                }
                mod_count += 1;
            }
            _ => unreachable!(),
        }
    }

    // Set directory mtimes for directories with explicit changes (deepest paths first)
    dir_metadata_changes.sort_by(|a, b| b.0.path.depth().cmp(&a.0.path.depth()));
    for (change, metadata) in &dir_metadata_changes {
        let full_path = change.path.to_full_path(&root_dir);
        set_mtime(&full_path, metadata.mtime_sec, metadata.mtime_nsec);
    }

    // Restore mtimes for directories NOT in the diff (deepest path first)
    let mut saved_dirs = saved_dir_mtimes.into_iter().collect::<Vec<_>>();
    saved_dirs.sort_by(|a, b| {
        let depth_a = a.0.components().count();
        let depth_b = b.0.components().count();
        depth_b.cmp(&depth_a)
    });
    for (dir_path, (mtime_sec, mtime_nsec)) in &saved_dirs {
        set_mtime(dir_path, *mtime_sec, *mtime_nsec);
    }
    eprintln!("Apply complete:");
    eprintln!("  Added: {}", add_count);
    eprintln!("  Modified: {}", mod_count);
    eprintln!("  Deleted: {}", delete_count);
    if err_count > 0 {
        eprintln!("  Errors: {}", err_count);
    }

    Ok(())
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

/// Write file conent atomically using a temp file + rename
fn write_file_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temp, content)?;
    temp.persist(path)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::diff::{AddedEntry, ModifiedEntry};
    use crate::model::entry::Entry;
    use crate::model::path::RelativePath;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn make_metadata(size: u64, perms: u32) -> Metadata {
        Metadata {
            size,
            mtime_sec: 1000,
            mtime_nsec: 0,
            permissions: perms,
            uid: unsafe { nix::libc::getuid() },
            gid: unsafe { nix::libc::getgid() },
        }
    }

    fn diff_change_json(change: &Change) -> String {
        format!(
            r#"{{"type":"diff_change","change":{}}}"#,
            serde_json::to_string(change).unwrap()
        )
    }

    fn file_content_json(data: &[u8]) -> String {
        format!(
            r#"{{"type":"file_content","data":"{}"}}"#,
            BASE64.encode(data)
        )
    }

    /// Build a JSON-line diff file from a list of record lines.
    fn build_diff_file(dir: &Path, records: &[String]) -> PathBuf {
        let diff_path = dir.join("test.diff");
        let mut file = File::create(&diff_path).unwrap();
        writeln!(
            file,
            r#"{{"type":"header","file_type":"diff","version":1,"created_at":0,"source_snapshot_hash":null,"root_dir":null}}"#
        )
        .unwrap();
        for record in records {
            writeln!(file, "{}", record).unwrap();
        }
        diff_path
    }

    // --- write_file_atomic ---

    #[test]
    fn write_file_atomic_creates_new_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.txt");
        write_file_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn write_file_atomic_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.txt");
        fs::write(&path, b"old").unwrap();
        write_file_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    // --- run_apply: add / delete / modify ---

    #[test]
    fn apply_adds_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();

        let change = Change {
            path: RelativePath::new(Path::new("hello.txt")).unwrap(),
            kind: ChangeKind::Added(AddedEntry {
                entry: Entry {
                    path: RelativePath::new(Path::new("hello.txt")).unwrap(),
                    kind: EntryKind::File,
                    metadata: make_metadata(5, 0o644),
                    hash: None,
                    symlink_target: None,
                },
                has_content: true,
            }),
        };

        let diff_path = build_diff_file(
            dir.path(),
            &[diff_change_json(&change), file_content_json(b"hello")],
        );
        run_apply(&root, &diff_path).unwrap();

        assert_eq!(fs::read(root.join("hello.txt")).unwrap(), b"hello");
    }

    #[test]
    fn apply_deletes_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("bye.txt"), b"bye").unwrap();

        let change = Change {
            path: RelativePath::new(Path::new("bye.txt")).unwrap(),
            kind: ChangeKind::Removed(EntryKind::File),
        };

        let diff_path = build_diff_file(dir.path(), &[diff_change_json(&change)]);
        run_apply(&root, &diff_path).unwrap();

        assert!(!root.join("bye.txt").exists());
    }

    #[test]
    fn apply_modifies_file_content() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("data.txt"), b"old").unwrap();

        let change = Change {
            path: RelativePath::new(Path::new("data.txt")).unwrap(),
            kind: ChangeKind::Modified(ModifiedEntry {
                new_metadata: Some(make_metadata(3, 0o644)),
                new_hash: None,
                has_content: true,
                new_symlink_target: None,
            }),
        };

        let diff_path = build_diff_file(
            dir.path(),
            &[diff_change_json(&change), file_content_json(b"new")],
        );
        run_apply(&root, &diff_path).unwrap();

        assert_eq!(fs::read(root.join("data.txt")).unwrap(), b"new");
    }

    #[test]
    fn apply_adds_directory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();

        let change = Change {
            path: RelativePath::new(Path::new("subdir")).unwrap(),
            kind: ChangeKind::Added(AddedEntry {
                entry: Entry {
                    path: RelativePath::new(Path::new("subdir")).unwrap(),
                    kind: EntryKind::Directory,
                    metadata: make_metadata(0, 0o755),
                    hash: None,
                    symlink_target: None,
                },
                has_content: false,
            }),
        };

        let diff_path = build_diff_file(dir.path(), &[diff_change_json(&change)]);
        run_apply(&root, &diff_path).unwrap();

        assert!(root.join("subdir").is_dir());
    }

    #[test]
    fn apply_deletes_nested_directory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/file.txt"), b"x").unwrap();

        let records = vec![
            diff_change_json(&Change {
                path: RelativePath::new(Path::new("a/b/file.txt")).unwrap(),
                kind: ChangeKind::Removed(EntryKind::File),
            }),
            diff_change_json(&Change {
                path: RelativePath::new(Path::new("a/b")).unwrap(),
                kind: ChangeKind::Removed(EntryKind::Directory),
            }),
            diff_change_json(&Change {
                path: RelativePath::new(Path::new("a")).unwrap(),
                kind: ChangeKind::Removed(EntryKind::Directory),
            }),
        ];

        let diff_path = build_diff_file(dir.path(), &records);
        run_apply(&root, &diff_path).unwrap();

        assert!(!root.join("a").exists());
    }

    // --- Phase ordering ---

    #[test]
    fn deletions_sorted_deepest_first() {
        let changes: Vec<(Change, Option<Vec<u8>>)> = vec![
            (
                Change {
                    path: RelativePath::new(Path::new("a")).unwrap(),
                    kind: ChangeKind::Removed(EntryKind::Directory),
                },
                None,
            ),
            (
                Change {
                    path: RelativePath::new(Path::new("a/b/c")).unwrap(),
                    kind: ChangeKind::Removed(EntryKind::File),
                },
                None,
            ),
            (
                Change {
                    path: RelativePath::new(Path::new("a/b")).unwrap(),
                    kind: ChangeKind::Removed(EntryKind::Directory),
                },
                None,
            ),
        ];

        let mut deletions: Vec<&(Change, Option<Vec<u8>>)> = changes.iter().collect();
        deletions.sort_by(|a, b| b.0.path.depth().cmp(&a.0.path.depth()));

        assert_eq!(deletions[0].0.path.depth(), 3); // a/b/c
        assert_eq!(deletions[1].0.path.depth(), 2); // a/b
        assert_eq!(deletions[2].0.path.depth(), 1); // a
    }
}
