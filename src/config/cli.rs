use clap::{Args, Parser, Subcommand};

use crate::commit_reference::CommitReference;

/// Top-level CLI. With no subcommand, the existing diff TUI runs (preserving
/// backward compatibility with `lens [reference]`). Subcommands provide the
/// scripting surface for agents.
#[derive(Parser)]
#[command(name = "lens")]
#[command(about = "Interactive side-by-side git diff viewer", long_about = None)]
#[command(version)]
pub struct Cli {
    /// Path to configuration file eg: ./path/to/lens.config.json
    #[arg(long, global = true)]
    pub config: Option<String>,

    #[command(flatten)]
    pub diff: DiffArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Arguments to the default (diff) action. Flattened into the top-level so
/// `lens main` still works without a subcommand.
#[derive(Args, Debug, Clone)]
pub struct DiffArgs {
    /// Ref to diff the working tree against (defaults to HEAD).
    /// Examples: `main`, `HEAD~3`, a SHA.
    #[arg(value_parser = clap::value_parser!(CommitReference))]
    pub reference: Option<CommitReference>,

    /// Filter to specific files
    #[arg(short, long)]
    pub file: Option<Vec<String>>,

    /// Disable watching for file changes (auto-reload is on by default).
    #[arg(long)]
    pub no_watch: bool,

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

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Read or write annotations stored alongside the repo.
    #[command(subcommand)]
    Annotation(AnnotationCommand),
    /// Install the bundled SKILL.md so coding agents pick it up.
    #[command(subcommand)]
    Skill(SkillCommand),
}

#[derive(Subcommand, Debug, Clone)]
pub enum SkillCommand {
    /// Install the bundled SKILL.md for a given agent provider.
    Install(SkillInstallArgs),
}

#[derive(Args, Debug, Clone)]
pub struct SkillInstallArgs {
    /// Which agent provider to install for.
    #[arg(value_enum)]
    pub provider: SkillProvider,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum SkillProvider {
    Claude,
    Codex,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AnnotationCommand {
    /// List annotations as JSON.
    List(AnnotationListArgs),
    /// Create a new annotation thread.
    New(AnnotationNewArgs),
    /// Append a reply to an existing annotation thread.
    Reply(AnnotationReplyArgs),
    /// Delete an annotation thread (and its replies).
    Delete(AnnotationDeleteArgs),
}

#[derive(Args, Debug, Clone)]
pub struct AnnotationListArgs {
    /// Restrict to annotations on a specific file (repeatable).
    #[arg(short, long)]
    pub file: Option<Vec<String>>,
    /// Restrict to a single annotation id.
    #[arg(long)]
    pub id: Option<i64>,
}

#[derive(Args, Debug, Clone)]
pub struct AnnotationNewArgs {
    /// File the annotation is anchored to (repo-relative path).
    #[arg(short, long)]
    pub file: String,
    /// First line of the range (1-based). Omit to annotate the whole file.
    #[arg(long)]
    pub line: Option<usize>,
    /// Last line of the range. Defaults to `--line` when only one line is given.
    #[arg(long)]
    pub line_end: Option<usize>,
    /// Which side of the diff the line refers to.
    #[arg(long, value_enum, default_value_t = PanelArg::New)]
    pub panel: PanelArg,
    /// Source tag recorded on the annotation. Defaults to "agent" since this
    /// CLI is meant for programmatic callers; override with --source cli when
    /// invoking manually.
    #[arg(long, default_value = "agent")]
    pub source: String,
    /// Annotation body.
    pub content: String,
}

#[derive(Args, Debug, Clone)]
pub struct AnnotationReplyArgs {
    /// Id of the annotation to reply to.
    #[arg(long)]
    pub id: i64,
    /// Source tag recorded on the reply (see `new --source`).
    #[arg(long, default_value = "agent")]
    pub source: String,
    /// Reply body.
    pub content: String,
}

#[derive(Args, Debug, Clone)]
pub struct AnnotationDeleteArgs {
    /// Id of the annotation to delete.
    #[arg(long)]
    pub id: i64,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum PanelArg {
    Old,
    New,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parses_without_subcommand() {
        let cli = Cli::try_parse_from(["lens"]).unwrap();
        assert!(cli.diff.reference.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parses_with_reference() {
        let cli = Cli::try_parse_from(["lens", "main"]).unwrap();
        assert!(cli.diff.reference.is_some());
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_annotation_list_parses() {
        let cli = Cli::try_parse_from(["lens", "annotation", "list"]).unwrap();
        match cli.command {
            Some(Command::Annotation(AnnotationCommand::List(args))) => {
                assert!(args.file.is_none());
                assert!(args.id.is_none());
            }
            other => panic!("expected annotation list, got {:?}", other),
        }
    }

    #[test]
    fn test_annotation_new_requires_file_and_content() {
        let cli = Cli::try_parse_from([
            "lens",
            "annotation",
            "new",
            "--file",
            "src/main.rs",
            "--line",
            "10",
            "looks off",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Annotation(AnnotationCommand::New(args))) => {
                assert_eq!(args.file, "src/main.rs");
                assert_eq!(args.line, Some(10));
                assert_eq!(args.content, "looks off");
                assert_eq!(args.source, "agent");
            }
            other => panic!("expected annotation new, got {:?}", other),
        }
    }

    #[test]
    fn test_annotation_reply_parses() {
        let cli = Cli::try_parse_from([
            "lens",
            "annotation",
            "reply",
            "--id",
            "42",
            "ack",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Annotation(AnnotationCommand::Reply(args))) => {
                assert_eq!(args.id, 42);
                assert_eq!(args.content, "ack");
            }
            other => panic!("expected annotation reply, got {:?}", other),
        }
    }
}
