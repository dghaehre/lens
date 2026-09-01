use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

/// Event emitted by the file-system watcher.
///
/// Source-file changes and annotation-DB changes are kept separate so the
/// main loop can refresh annotations without re-running the (more expensive)
/// git diff load.
pub enum WatchEvent {
    /// Source files changed under the working directory.
    Files { changed_files: HashSet<String> },
    /// The annotation SQLite database changed (e.g., another `lens annotation`
    /// process wrote to it).
    AnnotationDb,
}

/// Set up the watcher.
///
/// Always watches the current working directory recursively for source-file
/// changes. If `db_path` is supplied, also watches that file (and its
/// sibling WAL/journal files) so external writes to the annotation DB
/// surface as `WatchEvent::AnnotationDb`.
pub fn setup_watcher(db_path: Option<&Path>, vcs_state_dir: &Path) -> Option<Receiver<WatchEvent>> {
    let (tx, rx) = mpsc::channel();

    // Get current directory for making paths relative
    let cwd = std::env::current_dir().ok();
    let db_path_owned: Option<PathBuf> = db_path.map(|p| p.to_path_buf());
    let db_dir: Option<PathBuf> = db_path_owned
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let vcs_state_dir = vcs_state_dir.to_path_buf();

    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            if let Ok(events) = res {
                let mut changed_files = HashSet::new();
                let mut db_changed = false;
                for event in events {
                    if event.kind != DebouncedEventKind::Any {
                        continue;
                    }

                    // Match annotation DB writes (the main file plus any
                    // SQLite sidecars: -journal, -wal, -shm).
                    if let Some(ref db_path) = db_path_owned {
                        if is_annotation_db_path(&event.path, db_path) {
                            db_changed = true;
                            continue;
                        }
                    }

                    // jj snapshots update `.jj/`; treating those metadata
                    // writes as source edits would trigger another snapshot
                    // and cause an endless watch/reload loop. The same rule
                    // also keeps ordinary Git metadata changes out of the
                    // source-file reload path. lens.db was handled above.
                    if event.path.starts_with(&vcs_state_dir) {
                        continue;
                    }

                    // Normalize path to be relative from current directory
                    // This matches the format used in file_diffs.filename (e.g., "src/foo.rs")
                    let path_str = if let Some(ref cwd) = cwd {
                        event
                            .path
                            .strip_prefix(cwd)
                            .unwrap_or(&event.path)
                            .to_string_lossy()
                            .to_string()
                    } else {
                        event.path.to_string_lossy().to_string()
                    };
                    // Also strip leading "./" if present
                    let normalized = path_str.strip_prefix("./").unwrap_or(&path_str);
                    changed_files.insert(normalized.to_string());
                }
                if !changed_files.is_empty() {
                    let _ = tx.send(WatchEvent::Files { changed_files });
                }
                if db_changed {
                    let _ = tx.send(WatchEvent::AnnotationDb);
                }
            }
        },
    )
    .ok()?;

    debouncer
        .watcher()
        .watch(Path::new("."), notify::RecursiveMode::Recursive)
        .ok()?;

    // Also explicitly watch the directory holding lens.db (usually `.git/`)
    // so external CLI writes surface even when it lies outside the cwd
    // or is filtered by something upstream. Non-recursive: we only care
    // about lens.db and its WAL/journal sidecars.
    if let Some(ref dir) = db_dir {
        let _ = debouncer
            .watcher()
            .watch(dir, notify::RecursiveMode::NonRecursive);
    }

    std::mem::forget(debouncer);

    Some(rx)
}

/// True if `path` is the annotation DB itself or one of its SQLite sidecars
/// (`-journal`, `-wal`, `-shm`). Compared by filename so symlink/canonical
/// differences between `event.path` and `db_path` don't cause false negatives.
fn is_annotation_db_path(path: &Path, db_path: &Path) -> bool {
    let (Some(event_name), Some(db_name)) = (
        path.file_name().and_then(|s| s.to_str()),
        db_path.file_name().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    if event_name == db_name {
        return true;
    }
    if let Some(rest) = event_name.strip_prefix(db_name) {
        matches!(rest, "-journal" | "-wal" | "-shm")
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The annotation-DB check in the event loop runs *before* the
    /// `starts_with(vcs_state_dir)` skip. That ordering is load-bearing:
    /// `lens.db` lives inside the state dir (`.git/` or `.jj/`), so if the
    /// skip ran first the DB write would be swallowed as VCS metadata and
    /// external `lens annotation` writes would never reach the TUI.
    #[test]
    fn db_under_vcs_state_dir_classified_as_db() {
        let db = Path::new("/repo/.jj/lens.db");
        assert!(is_annotation_db_path(Path::new("/repo/.jj/lens.db"), db));
        assert!(is_annotation_db_path(
            Path::new("/repo/.jj/lens.db-wal"),
            db
        ));
        assert!(is_annotation_db_path(
            Path::new("/repo/.jj/lens.db-shm"),
            db
        ));
        assert!(is_annotation_db_path(
            Path::new("/repo/.jj/lens.db-journal"),
            db
        ));

        // Other writes under the state dir are not the DB, and fall through
        // to the skip.
        assert!(!is_annotation_db_path(
            Path::new("/repo/.jj/working_copy/tree_state"),
            db
        ));
    }

    /// `is_annotation_db_path` compares by filename, so notify's event path
    /// and the configured db path can disagree on symlink resolution
    /// (macOS reports `/private/var` for a `/var` tempdir) without causing
    /// a false negative.
    #[test]
    fn is_annotation_db_path_matches_across_canonicalization() {
        assert!(is_annotation_db_path(
            Path::new("/private/var/t/repo/.jj/lens.db"),
            Path::new("/var/t/repo/.jj/lens.db"),
        ));
    }
}
