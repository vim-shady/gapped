pub mod apply;
pub mod diff;
pub mod simulate;
pub mod snapshot;
pub mod verify;

use crate::error::{GappedError, Result};
use std::path::{Path, PathBuf};

/// Validate that a root directory exists and return its canonical path.
pub fn validate_root_dir(root_dir: &Path) -> Result<PathBuf> {
    if !root_dir.is_dir() {
        return Err(GappedError::RootNotFound(root_dir.to_path_buf()));
    }
    Ok(root_dir.canonicalize()?)
}
