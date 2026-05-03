use std::collections::HashMap;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

use crate::model::entry::{Entry, EntryKind};
use crate::model::path::RelativePath;

const STREAM_BUF: usize = 64 * 1024;

/// Compute XXH3-128 hash of a file by streaming the content.
pub fn hash_file(path: &Path) -> std::io::Result<[u8; 16]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = [0u8; STREAM_BUF];

    loop {
        let chunk_size = file.read(&mut buf)?;
        if chunk_size == 0 {
            break;
        }
        hasher.update(&buf[..chunk_size]);
    }

    Ok(hasher.digest128().to_le_bytes())
}

/// Compute directory hashes bottom-up for all directory entries.
///
/// Entries must be sorted by path.
pub fn compute_dir_hashes(entries: &mut [Entry]) {
    let mut children: HashMap<RelativePath, Vec<usize>> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(parent) = entry.path.parent() {
            children.entry(parent).or_default().push(i);
        }
    }

    let mut dir_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EntryKind::Directory)
        .map(|(i, _)| i)
        .collect();
    dir_indices.sort_by(|&a, &b| entries[b].path.depth().cmp(&entries[a].path.depth()));

    for dir_idx in dir_indices {
        let dir_path = entries[dir_idx].path.clone();
        let child_indices = children.remove(&dir_path).unwrap_or_default();

        let mut sorted_children: Vec<usize> = child_indices;
        sorted_children.sort_by(|&a, &b| {
            let a_name = entries[a].path.as_ref().file_name().unwrap_or_default();
            let b_name = entries[b].path.as_ref().file_name().unwrap_or_default();
            a_name.as_bytes().cmp(b_name.as_bytes())
        });

        let mut hasher = Xxh3::new();
        for &ci in &sorted_children {
            let child = &entries[ci];
            let name_bytes = child
                .path
                .as_ref()
                .file_name()
                .unwrap_or_default()
                .as_bytes();
            hasher.update(name_bytes);
            hasher.update(&[0x00]);
            hasher.update(&[child.kind as u8]);

            let content_hash: [u8; 16] = match child.kind {
                EntryKind::File => child.hash.unwrap_or([0u8; 16]),
                EntryKind::Directory => child.dir_hash.unwrap_or([0u8; 16]),
                EntryKind::Symlink => {
                    if let Some(target) = &child.symlink_target {
                        let mut h = Xxh3::new();
                        h.update(target.as_os_str().as_bytes());
                        h.digest128().to_le_bytes()
                    } else {
                        [0u8; 16]
                    }
                }
            };
            hasher.update(&content_hash);
            hasher.update(&child.metadata.permissions.to_le_bytes());
            hasher.update(&child.metadata.uid.to_le_bytes());
            hasher.update(&child.metadata.gid.to_le_bytes());
            hasher.update(&child.metadata.mtime_sec.to_le_bytes());
            hasher.update(&child.metadata.mtime_nsec.to_le_bytes());

            let size = match child.kind {
                EntryKind::File => child.metadata.size,
                _ => 0u64,
            };
            hasher.update(&size.to_le_bytes());
        }

        entries[dir_idx].dir_hash = Some(hasher.digest128().to_le_bytes());
    }
}
