use std::time::SystemTime;

use super::types::DiffPanelFocus;

pub mod github;
pub mod memory;
pub mod sqlite;

pub use github::GithubStore;
pub use memory::InMemoryStore;
pub use sqlite::SqliteStore;

/// Identifier for an annotation backend (e.g., "ui", "cli", "agent",
/// "github:owner/repo#123").
pub type BackendId = String;

/// Origins that live in the local SQLite store. The store accepts updates,
/// replies, and deletes for any id whose `origin` is in this set. The actual
/// origin recorded against an annotation reflects who created its root
/// comment ("ui" from the diff viewer, "cli"/"agent" from the CLI).
pub const LOCAL_ORIGINS: &[&str] = &["ui", "cli", "agent"];

/// Mirror of [`LOCAL_ORIGINS`] for the GitHub-backed store. Currently a
/// single tag; left as a slice so we can add e.g. "github-enterprise" later
/// without changing call sites.
pub const GITHUB_ORIGINS: &[&str] = &["github"];

pub fn is_local_origin(origin: &str) -> bool {
    LOCAL_ORIGINS.iter().any(|o| *o == origin)
}

pub fn is_github_origin(origin: &str) -> bool {
    GITHUB_ORIGINS.iter().any(|o| *o == origin)
}

/// Globally-unique annotation identifier composed of the source origin and
/// the backend's native ID.
///
/// `origin` reflects the source of the annotation's root comment: "ui" for
/// annotations created in the diff viewer, "cli" or "agent" for ones created
/// via the CLI. Future remote origins (e.g., "github:owner/repo#123") will
/// route to other stores.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AnnotationId {
    pub origin: BackendId,
    pub native: String,
}

/// The target of an annotation: either a whole file or a specific line range
/// on one panel.
#[derive(Clone, Debug)]
pub enum AnnotationTarget {
    File,
    LineRange {
        panel: DiffPanelFocus,
        start_line: usize,
        end_line: usize,
    },
}

/// Who wrote a comment.
#[allow(dead_code)] // `login` / `display_name` surfaced by UI once threads gain author display
#[derive(Clone, Debug)]
pub struct Author {
    pub login: String,
    pub display_name: Option<String>,
}

impl Author {
    /// Default author for a given origin string. Used when the caller hasn't
    /// supplied a richer identity (e.g., git config name, GitHub login).
    pub fn for_origin(origin: &str) -> Self {
        let login = match origin {
            "ui" => "you",
            other => other,
        };
        Self {
            login: login.to_string(),
            display_name: None,
        }
    }
}

/// A single message inside an annotation thread.
#[derive(Clone, Debug)]
pub struct Comment {
    /// Stable id of this individual comment within its store. Currently the
    /// CLI only exposes annotation-level ids, but per-comment ids are useful
    /// for replies, edits, and audit display.
    pub native_id: Option<String>,
    #[allow(dead_code)] // surfaced by UI once threads gain author display
    pub author: Author,
    /// Where this specific comment came from ("ui", "cli", "agent", ...).
    pub origin: String,
    pub content: String,
    pub created_at: SystemTime,
}

/// An annotation thread anchored at a file or line range.
///
/// `comments` is non-empty by invariant; `comments[0]` is the root (the
/// originating comment), subsequent entries are replies. Today the UI only
/// produces threads with a single comment — replies are a future story but
/// the shape is in place for GitHub-style review threads.
#[derive(Clone, Debug)]
pub struct Annotation {
    pub id: AnnotationId,
    pub filename: String,
    pub target: AnnotationTarget,
    pub comments: Vec<Comment>,
    pub resolved: bool,
}

impl Annotation {
    /// The root comment of the thread.
    pub fn root(&self) -> &Comment {
        &self.comments[0]
    }

    /// Mutable access to the root comment.
    pub fn root_mut(&mut self) -> &mut Comment {
        &mut self.comments[0]
    }

    /// When the thread was started (creation time of the root comment).
    pub fn created_at(&self) -> SystemTime {
        self.root().created_at
    }

    pub fn format_time(&self) -> String {
        use std::time::UNIX_EPOCH;
        let duration = self
            .created_at()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = duration.as_secs();
        let hours = (secs / 3600) % 24;
        let minutes = (secs / 60) % 60;
        format!("{:02}:{:02}", hours, minutes)
    }

