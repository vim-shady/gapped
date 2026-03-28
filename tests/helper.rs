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
        let status = Command::new("rsync")
            .args([
                "-a",
                &format!("{}/", self.source().display()),
                &format!("{}/", self.target().display()),
            ])
            .status()
            .expect("Failed to sync source to target");
        assert!(status.success());
    }

    /// Verify source and target are identical using rsync
    pub fn verify_rsync_identical(&self) -> bool {
        let output = Command::new("rsync")
            .args([
                "-a",
                &format!("{}/", self.source().display()),
                &format!("{}/", self.target().display()),
            ])
            .output()
            .expect("Failed to verify source and target");
        let stdout = String::from_utf8_lossy(&output.stdout);
        // rsync -avn output: if only stats lines, no differences
        let lines: Vec<&str> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .filter(|line| !line.starts_with("sending"))
            .filter(|line| !line.starts_with("total size"))
            .filter(|line| !line.starts_with("sent"))
            .collect();
        lines.is_empty()
    }
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
