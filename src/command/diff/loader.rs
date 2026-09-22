use std::fs;
use std::path::Path;

use super::types::{is_binary_content, FileDiff, FileStatus};
use super::DiffOptions;
use crate::vcs::VcsBackend;

pub fn get_current_branch(backend: &dyn VcsBackend) -> String {
    backend
        .get_current_branch()
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string())
}

/// Which of the three `git diff` semantics the user has selected.
///
/// Mirrors `git diff`'s own modes:
/// - `Unstaged` — `git diff` (index → worktree)
/// - `Staged`   — `git diff --cached` (HEAD → index)
/// - `Ref(r)`   — `git diff <ref>` (ref → worktree)
pub enum DiffMode {
    Unstaged,
    Staged,
    Ref(String),
}

impl DiffMode {
    pub fn from_options(options: &DiffOptions) -> Self {
        match options.reference.as_ref().map(|r| r.as_str()) {
            None => DiffMode::Unstaged,
            Some(s) if s == "--cached" || s == "--staged" => DiffMode::Staged,
            Some(s) => DiffMode::Ref(s.to_string()),
        }
    }
}

/// Get the list of files changed for the selected diff mode.
pub fn get_changed_files(options: &DiffOptions, backend: &dyn VcsBackend) -> Vec<String> {
    let mode = DiffMode::from_options(options);

    let files: Vec<String> = match &mode {
        DiffMode::Unstaged => backend.get_unstaged_changed_files().unwrap_or_default(),
        DiffMode::Staged => backend.get_staged_changed_files().unwrap_or_default(),
        DiffMode::Ref(r) if r == "HEAD" => {
            backend.get_working_tree_changed_files().unwrap_or_default()
        }
        DiffMode::Ref(r) => {
            // Union of `r..HEAD` and the working tree.
            let range_files = backend
                .get_range_changed_files(r, "HEAD")
                .unwrap_or_default();
            let wt_files = backend.get_working_tree_changed_files().unwrap_or_default();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut combined: Vec<String> = Vec::new();
            for f in range_files.into_iter().chain(wt_files.into_iter()) {
                if seen.insert(f.clone()) {
                    combined.push(f);
                }
            }
            combined
        }
    };

    if let Some(ref filter) = options.file {
        files.into_iter().filter(|f| filter.contains(f)).collect()
    } else {
        files
    }
}

/// Get content of a file at the "old" side of the diff for the selected mode.
pub fn get_old_content(filename: &str, mode: &DiffMode, backend: &dyn VcsBackend) -> String {
    let path = Path::new(filename);
    match mode {
        DiffMode::Unstaged => backend.get_file_content_at_index(path).unwrap_or_default(),
        DiffMode::Staged => backend
            .get_file_content_at_ref("HEAD", path)
            .unwrap_or_default(),
        DiffMode::Ref(r) => backend.get_file_content_at_ref(r, path).unwrap_or_default(),
    }
}

/// Get content of a file at the "new" side of the diff for the selected mode.
pub fn get_new_content(filename: &str, mode: &DiffMode, backend: &dyn VcsBackend) -> String {
    match mode {
        DiffMode::Staged => backend
            .get_file_content_at_index(Path::new(filename))
            .unwrap_or_default(),
        DiffMode::Unstaged | DiffMode::Ref(_) => fs::read_to_string(filename).unwrap_or_default(),
    }
}

pub fn load_file_diffs(options: &DiffOptions, backend: &dyn VcsBackend) -> Vec<FileDiff> {
    // jj represents uncommitted edits by snapshotting them into `@`. Refresh
    // before every load so both the initial view and watch-mode reloads see
    // changes written after Lens started. Git's implementation is a no-op.
    let _ = backend.refresh();

    let mode = DiffMode::from_options(options);
    get_changed_files(options, backend)
        .into_iter()
        .map(|filename| {
            let old_content = get_old_content(&filename, &mode, backend);
            let new_content = get_new_content(&filename, &mode, backend);
            let status = if old_content.is_empty() && !new_content.is_empty() {
                FileStatus::Added
            } else if !old_content.is_empty() && new_content.is_empty() {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };
            let is_binary = is_binary_content(&old_content) || is_binary_content(&new_content);
            FileDiff {
                filename,
                old_content,
                new_content,
                status,
                is_binary,
            }
        })
        .collect()
}

