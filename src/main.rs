mod cli;
mod commands;
mod format;
mod fs;
mod model;

use clap::Parser;

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

        Commands::Diff { .. } => unimplemented!(),
        Commands::Apply { .. } => unimplemented!(),
        Commands::Verify { .. } => unimplemented!(),
    };
}
