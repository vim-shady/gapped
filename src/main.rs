mod cli;
mod commands;
mod format;
mod fs;
mod model;

use clap::Parser;

use crate::commands::apply::run_apply;
use crate::commands::diff::run_diff;
use crate::commands::snapshot::run_snapshot;
use cli::{Cli, Commands};

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Snapshot {
            root_dir,
            snapshot_out,
            snapshot_in,
            compress,
        } => run_snapshot(&root_dir, &snapshot_out, snapshot_in.as_deref(), compress),

        Commands::Diff {
            root_dir,
            snapshot_in,
            diff_out,
            snapshot_out,
            split_size,
            compress,
        } => run_diff(
            &root_dir,
            &snapshot_in,
            &diff_out,
            &snapshot_out,
            split_size,
            compress,
        ),
        Commands::Apply { root_dir, diff_in } => run_apply(&root_dir, &diff_in),
        Commands::Verify { .. } => unimplemented!(),
    };
}
