use std::fs;
use std::path::Path;

/// Recursively copy a directory tree, preserving file mtimes and directory mtimes.
pub fn copy_tree(src: &Path, dst: &Path) {
    use nix::sys::stat::UtimensatFlags;
    use nix::sys::time::TimeSpec;
    use std::os::unix::fs::MetadataExt;

    fn set_mtime_from(path: &Path, src_meta: &fs::Metadata) {
        let atime = TimeSpec::UTIME_OMIT;
        let mtime = TimeSpec::new(src_meta.mtime(), src_meta.mtime_nsec());
        nix::sys::stat::utimensat(None, path, &atime, &mtime, UtimensatFlags::NoFollowSymlink)
            .unwrap();
    }

    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type().unwrap();
        if ft.is_dir() {
            copy_tree(&src_path, &dst_path);
        } else if ft.is_file() {
            fs::copy(&src_path, &dst_path).unwrap();
            let m = fs::metadata(&src_path).unwrap();
            set_mtime_from(&dst_path, &m);
        }
    }
    // set the directory's mtime last
    let src_meta = fs::metadata(src).unwrap();
    set_mtime_from(dst, &src_meta);
}
