use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "gapped")]
#[command(about = "Offline file synchronizer for air-gapped systems")]
#[command(version = "0.1.0")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a snapshot of the current filesystem state
    Snapshot {
        /// The root directory to snapshot
        root_dir: PathBuf,

        /// Output snapshot file
        snapshot_out: PathBuf,

        /// Optional previous snapshot file
        snapshot_in: Option<PathBuf>,

        /// Compress output
        #[arg(long, short)]
        compress: bool,
    },

    /// Compute differences between current filesystem and snapshot
    Diff {
        /// The root directory to diff against
        root_dir: PathBuf,

        /// Input snapshot file (previous state)
        snapshot_in: PathBuf,

        /// Output diff file
        diff_out: PathBuf,

        /// Output snapshot file (current state)
        snapshot_out: PathBuf,

        /// Split size for diff
        #[arg(long)]
        split_size: Option<u64>,

        /// Compress the output
        #[arg(long, short)]
        compress: bool,
    },

    /// Apply a diff to the target filesystem
    Apply {
        /// The root directory to apply the diff to
        root_dir: PathBuf,

        /// Diff file to apply (or base name for split diffs)
        diff_in: PathBuf,
    },

    /// Verify that applying diff produces expected state
    Verify {
        /// The root directory to verify against
        root_dir: PathBuf,

        /// Diff file to verify
        diff_file: PathBuf,

        /// Target snapshot file
        snapshot_file: PathBuf,
    }
}