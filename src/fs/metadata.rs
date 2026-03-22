use crate::model::entry::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub fn collect_metadata(path: &Path) -> std::io::Result<(Metadata, std::fs::FileType)> {
    let meta = path.symlink_metadata()?;
    let file_type = meta.file_type();

    let metadata = Metadata {
        size: if file_type.is_file() { meta.len() } else { 0 },
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec() as u32,
        permissions: meta.mode() & 0o777, // only permission bits
        uid: meta.uid(),
        gid: meta.gid(),
    };

    Ok((metadata, file_type))
}
