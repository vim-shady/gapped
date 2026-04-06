use crate::error::{GappedError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RelativePath(PathBuf);

impl RelativePath {
    /// Create a RelativePath from full path by stripping root prefix
    pub fn from_full_path(full_path: &Path, root: &Path) -> Result<Self> {
        let relative_path = full_path
            .strip_prefix(root)
            .map_err(|_| GappedError::InvalidPath(full_path.to_path_buf()))?;
        Self::new(relative_path)
    }

    /// Create a normalized RelativePath from an already relative path
    pub fn new(path: &Path) -> Result<Self> {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(c) => normalized.push(c),
                Component::CurDir => {} // skip ".",
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(GappedError::InvalidPath(path.to_path_buf()));
                }
            }
        }
        Ok(RelativePath(normalized))
    }

    pub fn root() -> Self {
        RelativePath(PathBuf::new())
    }

    /// Convert to full path by joining with root
    pub fn to_full_path(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }

    /// Return the number of components in the path
    pub fn depth(&self) -> usize {
        self.0.components().count()
    }
}

impl AsRef<Path> for RelativePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
