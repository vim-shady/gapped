use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[path = "helper.rs"]
mod helper;
use helper::{create_file, rsync_diff, rsync_mirror, run_gapped};

/// Loopback-mounted filesystem image. Unmounts and deletes the image on drop.
///
/// Requires passwordless sudo for `mount`/`umount` and the relevant `mkfs.*`
/// tool on PATH. Returns `None` if either is missing, so callers can skip
/// instead of failing.
struct LoopbackFs {
    image: PathBuf,
    mount_point: TempDir,
    _image_dir: TempDir,
}

impl LoopbackFs {
    fn new(fs_type: &str) -> Option<Self> {
        let (mkfs_bin, mkfs_args): (&str, &[&str]) = match fs_type {
            "ext4" => ("mkfs.ext4", &["-F", "-q"]),
            "xfs" => ("mkfs.xfs", &["-f", "-q"]),
            "btrfs" => ("mkfs.btrfs", &["-f", "-q"]),
            "vfat" => ("mkfs.vfat", &["-F", "32"]),
            _ => return None,
        };
        if !tool_exists(mkfs_bin) {
            eprintln!("skipping: {} not installed", mkfs_bin);
            return None;
        }
        // btrfs refuses images below ~50 MB; give every fs 128 MB for headroom.
        let size_mb = 128u64;

        let image_dir = TempDir::new().ok()?;
        let image = image_dir.path().join(format!("{}.img", fs_type));
        let image_str = image.to_str()?;

        if !run("truncate", &["-s", &format!("{}M", size_mb), image_str]) {
            return None;
        }
        let mut mk_args: Vec<&str> = mkfs_args.to_vec();
        mk_args.push(image_str);
        if !run(mkfs_bin, &mk_args) {
            return None;
        }

        let mount_point = TempDir::new().ok()?;
        let mp_str = mount_point.path().to_str()?;

        let uid = unsafe { nix::libc::getuid() };
        let gid = unsafe { nix::libc::getgid() };
        // FAT32 has no owner concept on disk — use mount-time uid/gid so the
        // current user can write. Unix filesystems need on-disk ownership,
        // so for those we just chown the mountpoint after mounting.
        let vfat_opts;
        let mount_opts: &str = if fs_type == "vfat" {
            vfat_opts = format!("loop,uid={},gid={}", uid, gid);
            &vfat_opts
        } else {
            "loop"
        };

        if !run("sudo", &["-n", "mount", "-o", mount_opts, image_str, mp_str]) {
            eprintln!("skipping: sudo -n mount failed (need passwordless sudo)");
            return None;
        }

        if fs_type != "vfat" {
            let _ = run(
                "sudo",
                &["-n", "chown", &format!("{}:{}", uid, gid), mp_str],
            );
        }

        Some(LoopbackFs {
            image,
            mount_point,
            _image_dir: image_dir,
        })
    }

    fn path(&self) -> &Path {
        self.mount_point.path()
    }
}

impl Drop for LoopbackFs {
    fn drop(&mut self) {
        if let Some(mp) = self.mount_point.path().to_str() {
            let _ = run("sudo", &["-n", "umount", mp]);
        }
        let _ = fs::remove_file(&self.image);
    }
}

fn tool_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn fs_tests_enabled() -> bool {
    std::env::var("GAPPED_FS_TESTS").ok().as_deref() == Some("1")
}

