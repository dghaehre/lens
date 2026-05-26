mod annotation;
mod annotation_store;
mod app;
mod context;
mod coordinates;
mod diff_algo;
pub mod highlight;
mod loader;
mod render;
mod search;
mod state;
mod sticky_lines;
pub mod theme;
mod types;
mod watcher;

use std::io;
use std::process;

use crate::commit_reference::CommitReference;
use crate::vcs::GitBackend;

pub struct DiffOptions {
    pub reference: Option<CommitReference>,
    pub file: Option<Vec<String>>,
    pub watch: bool,
    pub theme: Option<String>,
    pub stacked: bool,
    pub focus: Option<String>,
}

pub fn run_diff_ui(options: DiffOptions, backend: &GitBackend) -> io::Result<()> {
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
