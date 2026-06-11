use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use crate::command::diff::annotation_store::{
    AnnotationStore, GithubStore, InMemoryStore, NewAnnotation, NewReply,
};
use crate::command::diff::diff_algo::{
    compute_side_by_side, count_added_removed, find_hunk_starts,
};
use crate::command::diff::highlight::FileHighlighter;
use crate::vcs::github::{resolve_pr_for_current_branch, GhClient, PrInfo, RealGhClient};
use crate::vcs::GitBackend;

use crate::command::diff::search::SearchState;
use crate::command::diff::types::{
    build_file_tree, ChangeType, CursorPosition, DiffFullscreen, DiffLine, DiffPanelFocus,
    DiffViewSettings, FileDiff, FocusedPanel, Selection, SelectionMode, SidebarItem,
};
use crate::vcs::StackedCommitInfo;

pub use crate::command::diff::annotation_store::{Annotation, AnnotationId, AnnotationTarget};

#[derive(Default, Clone, Copy, PartialEq)]
pub enum PendingKey {
    #[default]
    None,
    G,
}

/// Where the active annotation store is backed.
///
/// `Local` is the per-repo SQLite DB (created/updated locally). `Github`
/// pulls review threads from a PR via `gh`. Phase 1 keeps `Github` strictly
/// read-only — see `GithubStore::capabilities`.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationSource {
    #[default]
    Local,
    Github,
}

impl AnnotationSource {
    pub fn next(self) -> Self {
        match self {
            Self::Local => Self::Github,
            Self::Github => Self::Local,
        }
    }
}

/// A blocking operation against the GitHub `gh` CLI that has been queued by
/// the UI but not yet executed. The main loop drains this *after* the next
/// `terminal.draw()`, so the corresponding `loading_message` is visible on
/// screen for the duration of the freeze.
#[derive(Clone, Debug)]
pub enum PendingGhAction {
    /// Flip the active annotation source to `target`. May resolve the PR for
    /// the current branch on first use.
    SwitchAnnotationSource(AnnotationSource),
    /// Copy the given annotation to the *other* source than the active one.
    CopyAnnotationToOther(AnnotationId),
}

/// Input state for the "change diff reference" prompt. When `active`, the
/// footer renders an input box and key events are routed into `query`. An
/// empty query on Enter means "diff against the worktree" (no ref).
#[derive(Default, Clone)]
pub struct RefInputState {
    pub active: bool,
    pub query: String,
    pub error: Option<String>,
}

impl RefInputState {
    pub fn start(&mut self, initial: &str) {
        self.active = true;
        self.query = initial.to_string();
        self.error = None;
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.query.clear();
        self.error = None;
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.error = None;
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.error = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HunkLineSelectionMode {
    Single,
    Range,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HunkLineSelection {
    pub hunk_index: usize,
    pub panel: DiffPanelFocus,
    pub mode: HunkLineSelectionMode,
}

fn sidebar_item_path(item: &SidebarItem) -> &str {
    match item {
        SidebarItem::Directory { path, .. } => path,
        SidebarItem::File { path, .. } => path,
    }
}

fn is_child_path(path: &str, parent: &str) -> bool {
    if parent.is_empty() {
        return false;
    }
    path.starts_with(&format!("{}/", parent))
}

fn build_sidebar_visible_indices(
    items: &[SidebarItem],
    collapsed_dirs: &HashSet<String>,
) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut collapsed_stack: Vec<String> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        let path = sidebar_item_path(item);
        while let Some(last) = collapsed_stack.last() {
            if is_child_path(path, last) {
                break;
            }
            collapsed_stack.pop();
        }

        if let Some(last) = collapsed_stack.last() {
            if is_child_path(path, last) {
                continue;
            }
        }

        visible.push(idx);

        if let SidebarItem::Directory { path, .. } = item {
            if collapsed_dirs.contains(path) {
                collapsed_stack.push(path.clone());
            }
        }
    }

    visible
}

pub struct AppState {
    pub file_diffs: Vec<FileDiff>,
    pub sidebar_items: Vec<SidebarItem>,
    pub sidebar_visible: Vec<usize>,
    pub collapsed_dirs: HashSet<String>,
    pub current_file: usize,
    pub sidebar_selected: usize,
    pub sidebar_scroll: usize,
    pub sidebar_h_scroll: u16,
    pub scroll: u16,
    pub h_scroll: u16,
    pub focused_panel: FocusedPanel,
    pub viewed_files: HashSet<usize>,
    /// Hunks marked as viewed, keyed by filename (stable across reloads).
    /// Values are hunk indices within that file's `find_hunk_starts` result.
    pub viewed_hunks: HashMap<String, HashSet<usize>>,
    pub show_sidebar: bool,
    pub settings: DiffViewSettings,
    pub diff_fullscreen: DiffFullscreen,
    pub search_state: SearchState,
    pub ref_input: RefInputState,
    pub pending_key: PendingKey,
    pub needs_reload: bool,
    pub focused_hunk: Option<usize>,
    // Annotation fields
    /// Render-time read cache. Populated from `store` on writes and reloads;
    /// the render path reads this directly and must not pay a `store.list()`
    /// cost per frame.
    pub annotations: Vec<Annotation>,
    /// Persistence backend for annotations. Designed to be swappable so we
    /// can plug in file-backed / remote stores later.
    store: Box<dyn AnnotationStore>,
    /// Which backend `store` currently points at.
    pub annotation_source: AnnotationSource,
    /// The *other* store, parked while it's not in use. Only ever holds the
    /// local SqliteStore: the GitHub store is rebuilt on every switch-in so
    /// the user sees a fresh fetch, but the local store owns a SQLite
    /// connection we can't cheaply re-open.
    inactive_local_store: Option<Box<dyn AnnotationStore>>,
    /// Resolved PR for the current branch. `None` until the first switch to
    /// `Github` (or until discovery fails); subsequent switches reuse this.
    pub pr_info: Option<PrInfo>,
    /// One-shot status string for the footer (e.g., "no PR for branch",
    /// "gh not installed"). Cleared on the next successful source switch.
    pub annotation_source_status: Option<String>,
    /// Set when a blocking network call is queued or in flight. Surfaces in
    /// the footer so the user knows the upcoming UI freeze is expected. The
    /// main loop sets this just before draining `pending_gh_action` and
    /// clears it once the call returns.
    pub loading_message: Option<String>,
    /// A queued blocking `gh` call. The main loop drains this right after a
    /// draw so the corresponding `loading_message` is visible on screen
    /// while `gh` is running.
    pub pending_gh_action: Option<PendingGhAction>,
    // Stacked mode fields
    pub stacked_mode: bool,
    pub stacked_commits: Vec<StackedCommitInfo>,
    pub current_commit_index: usize,
    /// Tracks viewed files per commit SHA (commit SHA -> set of viewed filenames)
    stacked_viewed_files: HashMap<String, HashSet<String>>,
    /// The commit reference used to open the diff (e.g., "HEAD~2..HEAD", "main..feature")
    pub diff_reference: Option<String>,
    // Selection state
    /// Which panel has selection focus
    pub diff_panel_focus: DiffPanelFocus,
    /// Current text selection
    pub selection: Selection,
    /// Keyboard-driven line/range selection scoped to the focused hunk.
    pub hunk_line_selection: Option<HunkLineSelection>,
    /// When `Some`, the diff view is in annotation-select mode: `j`/`k`
    /// walk through annotations (sorted by `(file_index, start_line)`),
    /// `R` and `X` operate on the selected one, and most other keys are
    /// blocked. Cleared by Esc, or automatically if the referenced id
    /// disappears after a store reload.
    pub annotation_select: Option<AnnotationId>,
    /// Whether a mouse drag is in progress
    pub is_dragging: bool,
    /// Whether to show the selection action tooltip
    pub show_selection_tooltip: bool,
    // Cached diff computation
    /// Cached side_by_side diff for current file (invalidated on file change)
    cached_side_by_side: Option<(usize, Vec<DiffLine>)>,
    /// Cached hunk starts for current file
    cached_hunks: Option<(usize, Vec<usize>)>,
    /// Cached total line count for current file (avoids recomputing diff for max_scroll)
    cached_total_lines: Option<(usize, usize)>,
    /// Cached syntax highlighters for current file (avoids re-parsing with tree-sitter every frame)
    cached_highlighters: Option<(usize, FileHighlighter, FileHighlighter)>,
    /// Whether search matches need to be recomputed
    search_dirty: bool,
    /// Number of non-content rows at the top of the rendered diff (context lines + file annotations).
    /// Set by render_diff each frame, used by mouse handlers for coordinate mapping.
    pub content_row_offset: usize,
    /// Annotation overlay gaps within the content area. Each entry is
    /// `(content_line_after, gap_height)` where `content_line_after` is the 0-based
    /// visible content line index after which an overlay gap of `gap_height` rows appears.
    /// Used by mouse handlers to correctly map screen rows to side_by_side indices.
    pub annotation_overlay_gaps: Vec<(usize, usize)>,
    /// Total added lines across all files in the current diff. Recomputed on reload.
    pub total_added: usize,
    /// Total removed lines across all files in the current diff. Recomputed on reload.
    pub total_removed: usize,
}

fn compute_total_line_stats(file_diffs: &[FileDiff]) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for diff in file_diffs {
        if diff.is_binary {
            continue;
        }
        let (a, r) = count_added_removed(&diff.old_content, &diff.new_content);
        added += a;
        removed += r;
    }
    (added, removed)
}