fn seed_source(source: &Path) {
    create_file(&source.join("a.txt"), "alpha\n");
    create_file(&source.join("sub/b.txt"), "beta\n");
    create_file(&source.join("sub/deep/c.txt"), "charlie\n");
    symlink("a.txt", source.join("link")).unwrap();
    fs::set_permissions(source.join("sub/b.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(source.join("sub/deep/c.txt"), fs::Permissions::from_mode(0o600)).unwrap();
}

fn mutate_source(source: &Path) {
    create_file(&source.join("a.txt"), "alpha updated with more content\n");
    create_file(&source.join("d.txt"), "delta\n");
    fs::remove_file(source.join("sub/b.txt")).unwrap();
    create_file(&source.join("sub/deep/c.txt"), "charlie also updated\n");
}

fn assert_trees_identical(source: &Path, target: &Path, context: &str) {
    let diff_lines = rsync_diff(source, target);
    assert!(
        diff_lines.is_empty(),
        "{}: trees diverged:\n{}",
        context,
        diff_lines.join("\n")
    );
}

/// Shared body for Linux-to-Linux roundtrip tests.
/// `transport_fs` is `None` for a direct transfer, `Some(fs)` for a diff
/// whose chunks are parked on an intermediate filesystem (the air-gapped case).
fn run_roundtrip(source_fs: &str, target_fs: &str, transport_fs: Option<&str>) {
    if !fs_tests_enabled() {
        eprintln!("skipping: set GAPPED_FS_TESTS=1 (needs passwordless sudo and mkfs.*)");
        return;
    }

    let Some(src_fs) = LoopbackFs::new(source_fs) else {
        return;
    };
    let Some(tgt_fs) = LoopbackFs::new(target_fs) else {
        return;
    };
    let transport = match transport_fs {
        Some(fs) => {
            let Some(t) = LoopbackFs::new(fs) else {
                return;
            };
            Some(t)
        }
        None => None,
    };
    let work_dir = TempDir::new().unwrap();
    let work_root: &Path = transport
        .as_ref()
        .map(|t| t.path())
        .unwrap_or_else(|| work_dir.path());

    let source = src_fs.path().join("tree");
    let target = tgt_fs.path().join("tree");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();

    seed_source(&source);

    let snap1 = work_root.join("snap1");
    assert!(run_gapped(&[
        "snapshot",
        source.to_str().unwrap(),
        snap1.to_str().unwrap(),
    ]));

    rsync_mirror(&source, &target);

    std::thread::sleep(std::time::Duration::from_millis(1100));
    mutate_source(&source);

    let diff_base = work_root.join("diff.gapped");
    let snap2 = work_root.join("snap2");
    // When transport is a real medium, force split diffs to exercise that path.
    let mut diff_args = vec![
        "diff",
        source.to_str().unwrap(),
        snap1.to_str().unwrap(),
        diff_base.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ];
    if transport_fs.is_some() {
        diff_args.extend_from_slice(&["--split-size", "4096"]);
    }
    assert!(run_gapped(&diff_args));

    assert!(run_gapped(&[
        "verify",
        target.to_str().unwrap(),
        diff_base.to_str().unwrap(),
        snap2.to_str().unwrap(),
    ]));

    assert!(run_gapped(&[
        "apply",
        target.to_str().unwrap(),
        diff_base.to_str().unwrap(),
    ]));

    let context = match transport_fs {
        Some(t) => format!("{} → {} (via {})", source_fs, target_fs, t),
        None => format!("{} → {}", source_fs, target_fs),
    };
    assert_trees_identical(&source, &target, &context);
}

// Direct Linux to Linux pairs across common server filesystems.

#[test]
fn roundtrip_ext4_to_xfs() {
    run_roundtrip("ext4", "xfs", None);
}

#[test]
fn roundtrip_ext4_to_btrfs() {
    run_roundtrip("ext4", "btrfs", None);
}

#[test]
fn roundtrip_xfs_to_ext4() {
    run_roundtrip("xfs", "ext4", None);
}

#[test]
fn roundtrip_xfs_to_btrfs() {
    run_roundtrip("xfs", "btrfs", None);
}

#[test]
fn roundtrip_btrfs_to_ext4() {
    run_roundtrip("btrfs", "ext4", None);
}

#[test]
fn roundtrip_btrfs_to_xfs() {
    run_roundtrip("btrfs", "xfs", None);
}

// Air-gapped scenarios: Linux endpoints, FAT32 carrier medium.

#[test]
fn roundtrip_ext4_to_xfs_via_fat32() {
    run_roundtrip("ext4", "xfs", Some("vfat"));
}

#[test]
fn roundtrip_xfs_to_btrfs_via_fat32() {
    run_roundtrip("xfs", "btrfs", Some("vfat"));
}
