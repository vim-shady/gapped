use super::path::RelativePath;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Kind of filesystem entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// Metadata for a filesystem entry.
///
/// NOTE: the derived `PartialEq` compares `mtime_nsec` exactly. Should not use
/// `==`/`!=` when comparing metadata from two independent filesystem walks —
/// nanosecond precision drifts across filesystems (FAT, some NFS configs
/// truncate to second or millisecond granularity). Use [`Metadata::matches`]
/// instead for diff/verify semantics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub size: u64,
    // negative values are actually used for timestamps that date before 1970 :)
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Metadata {
    /// Tolerance for mtime comparison across filesystems, in nanoseconds.
    /// Matches rsync's default `--modify-window=1`: two mtimes are considered
    /// equal if they lie within one second of each other. This lets snapshots
    /// round-trip cleanly through filesystems that truncate `mtime_nsec`
    /// (FAT32, some NFS configurations, etc.).
    pub const MTIME_TOLERANCE_NS: i128 = 1_000_000_000;

    /// Fast check used by the hash cache: identical `size` + `mtime_sec`
    /// means we can reuse the previously computed content hash. Nanoseconds
    /// are ignored on purpose (the cache runs on a single filesystem where
    /// sub-second precision is consistent, but we don't want a nsec
    /// drift to force re-hashing).
    pub fn size_and_mtime_match(&self, other: &Metadata) -> bool {
        self.size == other.size && self.mtime_sec == other.mtime_sec
    }

    /// True if two mtimes lie within [`Self::MTIME_TOLERANCE_NS`] of each
    /// other. Uses `i128` to avoid any possibility of overflow when mapping
    /// seconds+nanos to a single nanosecond count.
    pub fn mtime_matches(&self, other: &Metadata) -> bool {
        let a = self.mtime_sec as i128 * 1_000_000_000 + self.mtime_nsec as i128;
        let b = other.mtime_sec as i128 * 1_000_000_000 + other.mtime_nsec as i128;
        (a - b).abs() <= Self::MTIME_TOLERANCE_NS
    }

    /// Semantic equality used by `diff` and `verify`. All fields must match
    /// exactly *except* `mtime`, which is compared with the tolerance window
    /// defined above. Prefer this over the derived `PartialEq` whenever the
    /// two sides come from independent filesystem walks.
    pub fn matches(&self, other: &Metadata) -> bool {
        self.size == other.size
            && self.permissions == other.permissions
            && self.uid == other.uid
            && self.gid == other.gid
            && self.mtime_matches(other)
    }
}

/// A single filesystem entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: RelativePath,
    pub kind: EntryKind,
    pub metadata: Metadata,
    pub hash: Option<[u8; 32]>,
    pub symlink_target: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_meta(sec: i64, nsec: u32) -> Metadata {
        Metadata {
            size: 100,
            mtime_sec: sec,
            mtime_nsec: nsec,
            permissions: 0o644,
            uid: 1000,
            gid: 1000,
        }
    }

    #[test]
    fn matches_identical() {
        let a = create_meta(1_700_000_000, 123_456_789);
        assert!(a.matches(&a));
    }

    #[test]
    fn matches_within_one_second_window() {
        let a = create_meta(1_700_000_000, 999_999_999);
        let b = create_meta(1_700_000_001, 0);
        // only 1 ns apart across a second boundary → within tolerance
        assert!(a.matches(&b));
        assert!(b.matches(&a));
    }

    #[test]
    fn matches_at_exactly_one_second() {
        let a = create_meta(1_700_000_000, 0);
        let b = create_meta(1_700_000_001, 0);
        // exactly 1s apart → still within the inclusive window
        assert!(a.matches(&b));
    }

    #[test]
    fn does_not_match_beyond_one_second() {
        let a = create_meta(1_700_000_000, 0);
        let b = create_meta(1_700_000_001, 1);
        // just over 1s apart → outside tolerance
        assert!(!a.matches(&b));
    }

    #[test]
    fn matches_when_nsec_truncated_to_zero() {
        // simulates a FAT target: sender stores nsec=500_000_000, receiver
        // rounds to nsec=0 within the same second.
        let sent = create_meta(1_700_000_000, 500_000_000);
        let received = create_meta(1_700_000_000, 0);
        assert!(sent.matches(&received));
    }

    #[test]
    fn does_not_match_on_size_change() {
        let mut a = create_meta(1_700_000_000, 0);
        let mut b = a.clone();
        b.size = a.size + 1;
        assert!(!a.matches(&b));
        a.mtime_nsec = 0;
        b.mtime_nsec = 0;
        assert!(!a.matches(&b));
    }

    #[test]
    fn does_not_match_on_permission_change() {
        let a = create_meta(1_700_000_000, 0);
        let mut b = a.clone();
        b.permissions = 0o755;
        assert!(!a.matches(&b));
    }

    #[test]
    fn does_not_match_on_ownership_change() {
        let a = create_meta(1_700_000_000, 0);
        let mut b = a.clone();
        b.uid = 0;
        assert!(!a.matches(&b));

        let mut c = a.clone();
        c.gid = 0;
        assert!(!a.matches(&c));
    }

    #[test]
    fn mtime_matches_across_negative_timestamps() {
        // pre 1970 timestamps should also work (negative mtime_sec
        let a = create_meta(-10, 500_000_000);
        let b = create_meta(-9, 500_000_000);
        assert!(a.matches(&b));

        let c = create_meta(-10, 0);
        let d = create_meta(-8, 0);
        assert!(!c.matches(&d));
    }
}
