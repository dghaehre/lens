use clap::Parser;
use config::cli::Cli;
use config::LensConfig;
use error::LensError;
use std::process;

mod command;
mod commit_reference;
mod config;
mod error;
mod vcs;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("\x1b[91m\rerror:\x1b[0m {e}");
        process::exit(1);
    }
}

async fn run() -> Result<(), LensError> {
    let cli = Cli::parse();

    let config = match LensConfig::build(&cli) {
        Ok(config) => config,
        Err(e) => return Err(e),
    };

    // Discover the Git backend from the current directory.
    let cwd = std::env::current_dir()?;
    let backend = vcs::GitBackend::new(&cwd)?;

    let options = command::diff::DiffOptions {
        reference: cli.reference,
        file: cli.file,
        watch: cli.watch,
        theme: cli.theme.or(config.theme.clone()),
        stacked: cli.stacked,
        focus: cli.focus,
    };
    command::diff::run_diff_ui(options, &backend)?;

    Ok(())
}
