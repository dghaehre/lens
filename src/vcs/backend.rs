use std::path::Path;
use thiserror::Error;

/// Error types for VCS operations.
#[derive(Error, Debug)]
pub enum VcsError {
    #[error("invalid reference: {0}")]
    InvalidRef(String),

    #[error("file not found: {0}")]
    FileNotFound(String),

    #[error("not a repository")]
    NotARepository,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

/// Lightweight commit info for stacked diff navigation.
/// Unlike CommitInfo, this doesn't include the full diff content.
#[derive(Clone, Debug)]
pub struct StackedCommitInfo {
    /// Full commit SHA.
    pub commit_id: String,
    /// Short ID for display (7-12 chars)
    pub short_id: String,
    /// First line of commit message
    pub summary: String,
}

/// Information about a commit from any VCS.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields populated by VCS backends; consumed by diff UI / future commands
pub struct CommitInfo {
    /// The commit SHA.
    pub commit_id: String,
    /// Commit message
    pub message: String,
    /// Diff content
    pub diff: String,
    /// Author name and email
    pub author: String,
    /// Commit timestamp formatted for display (YYYY-MM-DD HH:MM:SS)
    pub date: String,
}

/// Abstraction over Git repository operations.
///
/// This trait intentionally does not require `Send + Sync` bounds because the
/// backend is used synchronously from a single thread.
#[allow(dead_code)] // Not all methods used by all commands yet
pub trait VcsBackend {
    /// Get commit info for a reference (SHA, HEAD, branch name, etc.)
    fn get_commit(&self, reference: &str) -> Result<CommitInfo, VcsError>;

    /// Get diff of uncommitted changes (working tree vs HEAD/parent).
    /// `staged` selects index changes instead of unstaged working tree changes.
    fn get_working_tree_diff(&self, staged: bool) -> Result<String, VcsError>;

    /// Get diff between two refs (e.g., commit1..commit2).
    fn get_range_diff(&self, from: &str, to: &str, three_dot: bool) -> Result<String, VcsError>;

    /// Get list of changed files for a commit or range.
    fn get_changed_files(&self, reference: &str) -> Result<Vec<String>, VcsError>;

    /// Get file content at a specific ref.
    fn get_file_content_at_ref(&self, reference: &str, path: &Path) -> Result<String, VcsError>;

    /// Get current branch name.
    fn get_current_branch(&self) -> Result<Option<String>, VcsError>;

    /// Get commit log formatted for fzf selection.
    fn get_commit_log_for_fzf(&self) -> Result<String, VcsError>;

    /// Resolve a reference to a canonical commit SHA.
    /// Works with any Git ref type accepted by git rev parsing.
    fn resolve_ref(&self, reference: &str) -> Result<String, VcsError>;

    /// Get list of files changed in working tree (staged + unstaged + untracked).
    /// Combines staged, unstaged, and untracked changes.
    fn get_working_tree_changed_files(&self) -> Result<Vec<String>, VcsError>;

    /// Get the merge base (common ancestor) of two refs.
    /// Used for triple-dot diffs (A...B).
    fn get_merge_base(&self, ref1: &str, ref2: &str) -> Result<String, VcsError>;

    /// Get list of files changed between two refs (range diff).
    fn get_range_changed_files(&self, from: &str, to: &str) -> Result<Vec<String>, VcsError>;

    /// Get the parent ref for a commit, or the empty tree SHA for root commits.
    /// This handles the edge case where a commit has no parent (first commit).
    fn get_parent_ref_or_empty(&self, reference: &str) -> Result<String, VcsError>;

    /// Get list of commits in a range for stacked diff mode.
    /// Returns commits in chronological order (oldest first).
    /// Excludes commits with no file changes (e.g., merge commits).
    ///
    /// Equivalent to `git log --reverse from..to`, filtered by diff-tree.
    fn get_commits_in_range(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<StackedCommitInfo>, VcsError>;
}
