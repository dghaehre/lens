use clap::{Parser, Subcommand};

use crate::commit_reference::CommitReference;

#[derive(Parser)]
#[command(name = "lumen")]
#[command(about = "Interactive side-by-side git diff viewer", long_about = None)]
#[command(version)]
pub struct Cli {
    /// Path to configuration file eg: ./path/to/lumen.config.json
    #[arg(long)]
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Launch interactive side-by-side diff viewer
    Diff {
        /// Commit reference: SHA, HEAD, HEAD~3..HEAD, main..feature, main...feature
        /// Can also be a PR number or URL (e.g., 123 or https://github.com/owner/repo/pull/123)
        #[arg(value_parser = clap::value_parser!(CommitReference))]
        reference: Option<CommitReference>,

        /// View a GitHub pull request (number or URL)
        #[arg(long)]
        pr: Option<String>,

        /// Filter to specific files
        #[arg(short, long)]
        file: Option<Vec<String>>,

        /// Watch for file changes and auto-reload
        #[arg(short, long)]
        watch: bool,

        /// Color theme (e.g., dracula, nord, gruvbox-dark, catppuccin-mocha)
        #[arg(short, long)]
        theme: Option<String>,

        /// Show commits stacked (commit-by-commit navigation with ctrl+l/h)
        #[arg(long)]
        stacked: bool,

        /// Initially focus on this file path
        #[arg(long)]
        focus: Option<String>,

        /// Origin repository in owner/repo format (default: origin git remote)
        #[arg(long)]
        origin: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses_diff_command() {
        let cli = Cli::try_parse_from(["lumen", "diff"]).unwrap();
        assert!(matches!(cli.command, Commands::Diff { .. }));
    }
}