impl AppState {
    /// In-memory-store constructor; kept for tests. Production code uses
    /// `with_store` to inject a persistent backend.
    #[allow(dead_code)]
    pub fn new(file_diffs: Vec<FileDiff>, focus_file: Option<&str>) -> Self {
        Self::with_store(file_diffs, focus_file, Box::new(InMemoryStore::new()))
    }

    pub fn with_store(
        file_diffs: Vec<FileDiff>,
        focus_file: Option<&str>,
        store: Box<dyn AnnotationStore>,
    ) -> Self {
        let sidebar_items = build_file_tree(&file_diffs);
        let collapsed_dirs = HashSet::new();
        let sidebar_visible = build_sidebar_visible_indices(&sidebar_items, &collapsed_dirs);
        let (sidebar_selected, current_file) = if let Some(focus_path) = focus_file {
            if let Some(file_idx) = file_diffs.iter().position(|f| f.filename == focus_path) {
                let sidebar_idx = sidebar_visible
                    .iter()
                    .position(|&idx| {
                        matches!(sidebar_items[idx], SidebarItem::File { file_index, .. } if file_index == file_idx)
                    })
                    .unwrap_or(0);
                (sidebar_idx, file_idx)
            } else {
                eprintln!(
                    "\x1b[93mwarning:\x1b[0m --focus file '{}' not found in diff, using first file",
                    focus_path
                );
                Self::find_first_file(&sidebar_items, &sidebar_visible)
            }
        } else {
            Self::find_first_file(&sidebar_items, &sidebar_visible)
        };
        let settings = DiffViewSettings::default();
        let (total_added, total_removed) = compute_total_line_stats(&file_diffs);
        let (scroll, focused_hunk) = if !file_diffs.is_empty() && current_file < file_diffs.len() {
            let diff = &file_diffs[current_file];
            let side_by_side =
                compute_side_by_side(&diff.old_content, &diff.new_content, settings.tab_width);
            let hunks = find_hunk_starts(&side_by_side);
            let scroll = hunks
                .first()
                .map(|&h| (h as u16).saturating_sub(5))
                .unwrap_or(0);
            let focused = if hunks.is_empty() { None } else { Some(0) };
            (scroll, focused)
        } else {
            (0, None)
        };

        let mut state = Self {
            file_diffs,
            sidebar_items,
            sidebar_visible,
            collapsed_dirs,
            current_file,
            sidebar_selected,
            sidebar_scroll: 0,
            sidebar_h_scroll: 0,
            scroll,
            h_scroll: 0,
            focused_panel: FocusedPanel::default(),
            viewed_files: HashSet::new(),
            viewed_hunks: HashMap::new(),
            show_sidebar: true,
            settings,
            diff_fullscreen: DiffFullscreen::default(),
            search_state: SearchState::default(),
            ref_input: RefInputState::default(),
            pending_key: PendingKey::default(),
            needs_reload: false,
            focused_hunk,
            annotations: Vec::new(),
            store,
            annotation_source: AnnotationSource::Local,
            inactive_local_store: None,
            pr_info: None,
            annotation_source_status: None,
            loading_message: None,
            pending_gh_action: None,
            stacked_mode: false,
            stacked_commits: Vec::new(),
            current_commit_index: 0,
            stacked_viewed_files: HashMap::new(),
            diff_reference: None,
            diff_panel_focus: DiffPanelFocus::default(),
            selection: Selection::default(),
            hunk_line_selection: None,
            annotation_select: None,
            is_dragging: false,
            show_selection_tooltip: false,
            cached_side_by_side: None,
            cached_hunks: None,
            cached_total_lines: None,
            cached_highlighters: None,
            search_dirty: true,
            content_row_offset: 0,
            annotation_overlay_gaps: Vec::new(),
            total_added,
            total_removed,
        };
        state.load_annotations_for_current_files();
        state
    }

