mod cli;
mod commands;
mod error;
mod format;
mod fs;
mod model;
#[cfg(test)]
mod test_util;

use clap::Parser;

use crate::commands::apply::{detect_diff_files, run_apply};
use crate::commands::diff::run_diff;
use crate::commands::snapshot::run_snapshot;
use crate::commands::verify::run_verify;
use cli::{Cli, Commands};

fn main() {
    env_logger::init();

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
        Commands::Apply { root_dir, diff_in } => {
            let diff_files = match detect_diff_files(&diff_in) {
                Ok(files) if files.is_empty() => {
                    eprintln!("Error: No diff file(s) found at {}", diff_in.display());
                    std::process::exit(1);
                }
                Ok(files) => files,
                Err(e) => { eprintln!("Error: {:#}", e); std::process::exit(1); }
            };
            let diff_refs: Vec<&std::path::Path> = diff_files.iter().map(|p| p.as_path()).collect();
            run_apply(&root_dir, &diff_refs)
        }
        Commands::Verify {
            root_dir,
            diff_file,
            snapshot_file,
        } => {
            let diff_files = match detect_diff_files(&diff_file) {
                Ok(files) if files.is_empty() => {
                    eprintln!("Error: No diff file(s) found at {}", diff_file.display());
                    std::process::exit(1);
                }
                Ok(files) => files,
                Err(e) => { eprintln!("Error: {:#}", e); std::process::exit(1); }
            };
            let diff_refs: Vec<&std::path::Path> = diff_files.iter().map(|p| p.as_path()).collect();
            run_verify(&root_dir, &diff_refs, &snapshot_file)
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
