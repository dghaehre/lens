mod annotation;
pub mod annotation_store;
mod app;
mod context;
mod coordinates;
mod diff_algo;
pub mod highlight;
mod loader;
mod pr_comment_editor;
mod render;
mod search;
mod state;
mod sticky_lines;
pub mod theme;
pub mod types;
mod watcher;

use std::io;
use std::process;

use crate::commit_reference::CommitReference;
use crate::vcs::{VcsBackend, VcsKind};

pub struct DiffOptions {
    pub reference: Option<CommitReference>,
    /// jj-only immutable, commit-by-commit review target. A revset may
    /// resolve to one or many commits, so it is deliberately separate from
    /// `reference`, whose established meaning is a base for a live diff.
    pub revset: Option<String>,
    pub file: Option<Vec<String>>,
    pub watch: bool,
    pub theme: Option<String>,
    pub stacked: bool,
    pub focus: Option<String>,
}

pub fn run_diff_ui(mut options: DiffOptions, backend: &dyn VcsBackend) -> io::Result<()> {
    let range = options
        .revset
        .as_deref()
        .or_else(|| options.reference.as_ref().map(|r| r.as_str()))
        .filter(|s| crate::commit_reference::split_endpoint_range(s).is_some())
        .map(str::to_string);
    if let Some(raw) = range {
        validate_endpoint_range(&raw, backend).map_err(io::Error::other)?;
        if options.stacked {
            return Err(io::Error::other(
                "--stacked requires a single base ref; use lens main --stacked",
            ));
        }
        options.reference = Some(CommitReference(raw));
        options.revset = None;
    }
    // --cached / --staged are git-only; jj has no staging area.
    if backend.kind() == VcsKind::Jj {
        if let Some(r) = options.reference.as_ref() {
            let s = r.as_str();
            if s == "--cached" || s == "--staged" {
                eprintln!(
                    "\x1b[91merror:\x1b[0m jj has no staging area; --cached/--staged are git-only"
                );
                process::exit(1);
            }
        }
    }

    if let Some(revset) = options.revset.as_deref() {
        if backend.kind() != VcsKind::Jj {
            eprintln!("\x1b[91merror:\x1b[0m --revset is only available in jj repositories");
            process::exit(1);
        }

        let commits = match backend.get_commits_for_revset(revset) {
            Ok(c) if c.is_empty() => {
                eprintln!("\x1b[91merror:\x1b[0m revset {revset:?} did not select any commits");
                process::exit(1);
            }
            Ok(c) => c,
            Err(e) => {
                eprintln!("\x1b[91merror:\x1b[0m {e}");
                process::exit(1);
            }
        };

        return app::run_app_stacked(options, commits, backend);
    }

    if options.stacked {
        let from = match options.reference.as_ref() {
            Some(r) => r.as_str().to_string(),
            None => {
                eprintln!(
                    "\x1b[91merror:\x1b[0m --stacked requires a base ref (e.g. `--stacked main`)"
                );
                process::exit(1);
            }
        };

        let commits = match backend.get_commits_in_range(&from, "HEAD") {
            Ok(c) if c.is_empty() => {
                eprintln!(
                    "\x1b[91merror:\x1b[0m No commits found in range {}..HEAD",
                    from
                );
                process::exit(1);
            }
            Ok(c) => c,
            Err(e) => {
                eprintln!("\x1b[91merror:\x1b[0m {}", e);
                process::exit(1);
            }
        };

        return app::run_app_stacked(options, commits, backend);
    }

    app::run_app(options, backend)
}

fn validate_endpoint_range(raw: &str, backend: &dyn VcsBackend) -> Result<(), String> {
    if backend.kind() != VcsKind::Jj {
        return Err("endpoint ranges are currently jj-only".into());
    }
    let (from, to) = crate::commit_reference::split_endpoint_range(raw)
        .ok_or_else(|| "expected a bounded range such as main..@".to_string())?;
    backend.refresh().map_err(|e| e.to_string())?;
    backend.resolve_ref(from).map_err(|e| e.to_string())?;
    backend.resolve_ref(to).map_err(|e| e.to_string())?;
    Ok(())
}
