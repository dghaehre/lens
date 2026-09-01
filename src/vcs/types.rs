use thiserror::Error;

/// Error types for Git operations.
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

    #[error("not supported by this VCS backend: {0}")]
    Unsupported(String),

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

/// One row in the revision picker. `id` is written verbatim into
/// `diff_reference` when the row is chosen, so it must round-trip through
/// `resolve_ref`.
#[derive(Clone, Debug)]
pub struct RevisionSummary {
    /// Revset symbol identifying this revision (jj: a 12-char change-id
    /// prefix in jj's `z-k` display alphabet).
    pub id: String,
    /// Local bookmarks pointing at this revision, if any.
    pub bookmarks: Vec<String>,
    /// First line of the description; empty when there is none.
    pub summary: String,
    /// Whether this is the working-copy revision (`@`).
    pub is_working_copy: bool,
}

/// Information about a Git commit.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields populated by GitBackend; consumed by diff UI / future commands
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
