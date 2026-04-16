use crate::model::diff::{Change, ChangeKind};
use crate::model::entry::{Entry, EntryKind};
use crate::model::path::RelativePath;
use std::collections::{HashMap, HashSet};

/// Simulate `run_apply` against an in-memory entry map.
///
/// Apply preserves the mtime of parent directories that are implicitly touched
/// by child adds/removes but don't have an explicit change of their own in the
/// diff.
///
/// `verify` needs to mirror this so it doesn't report `METADATA MISMATCH` for
/// directories that `apply` is intentionally going to leave alone.
///
/// Returns the set of paths whose entries `apply` would leave unchanged.
pub fn simulate_apply(
    state: &mut HashMap<RelativePath, Entry>,
    changes: &[Change],
) -> HashSet<RelativePath> {
    let mut affected_parents: HashSet<RelativePath> = HashSet::new();
    let mut explicit_dirs: HashSet<RelativePath> = HashSet::new();

    for change in changes {
        if let Some(parent) = change.path.parent() {
            affected_parents.insert(parent);
        }
        match &change.kind {
            ChangeKind::Added(added) if added.entry.kind == EntryKind::Directory => {
                explicit_dirs.insert(change.path.clone());
            }
            ChangeKind::Modified(modified) if modified.new_metadata.is_some() => {
                // Apply checks the live filesystem; here we use the in-memory
                // state, which `verify` populates from a fresh walk.
                if state
                    .get(&change.path)
                    .map(|e| e.kind == EntryKind::Directory)
                    .unwrap_or(false)
                {
                    explicit_dirs.insert(change.path.clone());
                }
            }
            _ => {}
        }
    }

    let implicit_dirs: HashSet<RelativePath> = affected_parents
        .difference(&explicit_dirs)
        .cloned()
        .collect();

    for change in changes {
        match &change.kind {
            ChangeKind::Removed(_) => {
                state.remove(&change.path);
            }
            ChangeKind::Added(added) => {
                state.insert(change.path.clone(), added.entry.clone());
            }
            ChangeKind::Modified(modified) => {
                if let Some(existing) = state.get_mut(&change.path) {
                    if let Some(new_metadata) = &modified.new_metadata {
                        existing.metadata = new_metadata.clone();
                    }
                    if let Some(new_hash) = &modified.new_hash {
                        existing.hash = Some(*new_hash);
                    }
                    if let Some(new_symlink_target) = &modified.new_symlink_target {
                        existing.symlink_target = Some(new_symlink_target.clone());
                    }
                }
            }
        }
    }

    implicit_dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::diff::{AddedEntry, ModifiedEntry};
    use crate::model::entry::Metadata;
    use std::path::{Path, PathBuf};

    fn meta(mtime: i64) -> Metadata {
        Metadata {
            size: 0,
            mtime_sec: mtime,
            mtime_nsec: 0,
            permissions: 0o644,
            uid: 1000,
            gid: 1000,
        }
    }

    fn dir_entry(path: &str, mtime: i64) -> Entry {
        Entry {
            path: RelativePath::new(Path::new(path)).unwrap(),
            kind: EntryKind::Directory,
            metadata: Metadata {
                permissions: 0o755,
                ..meta(mtime)
            },
            hash: None,
            symlink_target: None,
        }
    }

    fn file_entry(path: &str, mtime: i64, hash: u8) -> Entry {
        Entry {
            path: RelativePath::new(Path::new(path)).unwrap(),
            kind: EntryKind::File,
            metadata: meta(mtime),
            hash: Some([hash; 16]),
            symlink_target: None,
        }
    }

    fn rel(path: &str) -> RelativePath {
        RelativePath::new(Path::new(path)).unwrap()
    }

    fn build_state(entries: Vec<Entry>) -> HashMap<RelativePath, Entry> {
        entries.into_iter().map(|e| (e.path.clone(), e)).collect()
    }

    #[test]
    fn implicit_dir_is_reported_when_only_child_changes() {
        // sub/ has no explicit change, but a child file is removed.
        let mut state = build_state(vec![dir_entry("sub", 100), file_entry("sub/a.txt", 100, 1)]);
        let changes = vec![Change {
            path: rel("sub/a.txt"),
            kind: ChangeKind::Removed(EntryKind::File),
        }];

        let implicit = simulate_apply(&mut state, &changes);
        assert!(implicit.contains(&rel("sub")));
        assert!(!state.contains_key(&rel("sub/a.txt")));
        // sub itself was not mutated by simulate_apply
        assert_eq!(state.get(&rel("sub")).unwrap().metadata.mtime_sec, 100);
    }

    #[test]
    fn explicit_dir_modification_is_excluded_from_implicit_set() {
        // sub/ has both an explicit Modified AND an implicit child change.
        let mut state = build_state(vec![dir_entry("sub", 100), file_entry("sub/a.txt", 100, 1)]);
        let changes = vec![
            Change {
                path: rel("sub"),
                kind: ChangeKind::Modified(ModifiedEntry {
                    new_metadata: Some(Metadata {
                        permissions: 0o755,
                        ..meta(500)
                    }),
                    new_hash: None,
                    has_content: false,
                    new_symlink_target: None,
                }),
            },
            Change {
                path: rel("sub/a.txt"),
                kind: ChangeKind::Removed(EntryKind::File),
            },
        ];

        let implicit = simulate_apply(&mut state, &changes);
        assert!(!implicit.contains(&rel("sub")));
        // sub WAS mutated to the new metadata
        assert_eq!(state.get(&rel("sub")).unwrap().metadata.mtime_sec, 500);
    }

    #[test]
    fn added_directory_is_excluded_from_implicit_set() {
        // newdir/ is freshly added - its child file should not mark newdir
        // as "implicitly touched" — apply will create newdir with the
        // explicit metadata from the diff.
        let mut state = build_state(vec![]);
        let changes = vec![
            Change {
                path: rel("newdir"),
                kind: ChangeKind::Added(AddedEntry {
                    entry: dir_entry("newdir", 200),
                    has_content: false,
                }),
            },
            Change {
                path: rel("newdir/a.txt"),
                kind: ChangeKind::Added(AddedEntry {
                    entry: file_entry("newdir/a.txt", 200, 7),
                    has_content: true,
                }),
            },
        ];

        let implicit = simulate_apply(&mut state, &changes);
        assert!(!implicit.contains(&rel("newdir")));
        assert_eq!(state.get(&rel("newdir")).unwrap().metadata.mtime_sec, 200);
        assert_eq!(
            state.get(&rel("newdir/a.txt")).unwrap().metadata.mtime_sec,
            200
        );
    }

    #[test]
    fn modified_file_with_content_updates_state() {
        let mut state = build_state(vec![file_entry("a.txt", 100, 1)]);
        let changes = vec![Change {
            path: rel("a.txt"),
            kind: ChangeKind::Modified(ModifiedEntry {
                new_metadata: Some(meta(500)),
                new_hash: Some([2; 16]),
                has_content: true,
                new_symlink_target: None,
            }),
        }];

        let _ = simulate_apply(&mut state, &changes);
        let entry = state.get(&rel("a.txt")).unwrap();
        assert_eq!(entry.metadata.mtime_sec, 500);
        assert_eq!(entry.hash, Some([2; 16]));
    }

    #[test]
    fn modified_symlink_target_updates_state() {
        let target = PathBuf::from("/old");
        let new_target = PathBuf::from("/new");
        let entry = Entry {
            path: rel("link"),
            kind: EntryKind::Symlink,
            metadata: meta(0),
            hash: None,
            symlink_target: Some(target),
        };
        let mut state = build_state(vec![entry]);
        let changes = vec![Change {
            path: rel("link"),
            kind: ChangeKind::Modified(ModifiedEntry {
                new_metadata: None,
                new_hash: None,
                has_content: false,
                new_symlink_target: Some(new_target.clone()),
            }),
        }];

        let _ = simulate_apply(&mut state, &changes);
        assert_eq!(
            state.get(&rel("link")).unwrap().symlink_target,
            Some(new_target)
        );
    }

    #[test]
    fn top_level_change_marks_root_as_implicit_when_root_not_explicit() {
        let mut state = build_state(vec![dir_entry("", 100), file_entry("a.txt", 100, 1)]);
        let changes = vec![Change {
            path: rel("a.txt"),
            kind: ChangeKind::Removed(EntryKind::File),
        }];

        let implicit = simulate_apply(&mut state, &changes);
        assert!(implicit.contains(&RelativePath::root()));
    }
}
