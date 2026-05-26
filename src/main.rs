use clap::Parser;
use config::cli::{Cli, Commands};
use config::LumenConfig;
use error::LumenError;
use std::process;
use vcs::VcsBackendType;

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

async fn run() -> Result<(), LumenError> {
    let cli = Cli::parse();

    let config = match LumenConfig::build(&cli) {
        Ok(config) => config,
        Err(e) => return Err(e),
    };

    // Get VCS backend based on CLI override or auto-detection
    let cwd = std::env::current_dir()?;
    let vcs_override = cli.vcs.map(VcsBackendType::from);
    let backend = vcs::get_backend(&cwd, vcs_override)?;

    match cli.command {
        Commands::Diff {
            reference,
            pr,
            file,
            watch,
            theme,
            stacked,
            focus,
            origin,
        } => {
            let options = command::diff::DiffOptions {
                reference,
                pr,
                file,
                watch,
                theme: theme.or(config.theme.clone()),
                stacked,
                focus,
                origin,
            };
            command::diff::run_diff_ui(options, backend.as_ref())?;
        }
    }

    Ok(())
}
