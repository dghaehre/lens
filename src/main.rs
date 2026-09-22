use clap::Parser;
use config::cli::{Cli, Command};
use config::LensConfig;
use error::LensError;
use std::path::{Path, PathBuf};
use std::process;
use vcs::{GitBackend, JjBackend, VcsBackend, VcsError};

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

/// Walk up from `start` looking for `.jj/` (preferred) or `.git/`. Returns
/// the first containing directory and which marker it found. `.jj/` wins
/// when both are present at the same level (colocated repos): jj users
/// who've set up jj want jj semantics.
fn discover_repo(start: &Path) -> Option<(PathBuf, &'static str)> {
    let mut cur = start;
    loop {
        if cur.join(".jj").is_dir() {
            return Some((cur.to_path_buf(), "jj"));
        }
        if cur.join(".git").exists() {
            return Some((cur.to_path_buf(), "git"));
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

async fn run() -> Result<(), LensError> {
    let cli = Cli::parse();

    // `skill` doesn't touch the repo — handle it before we require a
    // backend so `lens skill install` works outside a working tree.
    if let Some(Command::Skill(cmd)) = cli.command.clone() {
        return command::skill::run(cmd);
    }

    let config = LensConfig::build(&cli)?;

    let cwd = std::env::current_dir()?;
    let backend: Box<dyn VcsBackend> = match discover_repo(&cwd) {
        Some((root, "jj")) => Box::new(JjBackend::new(&root)?),
        Some((root, "git")) => Box::new(GitBackend::new(&root)?),
        Some((_, kind)) => unreachable!("unexpected repo kind: {kind}"),
        None => return Err(VcsError::NotARepository.into()),
    };

    match cli.command {
        Some(Command::Annotation(cmd)) => command::annotation::run(cmd, backend.as_ref()),
        Some(Command::Skill(_)) => unreachable!("handled above"),
        None => {
            let options = command::diff::DiffOptions {
                reference: cli.diff.reference,
                revset: cli.diff.revset,
                file: cli.diff.file,
                watch: !cli.diff.no_watch,
                theme: cli.diff.theme.or(config.theme.clone()),
                stacked: cli.diff.stacked,
                focus: cli.diff.focus,
            };
            command::diff::run_diff_ui(options, backend.as_ref())?;
            Ok(())
        }
    }
}