    /// Refresh the annotation cache from the backing store (e.g., after
    /// an external CLI/agent write reported by the file watcher).
    pub fn reload_annotations(&mut self) {
        self.load_annotations_for_current_files();
    }

    /// Whether the active store is read-only (e.g., the Phase-1 GitHub
    /// store, or a future drift-detected GitHub store). Used by the footer
    /// to badge "(read-only)" without exposing the store internals.
    pub fn active_store_is_read_only(&self) -> bool {
        self.store.capabilities().read_only
    }

    /// Switch the active annotation store. On success the cache is reloaded
    /// from the new backend. On failure the state is left untouched and the
    /// error is returned (also stashed in `annotation_source_status` so the
    /// footer can surface it).
    ///
    /// Switching INTO GitHub always builds a fresh `GithubStore` (one
    /// network round-trip) so the user sees current PR threads. Switching
    /// back to Local restores the parked SQLite store; the GitHub store is
    /// dropped.
    ///
    /// `backend` is used to resolve the local `HEAD` so we can detect
    /// "checkout drift" (local HEAD != PR head SHA). When the SHAs differ,
    /// the GitHub store is built read-only because line numbers wouldn't
    /// line up reliably for writes.
    pub fn switch_annotation_source(
        &mut self,
        target: AnnotationSource,
        backend: &GitBackend,
    ) -> Result<(), String> {
        let local_head = backend.resolve_ref("HEAD").ok();
        self.switch_annotation_source_inner(target, local_head, Box::new(RealGhClient))
    }

    /// Inner form of [`switch_annotation_source`] with the local HEAD and
    /// `gh` runner injected directly. Tests call this to stub out both;
    /// production code goes through the wrapper above.
    pub fn switch_annotation_source_inner(
        &mut self,
        target: AnnotationSource,
        local_head: Option<String>,
        gh: Box<dyn GhClient>,
    ) -> Result<(), String> {
        if target == self.annotation_source {
            return Ok(());
        }

        match target {
            AnnotationSource::Github => {
                // Resolve once and cache; subsequent switches reuse PrInfo
                // but always rebuild the store for a fresh fetch.
                if self.pr_info.is_none() {
                    match resolve_pr_for_current_branch(&*gh) {
                        Ok(pr) => self.pr_info = Some(pr),
                        Err(e) => {
                            let msg = e.to_string();
                            self.annotation_source_status = Some(msg.clone());
                            return Err(msg);
                        }
                    }
                }
                let pr = self
                    .pr_info
                    .clone()
                    .expect("pr_info populated above");
                // Writable only if the local checkout matches the PR head;
                // otherwise we'd be posting comments anchored to lines
                // that don't exist on the remote (GitHub rejects with 422).
                let writable = match local_head.as_deref() {
                    Some(h) => h == pr.head_oid.as_str(),
                    None => false,
                };
                match GithubStore::new(pr, gh, writable) {
                    Ok(gh_store) => {
                        // Park the local store so we can restore it on
                        // switch-back without re-opening sqlite.
                        let prev = std::mem::replace(
                            &mut self.store,
                            Box::new(InMemoryStore::new()),
                        );
                        self.inactive_local_store = Some(prev);
                        self.store = Box::new(gh_store);
                        self.annotation_source = AnnotationSource::Github;
                        self.annotation_source_status = if writable {
                            None
                        } else {
                            Some(
                                "checkout drift: local HEAD != PR head — read-only"
                                    .to_string(),
                            )
                        };
                        self.load_annotations_for_current_files();
                        Ok(())
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        self.annotation_source_status = Some(msg.clone());
                        Err(msg)
                    }
                }
            }
            AnnotationSource::Local => {
                let restored = self.inactive_local_store.take().ok_or_else(|| {
                    "internal: no local store to restore".to_string()
                })?;
                // Drop the github store; it's stateless and rebuilds cheaply.
                self.store = restored;
                self.annotation_source = AnnotationSource::Local;
                self.annotation_source_status = None;
                self.load_annotations_for_current_files();
                Ok(())
            }
        }
    }

    /// Copy the selected annotation's root comment to the *other* annotation
    /// source. Local → Github builds a transient `GithubStore` (requires a PR
    /// and a non-drifted checkout). Github → Local writes to the parked
    /// SQLite store. The active source is left unchanged so the user can
    /// keep reviewing where they were.
    pub fn copy_annotation_to_other_source(
        &mut self,
        id: &AnnotationId,
        backend: &GitBackend,
    ) -> Result<(), String> {
        let local_head = backend.resolve_ref("HEAD").ok();
        self.copy_annotation_to_other_source_inner(id, local_head, Box::new(RealGhClient))
    }

    pub fn copy_annotation_to_other_source_inner(
        &mut self,
        id: &AnnotationId,
        local_head: Option<String>,
        gh: Box<dyn GhClient>,
    ) -> Result<(), String> {
        let ann = self
            .get_annotation_by_id(id)
            .cloned()
            .ok_or_else(|| "annotation not found".to_string())?;

        match self.annotation_source {
            AnnotationSource::Local => {
                if self.pr_info.is_none() {
                    match resolve_pr_for_current_branch(&*gh) {
                        Ok(pr) => self.pr_info = Some(pr),
                        Err(e) => {
                            let msg = e.to_string();
                            self.annotation_source_status = Some(msg.clone());
                            return Err(msg);
                        }
                    }
                }
                let pr = self.pr_info.clone().expect("pr_info populated above");
                let writable = match local_head.as_deref() {
                    Some(h) => h == pr.head_oid.as_str(),
                    None => false,
                };
                if !writable {
                    let msg = "checkout drift: local HEAD != PR head — cannot push to github"
                        .to_string();
                    self.annotation_source_status = Some(msg.clone());
                    return Err(msg);
                }
                let mut gh_store = GithubStore::new(pr, gh, true).map_err(|e| {
                    let msg = e.to_string();
                    self.annotation_source_status = Some(msg.clone());
                    msg
                })?;
                let draft = NewAnnotation {
                    filename: ann.filename.clone(),
                    target: ann.target.clone(),
                    content: ann.root().content.clone(),
                    origin: "github".to_string(),
                    created_at: SystemTime::now(),
                };
                gh_store.create(draft).map_err(|e| {
                    let msg = e.to_string();
                    self.annotation_source_status = Some(msg.clone());
                    msg
                })?;
                self.annotation_source_status = Some("copied to github".to_string());
                Ok(())
            }
            AnnotationSource::Github => {
                let local_store = self.inactive_local_store.as_mut().ok_or_else(|| {
                    "internal: no local store available".to_string()
                })?;
                let draft = NewAnnotation {
                    filename: ann.filename.clone(),
                    target: ann.target.clone(),
                    content: ann.root().content.clone(),
                    origin: "ui".to_string(),
                    created_at: SystemTime::now(),
                };
                local_store.create(draft).map_err(|e| {
                    let msg = e.to_string();
                    self.annotation_source_status = Some(msg.clone());
                    msg
                })?;
                self.annotation_source_status = Some("copied to local".to_string());
                Ok(())
            }
        }
    }

