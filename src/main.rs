mod cli;
mod commands;
mod error;
mod format;
mod fs;
mod model;
mod parallel;
mod progress;
#[cfg(test)]
mod test_util;

use clap::Parser;
use indicatif_log_bridge::LogWrapper;

use crate::commands::apply::{detect_diff_files, run_apply};
use crate::commands::diff::run_diff;
use crate::commands::snapshot::run_snapshot;
use crate::commands::verify::run_verify;
use crate::progress::Reporter;
use cli::{Cli, Commands};

fn resolve_diff_files(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    match detect_diff_files(path) {
        Ok(files) if files.is_empty() => {
            eprintln!("Error: No diff file(s) found at {}", path.display());
            std::process::exit(1);
        }
        Ok(files) => files,
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
    }
}

fn main() {
    let reporter = Reporter::stderr();

    // Route log output through the MultiProgress so log lines suspend the
    // bars instead of interleaving with them. env_logger's own filter is
    // preserved by reading its level before we wrap it.
    let logger = env_logger::Builder::from_default_env().build();
    let level = logger.filter();
    LogWrapper::new(reporter.multi().clone(), logger)
        .try_init()
        .expect("logger initialized exactly once");
    log::set_max_level(level);

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Snapshot {
            root_dir,
            snapshot_out,
            snapshot_in,
            compress,
        } => run_snapshot(
            &root_dir,
            &snapshot_out,
            snapshot_in.as_deref(),
            compress,
            &reporter,
        ),

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
            &reporter,
        ),
        Commands::Apply { root_dir, diff_in } => {
            let diff_files = resolve_diff_files(&diff_in);
            let diff_refs: Vec<&std::path::Path> = diff_files.iter().map(|p| p.as_path()).collect();
            run_apply(&root_dir, &diff_refs, &reporter)
        }
        Commands::Verify {
            root_dir,
            diff_file,
            snapshot_file,
        } => {
            let diff_files = resolve_diff_files(&diff_file);
            let diff_refs: Vec<&std::path::Path> = diff_files.iter().map(|p| p.as_path()).collect();
            run_verify(&root_dir, &diff_refs, &snapshot_file, &reporter)
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