    pub fn target_label(&self) -> &'static str {
        match &self.target {
            AnnotationTarget::File => "file",
            AnnotationTarget::LineRange { panel, .. } => match panel {
                DiffPanelFocus::Old => "old",
                DiffPanelFocus::New => "new",
                DiffPanelFocus::None => "new",
            },
        }
    }

    pub fn line_range_display(&self) -> String {
        match &self.target {
            AnnotationTarget::File => String::new(),
            AnnotationTarget::LineRange {
                start_line,
                end_line,
                ..
            } => {
                if start_line == end_line {
                    format!("L{}", start_line)
                } else {
                    format!("L{}-{}", start_line, end_line)
                }
            }
        }
    }
}

/// A draft annotation passed to `AnnotationStore::create`. The store assigns
/// the final `AnnotationId` and returns the materialized `Annotation`.
#[derive(Debug, Clone)]
pub struct NewAnnotation {
    pub filename: String,
    pub target: AnnotationTarget,
    pub content: String,
    /// Source of this annotation's root comment ("ui", "cli", "agent").
    pub origin: String,
    pub created_at: SystemTime,
}

/// A draft reply appended to an existing annotation thread.
#[derive(Debug, Clone)]
pub struct NewReply {
    pub content: String,
    pub origin: String,
    pub created_at: SystemTime,
}

/// Static description of what a store accepts.
#[allow(dead_code)] // part of the public store API; not yet read by callers
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// When true, `create` / `update` / `delete` all return `Error::ReadOnly`.
    pub read_only: bool,
    pub supports_file_target: bool,
    pub supports_line_range_target: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            read_only: false,
            supports_file_target: true,
            supports_line_range_target: true,
        }
    }
}

#[derive(Debug)]
pub enum AnnotationError {
    NotFound,
    #[allow(dead_code)]
    ReadOnly,
    #[allow(dead_code)]
    UnsupportedTarget,
    WrongOrigin,
    #[allow(dead_code)]
    Backend(String),
}

impl std::fmt::Display for AnnotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "annotation not found"),
            Self::ReadOnly => write!(f, "store is read-only"),
            Self::UnsupportedTarget => write!(f, "store does not support this annotation target"),
            Self::WrongOrigin => write!(f, "annotation id does not belong to this store"),
            Self::Backend(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for AnnotationError {}

/// Storage backend for annotations.
///
/// Sync now; designed so an async sibling trait can be added later without
/// rewriting this one:
/// - every method returns `Result`, so impls that can fail (I/O, network,
///   parse) don't change the signature shape
/// - `list` returns an owned `Vec<Annotation>` (no `&[_]` borrow into `&self`),
///   the pattern that breaks under `async-trait`
/// - inputs are taken by value where ownership transfer is natural, so a
///   future async impl can move them into a future without lifetime gymnastics
/// - no method returns a reference into the store
#[allow(dead_code)] // `id`, `capabilities`, `list` are part of the public store API
pub trait AnnotationStore {
    /// Stable identifier for this store (e.g., "local"). Annotations created
    /// here will carry this as their `id.origin`.
    fn id(&self) -> &str;

    fn capabilities(&self) -> Capabilities;

    fn list(&self) -> Result<Vec<Annotation>, AnnotationError>;

    /// List annotations whose `filename` is in `filenames`. Default impl
    /// post-filters `list()`; SQL-backed stores should override with an
    /// indexed `WHERE filename IN (...)` query.
    fn list_for_files(&self, filenames: &[String]) -> Result<Vec<Annotation>, AnnotationError> {
        use std::collections::HashSet;
        let set: HashSet<&str> = filenames.iter().map(|s| s.as_str()).collect();
        Ok(self
            .list()?
            .into_iter()
            .filter(|a| set.contains(a.filename.as_str()))
            .collect())
    }

    /// Create a new annotation thread with a single root comment. The store
    /// assigns the `AnnotationId`.
    fn create(&mut self, draft: NewAnnotation) -> Result<Annotation, AnnotationError>;

    /// Update the root comment's content. Editing replies is not yet
    /// supported through this trait.
    fn update(&mut self, id: &AnnotationId, content: String) -> Result<(), AnnotationError>;

    /// Append a reply to an existing annotation thread. Returns the
    /// refreshed annotation including the new comment.
    fn reply(&mut self, id: &AnnotationId, reply: NewReply) -> Result<Annotation, AnnotationError>;

    /// Mark an annotation thread as resolved or re-open it. Equivalent to
    /// GitHub's "Resolve conversation" / "Unresolve" toggle on a PR review.
    fn set_resolved(&mut self, id: &AnnotationId, resolved: bool) -> Result<(), AnnotationError>;

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError>;
}