    /// Repopulate `self.annotations` from the backing store, filtered to files
    /// present in the current diff. Errors from the store are swallowed —
    /// the cache simply stays empty if the store can't be read.
    fn load_annotations_for_current_files(&mut self) {
        let filenames: Vec<String> = self
            .file_diffs
            .iter()
            .map(|f| f.filename.clone())
            .collect();
        match self.store.list_for_files(&filenames) {
            Ok(list) => self.annotations = list,
            Err(_) => self.annotations.clear(),
        }
        if let Some(id) = &self.annotation_select {
            if !self.annotations.iter().any(|a| &a.id == id) {
                self.annotation_select = None;
            }
        }
    }

    fn find_first_file(sidebar_items: &[SidebarItem], sidebar_visible: &[usize]) -> (usize, usize) {
        for (visible_idx, &item_idx) in sidebar_visible.iter().enumerate() {
            if let SidebarItem::File { file_index, .. } = &sidebar_items[item_idx] {
                return (visible_idx, *file_index);
            }
        }
        (0, 0)
    }

    /// Get cached side_by_side diff for current file, computing if necessary
    pub fn get_side_by_side(&mut self) -> &[DiffLine] {
        if self.file_diffs.is_empty() {
            return &[];
        }

        let current = self.current_file;
        let needs_recompute = match &self.cached_side_by_side {
            Some((cached_file, _)) => *cached_file != current,
            None => true,
        };

        if needs_recompute {
            let diff = &self.file_diffs[current];
            let side_by_side = compute_side_by_side(
                &diff.old_content,
                &diff.new_content,
                self.settings.tab_width,
            );
            let hunks = find_hunk_starts(&side_by_side);
            let total = side_by_side.len();
            self.cached_side_by_side = Some((current, side_by_side));
            self.cached_hunks = Some((current, hunks));
            self.cached_total_lines = Some((current, total));
        }

        &self.cached_side_by_side.as_ref().unwrap().1
    }

    /// Get cached hunk starts for current file
    pub fn get_hunks(&mut self) -> &[usize] {
        // Ensure side_by_side is computed (which also computes hunks)
        let _ = self.get_side_by_side();
        &self.cached_hunks.as_ref().unwrap().1
    }

    /// Ensure the side_by_side and hunk caches are populated for the current file.
    /// Call this before using `side_by_side_ref()` or `hunks_ref()`.
    pub fn ensure_cache(&mut self) {
        if self.file_diffs.is_empty() {
            return;
        }
        let current = self.current_file;
        let needs_recompute = match &self.cached_side_by_side {
            Some((cached_file, _)) => *cached_file != current,
            None => true,
        };
        if needs_recompute {
            let diff = &self.file_diffs[current];
            let sbs = compute_side_by_side(
                &diff.old_content,
                &diff.new_content,
                self.settings.tab_width,
            );
            let hnks = find_hunk_starts(&sbs);
            let total = sbs.len();
            self.cached_side_by_side = Some((current, sbs));
            self.cached_hunks = Some((current, hnks));
            self.cached_total_lines = Some((current, total));
        }
    }

    /// Get an immutable reference to the cached side_by_side data.
    /// Must call `ensure_cache()` first.
    pub fn side_by_side_ref(&self) -> &[DiffLine] {
        match &self.cached_side_by_side {
            Some((_, ref data)) => data,
            None => &[],
        }
    }

    /// Get an immutable reference to the cached hunk starts.
    /// Must call `ensure_cache()` first.
    pub fn hunks_ref(&self) -> &[usize] {
        match &self.cached_hunks {
            Some((_, ref data)) => data,
            None => &[],
        }
    }

    /// Ensure cache is populated and update search matches.
    /// Combines the mutable cache population with search update to avoid borrow conflicts.
    pub fn update_search_matches(&mut self) {
        self.ensure_cache();
        if self.search_dirty {
            let sbs = match &self.cached_side_by_side {
                Some((_, data)) => data.as_slice(),
                None => &[],
            };
            self.search_state.update_matches(sbs, self.diff_fullscreen);
            self.search_dirty = false;
        }
    }

    /// Mark search matches as needing recomputation
    pub fn mark_search_dirty(&mut self) {
        self.search_dirty = true;
    }