/// Load file diffs for a single commit (comparing commit to its parent).
/// Used by stacked mode to render each commit individually.
pub fn load_single_commit_diffs(
    commit_id: &str,
    file_filter: &Option<Vec<String>>,
    backend: &dyn VcsBackend,
) -> Vec<FileDiff> {
    let files = backend.get_changed_files(commit_id).unwrap_or_default();

    let files: Vec<String> = if let Some(ref filter) = file_filter {
        files.into_iter().filter(|f| filter.contains(f)).collect()
    } else {
        files
    };

    // Get parent ref (handles root commits gracefully)
    let parent_ref = backend
        .get_parent_ref_or_empty(commit_id)
        .unwrap_or_default();

    files
        .into_iter()
        .map(|filename| {
            let path = Path::new(&filename);

            let old_content = if parent_ref.is_empty() {
                String::new()
            } else {
                backend
                    .get_file_content_at_ref(&parent_ref, path)
                    .unwrap_or_default()
            };

            let new_content = backend
                .get_file_content_at_ref(commit_id, path)
                .unwrap_or_default();

            let status = if old_content.is_empty() && !new_content.is_empty() {
                FileStatus::Added
            } else if !old_content.is_empty() && new_content.is_empty() {
                FileStatus::Deleted
            } else {
                FileStatus::Modified
            };

            let is_binary = is_binary_content(&old_content) || is_binary_content(&new_content);
            FileDiff {
                filename,
                old_content,
                new_content,
                status,
                is_binary,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::test_utils::{git, make_temp_dir, RepoGuard};
    use crate::vcs::{GitBackend, JjBackend, VcsBackend};
    use std::fs;
    use std::process::Command;

    fn jj_available() -> bool {
        Command::new("jj")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn run_jj(cwd: &Path, args: &[&str]) {
        let output = Command::new("jj")
            .args(args)
            .current_dir(cwd)
            .env("JJ_USER", "lens test")
            .env("JJ_EMAIL", "lens@test")
            .output()
            .expect("jj command should spawn");
        assert!(
            output.status.success(),
            "jj {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn test_load_file_diffs_working_tree_untracked_in_new_dir() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-wt-untracked-dir");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Initial commit
        fs::write(dir.join("existing.txt"), "existing content\n").expect("write existing");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial"]);

        // Create untracked files in a NEW directory
        fs::create_dir_all(dir.join("new_dir")).expect("create new dir");
        fs::write(dir.join("new_dir/file1.txt"), "file 1\n").expect("write file1");
        fs::write(dir.join("new_dir/file2.txt"), "file 2\n").expect("write file2");

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let options = super::super::DiffOptions {
            reference: None,
            revset: None,
            file: None,
            watch: false,
            theme: None,
            stacked: false,
            focus: None,
        };

        let diffs = load_file_diffs(&options, &backend);
        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();

        assert!(
            diffs.iter().any(|d| d.filename == "new_dir/file1.txt"),
            "should include new_dir/file1.txt, got: {:?}",
            filenames
        );
        assert!(
            diffs.iter().any(|d| d.filename == "new_dir/file2.txt"),
            "should include new_dir/file2.txt, got: {:?}",
            filenames
        );

        let file1 = diffs
            .iter()
            .find(|d| d.filename == "new_dir/file1.txt")
            .unwrap();
        assert_eq!(file1.status, FileStatus::Added);
        assert!(file1.old_content.is_empty());
        assert_eq!(file1.new_content, "file 1\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_file_diffs_worktree_vs_earlier_ref() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-worktree-vs-ref");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Base commit (will be referenced as HEAD~1 later)
        fs::write(dir.join("base.txt"), "base\n").expect("write base");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "base"]);

        // Commit on top — committed change
        fs::write(dir.join("committed.txt"), "committed\n").expect("write committed");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "add committed"]);

        // Working tree changes (uncommitted)
        fs::write(dir.join("uncommitted.txt"), "uncommitted\n").expect("write uncommitted");
        fs::write(dir.join("base.txt"), "base modified\n").expect("modify base");

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = crate::vcs::GitBackend::from_cwd().expect("should open repo");
        let options = super::super::DiffOptions {
            reference: Some(crate::commit_reference::CommitReference(
                "HEAD~1".to_string(),
            )),
            revset: None,
            file: None,
            watch: false,
            theme: None,
            stacked: false,
            focus: None,
        };

        let diffs = load_file_diffs(&options, &backend);
        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();

        assert!(
            filenames.contains(&"committed.txt"),
            "should include committed file, got: {:?}",
            filenames
        );
        assert!(
            filenames.contains(&"uncommitted.txt"),
            "should include untracked working tree file, got: {:?}",
            filenames
        );
        assert!(
            filenames.contains(&"base.txt"),
            "should include modified working tree file, got: {:?}",
            filenames
        );

        // base.txt: old=from HEAD~1 ("base\n"), new=working tree ("base modified\n")
        let base = diffs.iter().find(|d| d.filename == "base.txt").unwrap();
        assert_eq!(base.old_content, "base\n");
        assert_eq!(base.new_content, "base modified\n");

        // committed.txt: old=empty (not in HEAD~1), new=fs content
        let committed = diffs
            .iter()
            .find(|d| d.filename == "committed.txt")
            .unwrap();
        assert_eq!(committed.old_content, "");
        assert_eq!(committed.new_content, "committed\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_commit_diffs_added_file() {
        let _repo = RepoGuard::new();
        let backend = GitBackend::from_cwd().expect("should open repo");

        let diffs = load_single_commit_diffs("HEAD", &None, &backend);

        assert_eq!(diffs.len(), 1, "should have 1 file diff");
        assert_eq!(diffs[0].filename, "README.md");
        assert_eq!(diffs[0].status, FileStatus::Added);
        assert!(
            diffs[0].old_content.is_empty(),
            "old content should be empty for added file"
        );
        assert_eq!(diffs[0].new_content.trim(), "hello");
    }

    #[test]
    fn test_load_single_commit_diffs_modified_file() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-modified");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        fs::write(dir.join("file.txt"), "original content\n").expect("write file");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "first"]);

        fs::write(dir.join("file.txt"), "modified content\n").expect("modify file");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "second"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let diffs = load_single_commit_diffs("HEAD", &None, &backend);

        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename, "file.txt");
        assert_eq!(diffs[0].status, FileStatus::Modified);
        assert_eq!(diffs[0].old_content, "original content\n");
        assert_eq!(diffs[0].new_content, "modified content\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_commit_diffs_multiple_files() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-multi");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        fs::write(dir.join("a.txt"), "file a\n").expect("write a");
        fs::write(dir.join("b.txt"), "file b\n").expect("write b");
        fs::write(dir.join("c.txt"), "file c\n").expect("write c");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "multi"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let diffs = load_single_commit_diffs("HEAD", &None, &backend);

        assert_eq!(diffs.len(), 3, "should have 3 file diffs");

        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();
        assert!(filenames.contains(&"a.txt"));
        assert!(filenames.contains(&"b.txt"));
        assert!(filenames.contains(&"c.txt"));

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_commit_diffs_with_filter() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-diff-filter");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        fs::write(dir.join("wanted.txt"), "wanted\n").expect("write wanted");
        fs::write(dir.join("unwanted.txt"), "unwanted\n").expect("write unwanted");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "filter test"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let filter = Some(vec!["wanted.txt".to_string()]);
        let diffs = load_single_commit_diffs("HEAD", &filter, &backend);

        assert_eq!(diffs.len(), 1, "filter should limit to 1 file");
        assert_eq!(diffs[0].filename, "wanted.txt");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stacked_diff_integration_git() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = make_temp_dir("git-stacked-integration");
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Base commit
        fs::write(dir.join("base.txt"), "base\n").expect("write base");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "base"]);

        // Commit A
        fs::write(dir.join("a.txt"), "commit A\n").expect("write a");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "commit A"]);

        // Commit B
        fs::write(dir.join("b.txt"), "commit B\n").expect("write b");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "commit B"]);

        std::env::set_current_dir(&dir).expect("set cwd");

        let backend = GitBackend::from_cwd().expect("should open repo");

        let commits = backend
            .get_commits_in_range("HEAD~2", "HEAD")
            .expect("should get commits");

        assert_eq!(commits.len(), 2, "should have 2 commits");
        assert_eq!(commits[0].summary, "commit A");
        assert_eq!(commits[1].summary, "commit B");

        let diffs_a = load_single_commit_diffs(&commits[0].commit_id, &None, &backend);
        assert_eq!(diffs_a.len(), 1);
        assert_eq!(diffs_a[0].filename, "a.txt");
        assert_eq!(diffs_a[0].new_content, "commit A\n");

        let diffs_b = load_single_commit_diffs(&commits[1].commit_id, &None, &backend);
        assert_eq!(diffs_b.len(), 1);
        assert_eq!(diffs_b[0].filename, "b.txt");
        assert_eq!(diffs_b[0].new_content, "commit B\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Set up a repo where one file has staged changes and another has
    /// unstaged-on-top-of-staged changes. Used by the mode-isolation tests.
    fn setup_mixed_repo(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = make_temp_dir(label);
        let original = std::env::current_dir().expect("get cwd");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test User"]);

        // Initial commit
        fs::write(dir.join("only_staged.txt"), "base\n").expect("write only_staged");
        fs::write(dir.join("both.txt"), "base\n").expect("write both");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial"]);

        // Stage a change to only_staged.txt — no further worktree edit
        fs::write(dir.join("only_staged.txt"), "staged value\n").expect("modify only_staged");
        git(&dir, &["add", "only_staged.txt"]);

        // Stage a change to both.txt, then add a further unstaged edit on top
        fs::write(dir.join("both.txt"), "staged value\n").expect("stage both");
        git(&dir, &["add", "both.txt"]);
        fs::write(dir.join("both.txt"), "worktree value\n").expect("worktree both");

        std::env::set_current_dir(&dir).expect("set cwd");
        (dir, original)
    }

    #[test]
    fn test_load_file_diffs_unstaged_only_hides_staged_only_files() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (dir, original) = setup_mixed_repo("git-diff-mode-unstaged");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let options = super::super::DiffOptions {
            reference: None, // unstaged mode
            revset: None,
            file: None,
            watch: false,
            theme: None,
            stacked: false,
            focus: None,
        };

        let diffs = load_file_diffs(&options, &backend);
        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();

        assert!(
            !filenames.contains(&"only_staged.txt"),
            "unstaged mode must not include staged-only file, got: {:?}",
            filenames
        );
        let both = diffs
            .iter()
            .find(|d| d.filename == "both.txt")
            .expect("unstaged mode should include file with worktree-vs-index diff");
        // old = index (staged value), new = worktree
        assert_eq!(both.old_content, "staged value\n");
        assert_eq!(both.new_content, "worktree value\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_file_diffs_staged_only_hides_unstaged_changes() {
        let _lock = crate::vcs::test_utils::cwd_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (dir, original) = setup_mixed_repo("git-diff-mode-staged");

        let backend = GitBackend::from_cwd().expect("should open repo");
        let options = super::super::DiffOptions {
            reference: Some(crate::commit_reference::CommitReference(
                "--cached".to_string(),
            )),
            revset: None,
            file: None,
            watch: false,
            theme: None,
            stacked: false,
            focus: None,
        };

        let diffs = load_file_diffs(&options, &backend);
        let filenames: Vec<&str> = diffs.iter().map(|d| d.filename.as_str()).collect();

        let only_staged = diffs
            .iter()
            .find(|d| d.filename == "only_staged.txt")
            .unwrap_or_else(|| {
                panic!(
                    "staged mode should include only_staged.txt, got: {:?}",
                    filenames
                )
            });
        // old = HEAD, new = index
        assert_eq!(only_staged.old_content, "base\n");
        assert_eq!(only_staged.new_content, "staged value\n");

        // For `both.txt`, the staged diff shows base → staged (not the
        // worktree edit on top).
        let both = diffs.iter().find(|d| d.filename == "both.txt").unwrap();
        assert_eq!(both.old_content, "base\n");
        assert_eq!(both.new_content, "staged value\n");

        let _ = std::env::set_current_dir(&original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_single_jj_commit_diffs_never_read_working_copy_files() {
        if !jj_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        run_jj(path, &["--config", "git.colocate=false", "git", "init"]);

        fs::write(path.join("tracked.txt"), "committed\n").unwrap();
        run_jj(path, &["commit", "-m", "committed change"]);

        let backend = JjBackend::new(path).unwrap();
        let commit_id = backend.resolve_ref("@-").unwrap();

        // Do not refresh: this is intentionally different from the commit
        // tree and must never become the "new" side of historical review.
        fs::write(path.join("tracked.txt"), "working copy only\n").unwrap();

        let diffs = load_single_commit_diffs(&commit_id, &None, &backend);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].filename, "tracked.txt");
        assert_eq!(diffs[0].new_content, "committed\n");
        assert_ne!(diffs[0].new_content, "working copy only\n");
    }
}
