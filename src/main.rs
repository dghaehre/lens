use clap::Parser;
use config::cli::{Cli, Command};
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

    let config = LensConfig::build(&cli)?;

    let cwd = std::env::current_dir()?;
    let backend = vcs::GitBackend::new(&cwd)?;

    match cli.command {
        Some(Command::Annotation(cmd)) => command::annotation::run(cmd, &backend),
        None => {
            let options = command::diff::DiffOptions {
                reference: cli.diff.reference,
                file: cli.diff.file,
                watch: !cli.diff.no_watch,
                theme: cli.diff.theme.or(config.theme.clone()),
                stacked: cli.diff.stacked,
                focus: cli.diff.focus,
            };
            command::diff::run_diff_ui(options, &backend)?;
            Ok(())
        }
    }
}