    /// Get the cached total line count for the current file.
    /// Ensures cache is populated first.
    pub fn total_lines(&mut self) -> usize {
        self.ensure_cache();
        self.cached_total_lines
            .as_ref()
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// Get cached FileHighlighters for the current file, creating them if needed.
    /// Returns (old_highlighter, new_highlighter).
    pub fn get_highlighters(&mut self) -> (&FileHighlighter, &FileHighlighter) {
        let current = self.current_file;
        let needs_recompute = match &self.cached_highlighters {
            Some((cached_file, _, _)) => *cached_file != current,
            None => true,
        };
        if needs_recompute {
            let diff = &self.file_diffs[current];
            let old_hl = FileHighlighter::new(&diff.old_content, &diff.filename);
            let new_hl = FileHighlighter::new(&diff.new_content, &diff.filename);
            self.cached_highlighters = Some((current, old_hl, new_hl));
        }
        let (_, old_hl, new_hl) = self.cached_highlighters.as_ref().unwrap();
        (old_hl, new_hl)
    }

    /// Get cached highlighters without triggering recompute.
    /// Must call get_highlighters() at least once first for current file.
    pub fn highlighters_ref(&self) -> Option<(&FileHighlighter, &FileHighlighter)> {
        self.cached_highlighters
            .as_ref()
            .map(|(_, old_hl, new_hl)| (old_hl, new_hl))
    }

    /// Invalidate the cache (call when file changes)
    pub fn invalidate_cache(&mut self) {
        self.cached_side_by_side = None;
        self.cached_hunks = None;
        self.cached_total_lines = None;
        self.cached_highlighters = None;
        self.search_dirty = true;
        self.content_row_offset = 0;
        self.annotation_overlay_gaps.clear();
    }

    /// Adjust a screen-relative content row for annotation overlay gaps.
    /// Returns `Some(adjusted_content_y)` for content rows, or `None` if the
    /// click landed inside an annotation overlay.
    pub fn adjust_for_overlay_gaps(&self, content_y: usize) -> Option<usize> {
        let mut cumulative = 0;
        for &(after_line, gap_height) in &self.annotation_overlay_gaps {
            let gap_screen_start = after_line + 1 + cumulative;
            let gap_screen_end = gap_screen_start + gap_height;
            if content_y < gap_screen_start {
                break; // Before this gap
            }
            if content_y < gap_screen_end {
                return None; // Inside an overlay gap
            }
            cumulative += gap_height;
        }
        Some(content_y - cumulative)
    }

    /// Like `adjust_for_overlay_gaps`, but if the position is inside a gap,
    /// returns the content line just before the gap instead of None.
    /// Used for drag operations where we always need a valid line.
    pub fn adjust_for_overlay_gaps_clamped(&self, content_y: usize) -> usize {
        let mut cumulative = 0;
        for &(after_line, gap_height) in &self.annotation_overlay_gaps {
            let gap_screen_start = after_line + 1 + cumulative;
            let gap_screen_end = gap_screen_start + gap_height;
            if content_y < gap_screen_start {
                break;
            }
            if content_y < gap_screen_end {
                return after_line; // Map to the content line just before the gap
            }
            cumulative += gap_height;
        }
        content_y - cumulative
    }

    /// Clear all selection state
    pub fn clear_selection(&mut self) {
        self.diff_panel_focus = DiffPanelFocus::None;
        self.selection = Selection::default();
        self.hunk_line_selection = None;
        self.is_dragging = false;
        self.show_selection_tooltip = false;
    }

    /// Start a new selection
    pub fn start_selection(
        &mut self,
        panel: DiffPanelFocus,
        pos: CursorPosition,
        mode: SelectionMode,
    ) {
        self.hunk_line_selection = None;
        self.diff_panel_focus = panel;
        self.selection = Selection {
            panel,
            anchor: pos,
            head: pos,
            mode,
        };
        self.is_dragging = true;
    }

    /// Extend the current selection to a new position
    pub fn extend_selection(&mut self, pos: CursorPosition) {
        if self.is_dragging {
            self.selection.head = pos;
        }
    }

    fn selectable_hunk_lines(&mut self, hunk_index: usize, panel: DiffPanelFocus) -> Vec<usize> {
        self.ensure_cache();
        let sbs = self.side_by_side_ref();
        let hunks = self.hunks_ref();
        let Some(&hunk_start) = hunks.get(hunk_index) else {
            return Vec::new();
        };
        let next_hunk_start = hunks.get(hunk_index + 1).copied().unwrap_or(sbs.len());

        (hunk_start..next_hunk_start)
            .filter(|&idx| {
                sbs.get(idx).map_or(false, |line| {
                    !matches!(line.change_type, ChangeType::Equal)
                        && line.line_number(panel).is_some()
                })
            })
            .collect()
    }

    pub fn start_hunk_line_selection(
        &mut self,
        hunk_index: usize,
        panel: DiffPanelFocus,
    ) -> Option<usize> {
        let line_idx = self
            .selectable_hunk_lines(hunk_index, panel)
            .into_iter()
            .next()?;
        let pos = CursorPosition {
            line: line_idx,
            column: 0,
        };

        self.diff_panel_focus = panel;
        self.selection = Selection {
            panel,
            anchor: pos,
            head: pos,
            mode: SelectionMode::Line,
        };
        self.hunk_line_selection = Some(HunkLineSelection {
            hunk_index,
            panel,
            mode: HunkLineSelectionMode::Single,
        });
        self.is_dragging = false;
        self.show_selection_tooltip = false;

        Some(line_idx)
    }

    pub fn begin_hunk_line_range(&mut self) -> Option<usize> {
        let active = self.hunk_line_selection.as_mut()?;
        active.mode = HunkLineSelectionMode::Range;
        self.selection.panel = active.panel;
        self.selection.anchor = self.selection.head;
        self.selection.mode = SelectionMode::Line;
        self.diff_panel_focus = active.panel;
        self.is_dragging = false;
        self.show_selection_tooltip = false;

        Some(self.selection.head.line)
    }

    pub fn move_hunk_line_selection(&mut self, direction: isize) -> Option<usize> {
        let active = self.hunk_line_selection?;
        let lines = self.selectable_hunk_lines(active.hunk_index, active.panel);
        if lines.is_empty() {
            return None;
        }

        let current = self.selection.head.line;
        let current_pos = lines
            .iter()
            .position(|&line| line == current)
            .unwrap_or_else(|| {
                let next_after_current = lines
                    .iter()
                    .position(|&line| line > current)
                    .unwrap_or(lines.len());
                if direction < 0 {
                    next_after_current.saturating_sub(1)
                } else {
                    next_after_current.min(lines.len().saturating_sub(1))
                }
            });
        let next_pos = if direction < 0 {
            current_pos.saturating_sub(1)
        } else {
            (current_pos + 1).min(lines.len().saturating_sub(1))
        };
        let next_line = lines[next_pos];
        let pos = CursorPosition {
            line: next_line,
            column: 0,
        };

        self.selection.panel = active.panel;
        self.selection.head = pos;
        self.selection.mode = SelectionMode::Line;
        self.diff_panel_focus = active.panel;
        if matches!(active.mode, HunkLineSelectionMode::Single) {
            self.selection.anchor = pos;
        }
        self.is_dragging = false;
        self.show_selection_tooltip = false;

        Some(next_line)
    }

    /// End the drag operation but keep the selection
    pub fn end_drag(&mut self) {
        self.is_dragging = false;
        // Show tooltip only if there's an actual selection (not just a click)
        if self.selection.is_active() && self.selection.anchor != self.selection.head {
            self.show_selection_tooltip = true;
        }
    }

    pub fn sidebar_visible_len(&self) -> usize {
        self.sidebar_visible.len()
    }

    pub fn sidebar_item_at_visible(&self, visible_index: usize) -> Option<&SidebarItem> {
        self.sidebar_visible
            .get(visible_index)
            .and_then(|idx| self.sidebar_items.get(*idx))
    }

    pub fn sidebar_visible_index_for_file(&self, file_index: usize) -> Option<usize> {
        self.sidebar_visible.iter().position(|idx| {
            matches!(self.sidebar_items[*idx], SidebarItem::File { file_index: fi, .. } if fi == file_index)
        })
    }

    pub fn sidebar_visible_index_for_dir(&self, dir_path: &str) -> Option<usize> {
        self.sidebar_visible.iter().position(|idx| {
            matches!(&self.sidebar_items[*idx], SidebarItem::Directory { path, .. } if path == dir_path)
        })
    }

    pub fn rebuild_sidebar_visible(&mut self) {
        let existing_dirs: HashSet<String> = self
            .sidebar_items
            .iter()
            .filter_map(|item| match item {
                SidebarItem::Directory { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        self.collapsed_dirs
            .retain(|path| existing_dirs.contains(path));
        self.sidebar_visible =
            build_sidebar_visible_indices(&self.sidebar_items, &self.collapsed_dirs);

        if self.sidebar_visible.is_empty() {
            self.sidebar_selected = 0;
            self.sidebar_scroll = 0;
            return;
        }

        if let Some(idx) = self.sidebar_visible_index_for_file(self.current_file) {
            self.sidebar_selected = idx;
        } else if self.sidebar_selected >= self.sidebar_visible.len() {
            self.sidebar_selected = self.sidebar_visible.len() - 1;
        }

        if self.sidebar_scroll >= self.sidebar_visible.len() {
            self.sidebar_scroll = self.sidebar_visible.len() - 1;
        }
    }

    pub fn toggle_directory(&mut self, dir_path: &str) {
        let selected_item = self.sidebar_item_at_visible(self.sidebar_selected).cloned();
        let collapsing = !self.collapsed_dirs.contains(dir_path);

        if collapsing {
            self.collapsed_dirs.insert(dir_path.to_string());
        } else {
            self.collapsed_dirs.remove(dir_path);
        }

        self.rebuild_sidebar_visible();

        if collapsing {
            if let Some(item) = &selected_item {
                let path = sidebar_item_path(item);
                if is_child_path(path, dir_path) {
                    if let Some(idx) = self.sidebar_visible_index_for_dir(dir_path) {
                        self.sidebar_selected = idx;
                        return;
                    }
                }
            }
        }

        if let Some(item) = selected_item {
            match item {
                SidebarItem::Directory { path, .. } => {
                    if let Some(idx) = self.sidebar_visible_index_for_dir(&path) {
                        self.sidebar_selected = idx;
                    }
                }
                SidebarItem::File { file_index, .. } => {
                    if let Some(idx) = self.sidebar_visible_index_for_file(file_index) {
                        self.sidebar_selected = idx;
                    }
                }
            }
        }
    }

    pub fn reveal_file(&mut self, file_index: usize) {
        if file_index >= self.file_diffs.len() {
            return;
        }
        let path = self.file_diffs[file_index].filename.clone();
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() > 1 {
            for i in 0..parts.len() - 1 {
                let dir_path = parts[..=i].join("/");
                self.collapsed_dirs.remove(&dir_path);
            }
        }
        self.rebuild_sidebar_visible();
        if let Some(idx) = self.sidebar_visible_index_for_file(file_index) {
            self.sidebar_selected = idx;
        }
    }

    /// Set the diff reference string (e.g., "HEAD~2..HEAD")
    pub fn set_diff_reference(&mut self, reference: Option<String>) {
        self.diff_reference = reference;
    }

    /// Initialize stacked mode with commits
    pub fn init_stacked_mode(&mut self, commits: Vec<StackedCommitInfo>) {
        self.stacked_mode = true;
        self.stacked_commits = commits;
        self.current_commit_index = 0;
    }

    /// Get the current commit info if in stacked mode
    pub fn current_commit(&self) -> Option<&StackedCommitInfo> {
        if self.stacked_mode {
            self.stacked_commits.get(self.current_commit_index)
        } else {
            None
        }
    }

    /// Save current viewed files for the current commit (stacked mode only)
    pub fn save_stacked_viewed_files(&mut self) {
        if !self.stacked_mode {
            return;
        }
        if let Some(commit) = self.stacked_commits.get(self.current_commit_index) {
            let viewed_filenames: HashSet<String> = self
                .viewed_files
                .iter()
                .filter_map(|&idx| self.file_diffs.get(idx).map(|f| f.filename.clone()))
                .collect();
            self.stacked_viewed_files
                .insert(commit.commit_id.clone(), viewed_filenames);
        }
    }

    /// Load viewed files for the current commit (stacked mode only)
    pub fn load_stacked_viewed_files(&mut self) {
        if !self.stacked_mode {
            return;
        }
        if let Some(commit) = self.stacked_commits.get(self.current_commit_index) {
            if let Some(viewed_filenames) = self.stacked_viewed_files.get(&commit.commit_id) {
                self.viewed_files = self
                    .file_diffs
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| viewed_filenames.contains(&f.filename))
                    .map(|(i, _)| i)
                    .collect();
            } else {
                self.viewed_files.clear();
            }
        }
    }

    /// Reload file diffs, optionally unmarking changed files from viewed set.
    /// Preserves scroll position and current file when possible.
    pub fn reload(&mut self, file_diffs: Vec<FileDiff>, changed_files: Option<&HashSet<String>>) {
        // Store current state to preserve
        let old_filename = self
            .file_diffs
            .get(self.current_file)
            .map(|f| f.filename.clone());
        let old_scroll = self.scroll;
        let old_h_scroll = self.h_scroll;

        // Convert viewed_files indices to filenames (to handle index changes after reload)
        let mut viewed_filenames: HashSet<String> = self
            .viewed_files
            .iter()
            .filter_map(|&idx| self.file_diffs.get(idx).map(|f| f.filename.clone()))
            .collect();

        // Remove changed files from viewed set
        if let Some(changed) = changed_files {
            for filename in changed {
                viewed_filenames.remove(filename);
                self.viewed_hunks.remove(filename);
            }
        }

        self.file_diffs = file_diffs;
        let (total_added, total_removed) = compute_total_line_stats(&self.file_diffs);
        self.total_added = total_added;
        self.total_removed = total_removed;
        self.sidebar_items = build_file_tree(&self.file_diffs);

        // Repopulate annotations from the store for the new file set.
        // Annotations for files no longer in the diff remain in persistent
        // storage but are dropped from the cache.
        self.load_annotations_for_current_files();
        let filenames: HashSet<&str> = self
            .file_diffs
            .iter()
            .map(|f| f.filename.as_str())
            .collect();
        self.viewed_hunks
            .retain(|fname, _| filenames.contains(fname.as_str()));

        // Convert viewed filenames back to indices in the new file_diffs
        self.viewed_files = self
            .file_diffs
            .iter()
            .enumerate()
            .filter(|(_, f)| viewed_filenames.contains(&f.filename))
            .map(|(i, _)| i)
            .collect();

        // Preserve current file selection
        if let Some(name) = old_filename {
            self.current_file = self
                .file_diffs
                .iter()
                .position(|f| f.filename == name)
                .unwrap_or(0);
        }
        if self.current_file >= self.file_diffs.len() && !self.file_diffs.is_empty() {
            self.current_file = self.file_diffs.len() - 1;
        }

        self.rebuild_sidebar_visible();

        self.needs_reload = false;
        self.invalidate_cache(); // Clear cache after reload

        // Preserve scroll position instead of resetting
        if !self.file_diffs.is_empty() {
            // Keep the old scroll position, but clamp to valid range
            let total = self.total_lines();
            let max_scroll = total.saturating_sub(10);
            self.scroll = old_scroll.min(max_scroll as u16);
            self.h_scroll = old_h_scroll;
        }
    }

    pub fn select_file(&mut self, file_index: usize) {
        self.current_file = file_index;
        self.clear_selection(); // Clear selection when changing files
        self.invalidate_cache(); // Clear cache for new file

        // Use cached computation
        let hunks = self.get_hunks().to_vec();
        self.scroll = hunks
            .first()
            .map(|&h| (h as u16).saturating_sub(5))
            .unwrap_or(0);
        self.h_scroll = 0;
        self.focused_hunk = if hunks.is_empty() { None } else { Some(0) };
    }

    /// Get annotation by id (reads from cache; fine in render path)
    pub fn get_annotation_by_id(&self, id: &AnnotationId) -> Option<&Annotation> {
        self.annotations.iter().find(|a| &a.id == id)
    }

    /// Get all annotations for a file (reads from cache)
    #[allow(dead_code)]
    pub fn get_annotations_for_file(&self, filename: &str) -> Vec<&Annotation> {
        self.annotations
            .iter()
            .filter(|a| a.filename == filename)
            .collect()
    }

    /// Whether this annotation's anchor still exists in the current diff.
    ///
    /// File-targeted annotations are orphaned only if the file is no longer
    /// in the diff. Line-range annotations are orphaned if the file is gone
    /// or if the saved line range falls outside the relevant panel's content.
    pub fn is_annotation_orphaned(&self, ann: &Annotation) -> bool {
        let Some(file_diff) = self
            .file_diffs
            .iter()
            .find(|f| f.filename == ann.filename)
        else {
            return true;
        };
        match &ann.target {
            AnnotationTarget::File => false,
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => {
                let content = match panel {
                    DiffPanelFocus::Old => &file_diff.old_content,
                    _ => &file_diff.new_content,
                };
                let line_count = content.lines().count();
                *start_line == 0 || *end_line == 0 || *end_line > line_count
            }
        }
    }

    /// Add a new annotation. Returns the assigned `AnnotationId` on success,
    /// `None` if the underlying store rejected the write.
    pub fn add_annotation(
        &mut self,
        filename: String,
        target: AnnotationTarget,
        content: String,
        created_at: SystemTime,
    ) -> Option<AnnotationId> {
        let draft = NewAnnotation {
            filename,
            target,
            content,
            origin: "ui".to_string(),
            created_at,
        };
        match self.store.create(draft) {
            Ok(annotation) => {
                let id = annotation.id.clone();
                self.annotations.push(annotation);
                Some(id)
            }
            Err(_) => None,
        }
    }

    /// Update an existing annotation's content. Errors from the store are
    /// swallowed; the cache is only updated if the store accepted the write.
    pub fn update_annotation(&mut self, id: &AnnotationId, content: String) {
        if self.store.update(id, content.clone()).is_ok() {
            if let Some(ann) = self.annotations.iter_mut().find(|a| &a.id == id) {
                ann.root_mut().content = content;
            }
        }
    }

    /// Append a reply to an existing annotation. On success the cached
    /// annotation is replaced with the refreshed thread; on failure (e.g.,
    /// the annotation was deleted underneath us), the cache is left intact.
    pub fn add_reply(&mut self, id: &AnnotationId, content: String) {
        let draft = NewReply {
            content,
            origin: "ui".to_string(),
            created_at: SystemTime::now(),
        };
        if let Ok(updated) = self.store.reply(id, draft) {
            if let Some(ann) = self.annotations.iter_mut().find(|a| &a.id == id) {
                *ann = updated;
            } else {
                self.annotations.push(updated);
            }
        }
    }

    /// Toggle the resolved state of an annotation. Errors from the store are
    /// swallowed; the cache is only updated if the store accepted the write.
    pub fn set_annotation_resolved(&mut self, id: &AnnotationId, resolved: bool) {
        if self.store.set_resolved(id, resolved).is_ok() {
            if let Some(ann) = self.annotations.iter_mut().find(|a| &a.id == id) {
                ann.resolved = resolved;
            }
        }
    }

    /// Remove an annotation by id. Errors from the store are swallowed.
    pub fn remove_annotation(&mut self, id: &AnnotationId) {
        if self.store.delete(id).is_ok() {
            self.annotations.retain(|a| &a.id != id);
        }
    }

    /// Format all annotations for export (GitHub PR review comment style).
    ///
    /// Uses `path`, `line`/`start_line`, and `side` references instead of
    /// embedding full source code — matching the shape of the GitHub Pull
    /// Request review comment API.
    pub fn format_annotations_for_export(&self) -> String {
        let mut result = String::new();

        if let Some(ref reference) = self.diff_reference {
            result.push_str(&format!("# {}\n\n", reference));
        }

        for (i, ann) in self.annotations.iter().enumerate() {
            if i > 0 {
                result.push_str("---\n\n");
            }

            match &ann.target {
                AnnotationTarget::File => {
                    result.push_str(&format!("**{}**\n\n", ann.filename));
                }
                AnnotationTarget::LineRange {
                    panel,
                    start_line,
                    end_line,
                    ..
                } => {
                    let side = match panel {
                        DiffPanelFocus::Old => "LEFT",
                        _ => "RIGHT",
                    };
                    if start_line == end_line {
                        result.push_str(&format!(
                            "**{}** line {} ({})\n\n",
                            ann.filename, start_line, side,
                        ));
                    } else {
                        result.push_str(&format!(
                            "**{}** lines {}-{} ({})\n\n",
                            ann.filename, start_line, end_line, side,
                        ));
                    }
                }
            }

            // Comment body (root comment only for now; replies will be
            // appended once the UI supports them).
            result.push_str(&ann.root().content);
            result.push_str("\n\n");
        }

        result.trim_end().to_string()
    }
}
pub fn adjust_scroll_to_line(
    line: usize,
    scroll: u16,
    visible_height: usize,
    max_scroll: usize,
) -> u16 {
    let margin = 10usize;
    let scroll_usize = scroll as usize;
    let content_height = visible_height.saturating_sub(2);

    let new_scroll = if line < scroll_usize + margin {
        line.saturating_sub(margin) as u16
    } else if line >= scroll_usize + content_height.saturating_sub(margin) {
        (line.saturating_sub(content_height.saturating_sub(margin).saturating_sub(1))) as u16
    } else {
        scroll
    };
    new_scroll.min(max_scroll as u16)
}

/// Adjust scroll for hunk focus - only scrolls if the hunk line is outside the viewport.
/// Uses a larger bottom margin to keep hunks visible with context below.
pub fn adjust_scroll_for_hunk(
    hunk_line: usize,
    scroll: u16,
    visible_height: usize,
    max_scroll: usize,
) -> u16 {
    let top_margin = 5usize;
    let bottom_margin = 25usize;
    let scroll_usize = scroll as usize;
    let content_height = visible_height.saturating_sub(2);

    // Check if hunk is above the viewport (with top margin)
    if hunk_line < scroll_usize + top_margin {
        return (hunk_line.saturating_sub(top_margin) as u16).min(max_scroll as u16);
    }

    // Check if hunk is below the viewport (with bottom margin)
    if hunk_line >= scroll_usize + content_height.saturating_sub(bottom_margin) {
        return (hunk_line.saturating_sub(
            content_height
                .saturating_sub(bottom_margin)
                .saturating_sub(1),
        ) as u16)
            .min(max_scroll as u16);
    }

    // Hunk is within viewport, don't scroll
    scroll
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::diff::types::FileStatus;

    fn make_file_diff(filename: &str) -> FileDiff {
        FileDiff {
            filename: filename.to_string(),
            old_content: String::new(),
            new_content: "content\n".to_string(),
            status: FileStatus::Added,
            is_binary: false,
        }
    }

    fn make_modified_file_diff(filename: &str, old_content: &str, new_content: &str) -> FileDiff {
        FileDiff {
            filename: filename.to_string(),
            old_content: old_content.to_string(),
            new_content: new_content.to_string(),
            status: FileStatus::Modified,
            is_binary: false,
        }
    }

    #[test]
    fn test_hunk_line_selection_moves_single_line() {
        let diffs = vec![make_modified_file_diff(
            "src/lib.rs",
            "one\ntwo\nthree\nfour\n",
            "one\nTWO\nTHREE\nfour\n",
        )];
        let mut state = AppState::new(diffs, None);

        let first_line = state
            .start_hunk_line_selection(0, DiffPanelFocus::New)
            .expect("hunk line selection should start");
        assert_eq!(first_line, 1);
        assert_eq!(state.selection.panel, DiffPanelFocus::New);
        assert_eq!(state.selection.anchor.line, first_line);
        assert_eq!(state.selection.head.line, first_line);

        let second_line = state
            .move_hunk_line_selection(1)
            .expect("hunk line selection should move");
        assert_eq!(second_line, 2);
        assert_eq!(state.selection.anchor.line, second_line);
        assert_eq!(state.selection.head.line, second_line);
    }

    #[test]
    fn test_hunk_line_selection_space_starts_range() {
        let diffs = vec![make_modified_file_diff(
            "src/lib.rs",
            "one\ntwo\nthree\nfour\n",
            "one\nTWO\nTHREE\nfour\n",
        )];
        let mut state = AppState::new(diffs, None);

        state
            .start_hunk_line_selection(0, DiffPanelFocus::New)
            .expect("hunk line selection should start");
        state
            .move_hunk_line_selection(1)
            .expect("hunk line selection should move");
        state.begin_hunk_line_range();
        state
            .move_hunk_line_selection(-1)
            .expect("hunk line selection should move");

        let (start, end) = state.selection.normalized_range();
        assert_eq!(start.line, 1);
        assert_eq!(end.line, 2);
        assert!(matches!(
            state.hunk_line_selection.map(|s| s.mode),
            Some(HunkLineSelectionMode::Range)
        ));
    }

    #[test]
    fn test_focus_selects_matching_file() {
        let diffs = vec![
            make_file_diff("src/main.rs"),
            make_file_diff("src/lib.rs"),
            make_file_diff("README.md"),
        ];

        let state = AppState::new(diffs, Some("src/lib.rs"));

        assert_eq!(state.file_diffs[state.current_file].filename, "src/lib.rs");
    }

    #[test]
    fn test_focus_none_selects_first_file_in_sidebar() {
        let diffs = vec![make_file_diff("bbb.rs"), make_file_diff("aaa.rs")];

        let state = AppState::new(diffs, None);

        // Sidebar sorts alphabetically, so aaa.rs (index 1) appears first
        assert_eq!(state.file_diffs[state.current_file].filename, "aaa.rs");
    }

    #[test]
    fn test_focus_not_found_falls_back_to_first_in_sidebar() {
        let diffs = vec![make_file_diff("bbb.rs"), make_file_diff("aaa.rs")];

        let state = AppState::new(diffs, Some("nonexistent.rs"));

        // Falls back to first file in sorted sidebar order
        assert_eq!(state.file_diffs[state.current_file].filename, "aaa.rs");
    }

    #[test]
    fn test_focus_updates_sidebar_selection() {
        let diffs = vec![
            make_file_diff("aaa.rs"),
            make_file_diff("bbb.rs"),
            make_file_diff("ccc.rs"),
        ];

        let state = AppState::new(diffs, Some("ccc.rs"));

        if let Some(SidebarItem::File { file_index, .. }) =
            state.sidebar_item_at_visible(state.sidebar_selected)
        {
            assert_eq!(*file_index, state.current_file);
        } else {
            panic!("sidebar_selected should point to a file");
        }
    }

    #[test]
    fn test_focus_empty_diffs() {
        let diffs = vec![];

        let state = AppState::new(diffs, Some("any.rs"));

        assert_eq!(state.current_file, 0);
        assert!(state.file_diffs.is_empty());
    }
}
