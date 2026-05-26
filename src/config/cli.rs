use clap::Parser;

use crate::commit_reference::CommitReference;

#[derive(Parser)]
#[command(name = "lens")]
#[command(about = "Interactive side-by-side git diff viewer", long_about = None)]
#[command(version)]
pub struct Cli {
    /// Path to configuration file eg: ./path/to/lens.config.json
    #[arg(long)]
    pub config: Option<String>,

    /// Ref to diff the working tree against (defaults to HEAD).
    /// Examples: `main`, `HEAD~3`, a SHA.
    #[arg(value_parser = clap::value_parser!(CommitReference))]
    pub reference: Option<CommitReference>,

    /// Filter to specific files
    #[arg(short, long)]
    pub file: Option<Vec<String>>,

    /// Watch for file changes and auto-reload
    #[arg(short, long)]
    pub watch: bool,

    /// Color theme (e.g., dracula, nord, gruvbox-dark, catppuccin-mocha)
    #[arg(short, long)]
    pub theme: Option<String>,

    /// Show commits stacked (commit-by-commit navigation with ctrl+l/h).
    /// Requires a base ref (walks <ref>..HEAD).
    #[arg(long)]
    pub stacked: bool,

    /// Initially focus on this file path
    #[arg(long)]
    pub focus: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses_without_subcommand() {
        let cli = Cli::try_parse_from(["lens"]).unwrap();
        assert!(cli.reference.is_none());
    }

    #[test]
    fn test_cli_parses_with_reference() {
        let cli = Cli::try_parse_from(["lens", "main"]).unwrap();
        assert!(cli.reference.is_some());
    }
}
