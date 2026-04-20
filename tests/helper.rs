use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

pub struct TestFixture {
    pub source_dir: TempDir,
    pub target_dir: TempDir,
    pub working_dir: TempDir,
}

impl TestFixture {
    pub fn new() -> Self {
        TestFixture {
            source_dir: TempDir::new().unwrap(),
            target_dir: TempDir::new().unwrap(),
            working_dir: TempDir::new().unwrap(),
        }
    }

    pub fn source(&self) -> &Path {
        self.source_dir.path()
    }

    pub fn target(&self) -> &Path {
        self.target_dir.path()
    }

    // Path for snapshot/diff files
    pub fn working_file(&self, name: &str) -> PathBuf {
        self.working_dir.path().join(name)
    }

    /// Copy source to target using rsync
    pub fn sync_source_to_target(&self) {
        rsync_mirror(self.source(), self.target());
    }

    /// Verify source and target are identical using rsync
    pub fn verify_rsync_identical(&self) -> bool {
        rsync_diff(self.source(), self.target()).is_empty()
    }
}

/// Mirror `source` into `target` with `rsync -a`. Panics on failure.
pub fn rsync_mirror(source: &Path, target: &Path) {
    let status = Command::new("rsync")
        .args([
            "-a",
            &format!("{}/", source.display()),
            &format!("{}/", target.display()),
        ])
        .status()
        .expect("Failed to run rsync");
    assert!(status.success(), "rsync -a mirror failed");
}

/// Return rsync's report of differences between two trees. Empty = identical.
pub fn rsync_diff(source: &Path, target: &Path) -> Vec<String> {
    let output = Command::new("rsync")
        .args([
            "-avn",
            "--delete",
            "--checksum",
            &format!("{}/", source.display()),
            &format!("{}/", target.display()),
        ])
        .output()
        .expect("Failed to run rsync");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with("sending"))
        .filter(|l| !l.starts_with("sent"))
        .filter(|l| !l.starts_with("total size"))
        .filter(|l| l.trim() != "./")
        .map(|l| l.to_string())
        .collect()
}

/// Get gapped binary path
pub fn gapped_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gapped"))
}

/// Run gapped command and return success status
pub fn run_gapped(args: &[&str]) -> bool {
    let output = Command::new(gapped_bin())
        .args(args)
        .output()
        .expect("Failed to run gapped");
    if !output.status.success() {
        eprintln!(
            "gapped failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.status.success()
}

/// Create a file with content
pub fn create_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}
