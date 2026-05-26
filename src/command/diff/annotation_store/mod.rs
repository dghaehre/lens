use std::time::SystemTime;

use super::types::DiffPanelFocus;

pub mod memory;

pub use memory::InMemoryStore;

/// Identifier for an annotation backend (e.g., "local", "github:owner/repo#123").
pub type BackendId = String;

/// Globally-unique annotation identifier composed of the backend origin and
/// the backend's native ID.
///
/// The composite shape exists so updates and deletes can be routed back to
/// the specific store that owns the annotation, and so multiple stores can
/// coexist without ID collisions.
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

/// An annotation attached to a file or line range.
#[derive(Clone, Debug)]
pub struct Annotation {
    pub id: AnnotationId,
    pub filename: String,
    pub target: AnnotationTarget,
    pub content: String,
    pub created_at: SystemTime,
}

impl Annotation {
    #[cfg(feature = "jj")]
    pub fn format_time(&self) -> String {
        use chrono::{DateTime, Local};
        let datetime: DateTime<Local> = self.created_at.into();
        datetime.format("%H:%M").to_string()
    }

    #[cfg(not(feature = "jj"))]
    pub fn format_time(&self) -> String {
        use std::time::UNIX_EPOCH;
        let duration = self.created_at.duration_since(UNIX_EPOCH).unwrap_or_default();
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
            AnnotationTarget::LineRange { start_line, end_line, .. } => {
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

    /// Create a new annotation. The store assigns the `AnnotationId`.
    fn create(&mut self, draft: NewAnnotation) -> Result<Annotation, AnnotationError>;

    fn update(&mut self, id: &AnnotationId, content: String) -> Result<(), AnnotationError>;

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError>;
}
