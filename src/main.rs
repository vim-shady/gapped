mod cli;
mod model;
mod fs;

use clap::Parser;

use cli::{Cli, Commands};

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Snapshot { .. } => unimplemented!(),
        Commands::Diff { .. } => unimplemented!(),
        Commands::Apply { .. } => unimplemented!(),
        Commands::Verify { .. } => unimplemented!(),
    };
}
