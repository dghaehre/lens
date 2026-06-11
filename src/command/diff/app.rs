use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;

use super::annotation::{AnnotationEditor, AnnotationEditorResult};
use super::annotation_store::{AnnotationStore, InMemoryStore, SqliteStore};
use super::pr_comment_editor::{PrCommentEditor, PrCommentEditorResult, PrCommentIntent};
use super::coordinates::{extract_selected_text, PanelLayout};
use super::highlight;
use super::loader::{get_current_branch, load_file_diffs, load_single_commit_diffs};
use super::render::{
    render_diff, render_empty_state, truncate_path, FilePickerItem, KeyBind, KeyBindSection, Modal,
    ModalContent, ModalFileStatus, ModalResult,
};
use super::state::{
    adjust_scroll_for_hunk, adjust_scroll_to_line, AnnotationSource, AppState, PendingGhAction,
    PendingKey,
};
use super::theme;
use super::types::{
    ChangeType, CursorPosition, DiffFullscreen, DiffPanelFocus, FileDiff, FileStatus, FocusedPanel,
    SelectionMode, SidebarItem,
};
use super::watcher::{setup_watcher, WatchEvent};
use super::DiffOptions;

use crate::commit_reference::CommitReference;
use crate::vcs::{GitBackend, StackedCommitInfo};

/// Navigate to a different commit in stacked mode.
/// Returns true if navigation was successful.
fn navigate_stacked_commit(
    state: &mut AppState,
    new_index: usize,
    options: &DiffOptions,
    backend: &GitBackend,
) -> bool {
    if new_index >= state.stacked_commits.len() {
        return false;
    }
    state.save_stacked_viewed_files();
    state.current_commit_index = new_index;
    if let Some(commit) = state.stacked_commits.get(new_index) {
        let file_diffs = load_single_commit_diffs(&commit.commit_id, &options.file, backend);
        state.reload(file_diffs, None);
        state.load_stacked_viewed_files();
        true
    } else {
        false
    }
}

/// Adjust sidebar scroll to ensure the selected item is visible.
fn ensure_sidebar_visible(state: &mut AppState, visible_height: usize) {
    if state.sidebar_selected >= state.sidebar_scroll + visible_height {
        state.sidebar_scroll = state.sidebar_selected.saturating_sub(visible_height) + 1;
    } else if state.sidebar_selected < state.sidebar_scroll {
        state.sidebar_scroll = state.sidebar_selected;
    }
}

/// Compute the largest horizontal scroll offset that still keeps content
/// in view for the current file. Walks `side_by_side` for the longest line
/// on each side and compares against the panel widths from `PanelLayout`.
fn max_h_scroll(state: &mut AppState, term_width: u16) -> u16 {
    if state.file_diffs.is_empty() {
        return 0;
    }
    let diff = &state.file_diffs[state.current_file];
    if diff.is_binary {
        return 0;
    }

    let sidebar_width = if state.show_sidebar {
        (term_width / 4).clamp(20, 35)
    } else {
        0
    };
    // The renderer collapses to a single panel for added or deleted files,
    // regardless of the diff_fullscreen toggle, so mirror that here.
    let effective_fullscreen = if diff.old_content.is_empty() && !diff.new_content.is_empty() {
        DiffFullscreen::NewOnly
    } else if !diff.old_content.is_empty() && diff.new_content.is_empty() {
        DiffFullscreen::OldOnly
    } else {
        state.diff_fullscreen
    };
    let layout = PanelLayout::calculate(
        term_width,
        sidebar_width,
        state.show_sidebar,
        effective_fullscreen,
    );

    state.ensure_cache();
    let sbs = state.side_by_side_ref();
    let mut max_old = 0usize;
    let mut max_new = 0usize;
    for line in sbs {
        if let Some((_, text)) = &line.old_line {
            max_old = max_old.max(text.chars().count());
        }
        if let Some((_, text)) = &line.new_line {
            max_new = max_new.max(text.chars().count());
        }
    }

    // Per-row overhead before the text: focus indicator (1 col) + line-number
    // gutter (5 cols). The new panel drops its indicator when it's rendered
    // alongside the old panel.
    let gutter = layout.gutter_width as usize;
    let old_overhead = layout.focus_indicator_width as usize + gutter;
    let new_overhead = if layout.old_panel_width > 0 {
        gutter
    } else {
        layout.focus_indicator_width as usize + gutter
    };

    let old_overflow = if layout.old_panel_width > 0 {
        (max_old + old_overhead).saturating_sub(layout.old_panel_width as usize)
    } else {
        0
    };
    let new_overflow = if layout.new_panel_width > 0 {
        (max_new + new_overhead).saturating_sub(layout.new_panel_width as usize)
    } else {
        0
    };

    old_overflow.max(new_overflow).min(u16::MAX as usize) as u16
}

/// Clamp `state.h_scroll` against the rightmost meaningful offset for the
/// current file and terminal width.
fn clamp_h_scroll(state: &mut AppState, term_width: u16) {
    let max = max_h_scroll(state, term_width);
    if state.h_scroll > max {
        state.h_scroll = max;
    }
}

/// Largest sidebar horizontal scroll offset that keeps a file/directory entry
/// in view. Computed from the longest visible label minus the sidebar's inner
/// content width.
fn max_sidebar_h_scroll(state: &AppState, term_width: u16) -> u16 {
    if !state.show_sidebar {
        return 0;
    }
    let sidebar_width = (term_width / 4).clamp(20, 35) as usize;
    // Sidebar uses Borders::TOP | LEFT | BOTTOM (the right edge is shared
    // with the diff panel's left border), so only the left border eats width.
    let inner_width = sidebar_width.saturating_sub(1);

    let mut max_label = 0usize;
    for item in &state.sidebar_items {
        let len = match item {
            SidebarItem::Directory { name, depth, .. } => {
                depth * 2 + 2 + 1 + 1 + name.chars().count()
            }
            SidebarItem::File { name, depth, .. } => depth * 2 + 2 + 1 + 1 + name.chars().count(),
        };
        max_label = max_label.max(len);
    }
    max_label.saturating_sub(inner_width).min(u16::MAX as usize) as u16
}

/// Clamp `state.sidebar_h_scroll` against the rightmost meaningful offset.
fn clamp_sidebar_h_scroll(state: &mut AppState, term_width: u16) {
    let max = max_sidebar_h_scroll(state, term_width);
    if state.sidebar_h_scroll > max {
        state.sidebar_h_scroll = max;
    }
}

/// Find the side_by_side array index for a given file line number on a specific panel.
fn find_sbs_index_for_line(
    side_by_side: &[super::types::DiffLine],
    panel: DiffPanelFocus,
    line_num: usize,
) -> Option<usize> {
    side_by_side
        .iter()
        .position(|dl| dl.line_number(panel) == Some(line_num))
}

fn preferred_hunk_annotation_panel(
    diff: &FileDiff,
    diff_fullscreen: DiffFullscreen,
) -> DiffPanelFocus {
    if (!diff.old_content.is_empty() && diff.new_content.is_empty())
        || matches!(diff_fullscreen, DiffFullscreen::OldOnly)
    {
        DiffPanelFocus::Old
    } else {
        DiffPanelFocus::New
    }
}

fn alternate_diff_panel(panel: DiffPanelFocus) -> DiffPanelFocus {
    match panel {
        DiffPanelFocus::Old => DiffPanelFocus::New,
        DiffPanelFocus::New | DiffPanelFocus::None => DiffPanelFocus::Old,
    }
}

/// Format an annotation for display in the annotations list.
fn format_annotation_preview(annotation: &super::state::Annotation, orphaned: bool) -> String {
    let preview = annotation.root().content.lines().next().unwrap_or("");
    let preview = if preview.len() > 40 {
        format!("{}...", &preview[..40])
    } else {
        preview.to_string()
    };
    let truncated_filename = truncate_path(&annotation.filename, 30);
    let line_display = annotation.line_range_display();
    let label = annotation.target_label();
    let orphan_tag = if orphaned { " (orphaned)" } else { "" };
    let resolved_tag = if annotation.resolved { "✓ " } else { "" };
    if line_display.is_empty() {
        format!(
            "{}{} [{}]{} | {} | {}",
            resolved_tag,
            truncated_filename,
            label,
            orphan_tag,
            preview,
            annotation.format_time()
        )
    } else {
        format!(
            "{}{}:{} [{}]{} | {} | {}",
            resolved_tag,
            truncated_filename,
            line_display,
            label,
            orphan_tag,
            preview,
            annotation.format_time()
        )
    }
}

/// Sort key for in-diff annotation navigation: (file_index, line).
/// File-targeted annotations sort at the top of their file (line 0).
/// Returns `None` if the annotation's file is no longer in the diff
/// (i.e., it's orphaned at the file level) — such annotations are skipped
/// during `(` / `)` traversal.
fn annotation_sort_key(
    ann: &super::state::Annotation,
    file_diffs: &[FileDiff],
) -> Option<(usize, usize)> {
    let file_idx = file_diffs.iter().position(|f| f.filename == ann.filename)?;
    let line = match &ann.target {
        super::state::AnnotationTarget::File => 0,
        super::state::AnnotationTarget::LineRange { start_line, .. } => *start_line,
    };
    Some((file_idx, line))
}

/// Return the file line number at the top of the current scroll viewport,
/// preferring the new panel and falling back to the old panel. Used as the
/// "cursor" position when finding the next/previous annotation.
fn current_anchor_line(state: &mut AppState) -> usize {
    state.ensure_cache();
    let sbs = state.side_by_side_ref();
    if sbs.is_empty() {
        return 0;
    }
    let idx = (state.scroll as usize).min(sbs.len() - 1);
    sbs.get(idx)
        .and_then(|dl| {
            dl.line_number(DiffPanelFocus::New)
                .or_else(|| dl.line_number(DiffPanelFocus::Old))
        })
        .unwrap_or(0)
}

/// Switch to `ann`'s file (if needed) and scroll so its anchor is visible.
/// Mirrors the behavior of the `AnnotationJump` modal result so that the
/// `I`-list and the `(` / `)` shortcuts converge on the same outcome.
fn jump_to_annotation(
    state: &mut AppState,
    ann: &super::state::Annotation,
    visible_height: usize,
    max_scroll: usize,
) {
    let filename = ann.filename.clone();
    let target = ann.target.clone();
    let Some(file_index) = state
        .file_diffs
        .iter()
        .position(|f| f.filename == filename)
    else {
        return;
    };
    if state.current_file != file_index {
        state.select_file(file_index);
    }
    if let super::state::AnnotationTarget::LineRange {
        panel, start_line, ..
    } = &target
    {
        state.ensure_cache();
        let sbs = state.side_by_side_ref();
        if let Some(sbs_idx) = find_sbs_index_for_line(sbs, *panel, *start_line) {
            state.scroll =
                adjust_scroll_to_line(sbs_idx, state.scroll, visible_height, max_scroll);
        }
    }
}

/// While annotation-select mode is active, the diff view only responds to
/// these keys: navigation (`j`/`k`/Up/Down), the in-mode actions (`R`/`X`),
/// quit (`q`, Ctrl+c), and Esc to exit the mode. Anything else is silently
/// ignored so the mode stays sticky until the user explicitly leaves.
fn allowed_in_annotation_select_mode(key: &KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Esc
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('j')
            | KeyCode::Char('k')
            | KeyCode::Char('R')
            | KeyCode::Char('X')
            | KeyCode::Char('q')
    ) || (matches!(key.code, KeyCode::Char('c'))
        && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Annotations sorted by `(file_index, start_line)`. Orphaned (file
/// missing from diff) annotations are skipped — they're only reachable
/// via the `I` modal.
fn sorted_annotations(state: &AppState) -> Vec<((usize, usize), &super::state::Annotation)> {
    let mut keyed: Vec<((usize, usize), &super::state::Annotation)> = state
        .annotations
        .iter()
        .filter_map(|a| annotation_sort_key(a, &state.file_diffs).map(|k| (k, a)))
        .collect();
    keyed.sort_by_key(|(k, _)| *k);
    keyed
}

/// Distance between two `(file_index, line)` keys. Cross-file distance
/// dominates so the nearest annotation in the current file wins over any
/// annotation in another file.
fn key_distance(a: (usize, usize), b: (usize, usize)) -> u64 {
    let file_diff = if a.0 >= b.0 { a.0 - b.0 } else { b.0 - a.0 } as u64;
    let line_diff = if a.1 >= b.1 { a.1 - b.1 } else { b.1 - a.1 } as u64;
    file_diff.saturating_mul(1_000_000).saturating_add(line_diff)
}

/// Annotation whose sort key is closest to `from`. Used to pick the
/// starting annotation when entering annotation-select mode.
fn nearest_annotation_to(
    state: &AppState,
    from: (usize, usize),
) -> Option<super::state::Annotation> {
    sorted_annotations(state)
        .into_iter()
        .min_by_key(|(k, _)| key_distance(*k, from))
        .map(|(_, a)| a.clone())
}

/// Next or previous annotation after `from` in `(file_index, line)` order.
/// Clamps at the ends — does not wrap.
fn adjacent_annotation_to(
    state: &AppState,
    from: (usize, usize),
    forward: bool,
) -> Option<super::state::Annotation> {
    let keyed = sorted_annotations(state);
    if keyed.is_empty() {
        return None;
    }
    let picked = if forward {
        keyed.iter().find(|(k, _)| *k > from)
    } else {
        keyed.iter().rev().find(|(k, _)| *k < from)
    };
    picked.map(|(_, a)| (*a).clone())
}

/// Human-readable label for the active annotation source, used by the
/// modal's source-picker row and the footer. Includes a `(read-only)`
/// suffix when the active store can't be mutated (Phase 1: always the
/// case for GitHub).
fn source_label(state: &AppState) -> String {
    match state.annotation_source {
        AnnotationSource::Local => "local".to_string(),
        AnnotationSource::Github => {
            let pr = state
                .pr_info
                .as_ref()
                .map(|p| p.slug())
                .unwrap_or_else(|| "?".to_string());
            let suffix = if state.active_store_is_read_only() {
                " (read-only)"
            } else {
                ""
            };
            format!("github {}{}", pr, suffix)
        }
    }
}

/// Build the compact status header rendered above the PR description body.
/// Returns up to two lines:
///   1. author · state badge · draft tag · mergeability · review decision · inline-thread count
///   2. checks summary (only when the PR has any checks)
///
/// `inline_threads` is the number of inline review threads (≡ number of
/// annotations available when the user switches the annotation source to
/// GitHub). `None` means we didn't manage to fetch the count — render
/// nothing rather than zero, so the absence reads as "unknown" not
/// "definitely none".
///
/// Returns an empty vec when there's nothing useful to show (e.g., `gh`
/// returned the new fields as empty strings on an older binary).
fn build_pr_status_header(
    pr: &crate::vcs::github::PrInfo,
    inline_threads: Option<u32>,
) -> Vec<ratatui::text::Line<'static>> {
    use crate::vcs::github::PrCheckSummary;
    let t = theme::get();

    let muted = Style::default().fg(t.ui.text_muted);
    let sep = Span::styled("  ·  ", muted);

    let mut first: Vec<Span<'static>> = Vec::new();

    // Author tag at the head — "@login" (with "[bot]" suffix for bots,
    // and the display name in parens if it differs from the login).
    if !pr.author.login.is_empty() {
        let bot_suffix = if pr.author.is_bot { "[bot]" } else { "" };
        first.push(Span::styled(
            format!("@{}{}", pr.author.login, bot_suffix),
            Style::default().fg(t.ui.text_secondary).bold(),
        ));
        if !pr.author.name.is_empty() && pr.author.name != pr.author.login {
            first.push(Span::styled(
                format!(" ({})", pr.author.name),
                Style::default().fg(t.ui.text_muted),
            ));
        }
        first.push(sep.clone());
    }

    // State badge — "Open" / "Merged" / "Closed". Draft overrides for
    // visual priority (a draft is still technically OPEN).
    let (state_label, state_fg) = if pr.is_draft {
        ("Draft", t.ui.text_muted)
    } else {
        match pr.state.as_str() {
            "OPEN" => ("Open", t.ui.status_added),
            "MERGED" => ("Merged", t.ui.highlight),
            "CLOSED" => ("Closed", t.ui.status_deleted),
            "" => ("?", t.ui.text_muted),
            other => {
                // Title-case unknown states so we don't yell at the user.
                let mut s = String::new();
                let mut chars = other.chars();
                if let Some(c) = chars.next() {
                    s.push(c.to_ascii_uppercase());
                }
                for c in chars {
                    s.push(c.to_ascii_lowercase());
                }
                first.push(Span::styled(
                    format!(" {} ", s),
                    Style::default()
                        .fg(t.ui.selection_fg)
                        .bg(t.ui.text_muted)
                        .bold(),
                ));
                ("", t.ui.text_muted)
            }
        }
    };
    if !state_label.is_empty() {
        first.push(Span::styled(
            format!(" {} ", state_label),
            Style::default()
                .fg(t.ui.selection_fg)
                .bg(state_fg)
                .bold(),
        ));
    }

    // Mergeability — only shown when meaningful.
    match pr.mergeable.as_str() {
        "MERGEABLE" => {
            first.push(sep.clone());
            first.push(Span::styled(
                "mergeable",
                Style::default().fg(t.ui.status_added),
            ));
        }
        "CONFLICTING" => {
            first.push(sep.clone());
            first.push(Span::styled(
                "conflicting",
                Style::default().fg(t.ui.status_deleted),
            ));
        }
        _ => {}
    }

    // Review decision — only shown when GitHub actually returned one.
    match pr.review_decision.as_str() {
        "APPROVED" => {
            first.push(sep.clone());
            first.push(Span::styled(
                "approved",
                Style::default().fg(t.ui.status_added),
            ));
        }
        "CHANGES_REQUESTED" => {
            first.push(sep.clone());
            first.push(Span::styled(
                "changes requested",
                Style::default().fg(t.ui.status_deleted),
            ));
        }
        "REVIEW_REQUIRED" => {
            first.push(sep.clone());
            first.push(Span::styled("review required", muted));
        }
        _ => {}
    }

    // Inline-comment count: how many annotations the user can expect to
    // find when they switch the annotation source to GitHub. We only
    // render the segment when the fetch succeeded — a missing value reads
    // as "unknown", which is more honest than a misleading "0".
    if let Some(n) = inline_threads {
        first.push(sep.clone());
        let style = if n > 0 {
            Style::default().fg(t.ui.highlight).bold()
        } else {
            muted
        };
        first.push(Span::styled(
            format!(
                "💬 {} inline {}",
                n,
                if n == 1 { "thread" } else { "threads" }
            ),
            style,
        ));
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    if !first.is_empty() {
        lines.push(Line::from(first));
    }

    // Checks summary — second line when present.
    if !pr.checks.is_empty() {
        let s = PrCheckSummary::from_checks(&pr.checks);
        let mut spans: Vec<Span<'static>> = vec![Span::styled("checks: ", muted)];
        let mut first_seg = true;
        let push_seg = |spans: &mut Vec<Span<'static>>,
                            count: usize,
                            symbol: &str,
                            label: &str,
                            color: Color,
                            first_seg: &mut bool| {
            if count == 0 {
                return;
            }
            if !*first_seg {
                spans.push(Span::styled("  ", Style::default()));
            }
            spans.push(Span::styled(
                format!("{} {} {}", symbol, count, label),
                Style::default().fg(color),
            ));
            *first_seg = false;
        };
        push_seg(
            &mut spans,
            s.passed,
            "✓",
            "passed",
            t.ui.status_added,
            &mut first_seg,
        );
        push_seg(
            &mut spans,
            s.failed,
            "✗",
            "failed",
            t.ui.status_deleted,
            &mut first_seg,
        );
        push_seg(
            &mut spans,
            s.pending,
            "●",
            "pending",
            t.ui.watching,
            &mut first_seg,
        );
        push_seg(
            &mut spans,
            s.skipped,
            "○",
            "skipped",
            t.ui.text_muted,
            &mut first_seg,
        );
        if first_seg {
            // No counts landed in any bucket (shouldn't happen, but be
            // defensive) — emit a fallback so the row isn't blank.
            spans.push(Span::styled(format!("{} total", s.total), muted));
        }
        lines.push(Line::from(spans));
    }

    lines
}

/// Build the markdown shown inside the PR description modal body. The PR
/// body comes first, then a "Comments" section listing every non-anchored
/// conversation entry (issue-level PR comments + review summary bodies).
/// Inline review-thread comments are intentionally omitted — those are
/// already surfaced as annotations elsewhere in the UI.
fn build_pr_body_markdown(pr: &crate::vcs::github::PrInfo) -> String {
    let mut out = String::new();
    if !pr.body.trim().is_empty() {
        out.push_str(&pr.body);
    }

    if !pr.comments.is_empty() {
        if !out.is_empty() {
            // Blank line + horizontal rule to visually separate the
            // description from the conversation. The markdown highlighter
            // styles `---` as a token so it reads as an intentional break.
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!("## Comments ({})\n", pr.comments.len()));
        for (i, c) in pr.comments.iter().enumerate() {
            if i > 0 {
                out.push_str("\n---\n");
            }
            out.push('\n');
            let bot_suffix = if c.author.is_bot { "[bot]" } else { "" };
            let login = if c.author.login.is_empty() {
                "unknown"
            } else {
                &c.author.login
            };
            let state_tag = c
                .review_state
                .as_deref()
                .map(|s| match s {
                    "APPROVED" => " _(approved)_",
                    "CHANGES_REQUESTED" => " _(changes requested)_",
                    "COMMENTED" => " _(review)_",
                    "DISMISSED" => " _(dismissed)_",
                    _ => "",
                })
                .unwrap_or("");
            let date = if c.date.is_empty() { "" } else { &c.date };
            out.push_str(&format!(
                "**@{}{}** · {}{}\n\n{}\n",
                login, bot_suffix, date, state_tag, c.body
            ));
        }
    }

    out
}

/// Build the items + annotations + diff snippets for the `I` (all-
/// annotations) modal. Active annotations come first (sorted by sidebar
/// file order, then by anchor line so the list reads top-to-bottom like
/// the diff), orphaned (anchor missing) ones after. Orphaned entries are
/// sorted by filename + line for stability. The third vector carries a
/// short snippet of the file content around each annotation's anchor so
/// the preview pane can show the code being commented on.
fn build_annotation_list(
    state: &AppState,
) -> (
    Vec<String>,
    Vec<super::state::Annotation>,
    Vec<Vec<(usize, String)>>,
) {
    let mut active: Vec<&super::state::Annotation> = Vec::new();
    let mut orphaned: Vec<&super::state::Annotation> = Vec::new();
    for ann in &state.annotations {
        if state.is_annotation_orphaned(ann) {
            orphaned.push(ann);
        } else {
            active.push(ann);
        }
    }

    // Sort active by (sidebar position, line). Using sidebar_items as the
    // source of truth ensures the modal lists files in the same order the
    // user sees them in the file explorer (sidebar is alphabetically sorted
    // within directories; file_diffs may be in a different order).
    let sidebar_pos = |filename: &str| -> usize {
        state
            .sidebar_items
            .iter()
            .position(|item| {
                matches!(item, SidebarItem::File { path, .. } if path == filename)
            })
            .unwrap_or(usize::MAX)
    };
    let anchor_line = |ann: &super::state::Annotation| -> usize {
        match &ann.target {
            super::state::AnnotationTarget::File => 0,
            super::state::AnnotationTarget::LineRange { start_line, .. } => *start_line,
        }
    };
    active.sort_by(|a, b| {
        sidebar_pos(&a.filename)
            .cmp(&sidebar_pos(&b.filename))
            .then_with(|| anchor_line(a).cmp(&anchor_line(b)))
    });
    orphaned.sort_by(|a, b| {
        a.filename
            .cmp(&b.filename)
            .then_with(|| anchor_line(a).cmp(&anchor_line(b)))
    });

    let mut items: Vec<String> = active
        .iter()
        .map(|a| format_annotation_preview(a, false))
        .collect();
    items.extend(
        orphaned
            .iter()
            .map(|a| format_annotation_preview(a, true)),
    );
    let mut annotations: Vec<super::state::Annotation> =
        active.into_iter().cloned().collect();
    annotations.extend(orphaned.into_iter().cloned());

    // Snippets are parallel to `annotations`. Orphaned annotations whose
    // file is gone get an empty snippet; render code falls back to a
    // placeholder. Context lines around the anchor help orient the reader.
    let snippets: Vec<Vec<(usize, String)>> = annotations
        .iter()
        .map(|ann| {
            state
                .file_diffs
                .iter()
                .find(|f| f.filename == ann.filename)
                .map(|fd| extract_diff_snippet(fd, ann))
                .unwrap_or_default()
        })
        .collect();

    (items, annotations, snippets)
}

/// Rebuild the annotations list modal from current state, carrying over the
/// previous row selection and source-picker focus from `current` (when it is
/// an `Annotations` modal). Used wherever the underlying annotation set may
/// have shifted out from under an open modal — external DB writes, source
/// switches, reply/edit/resolve actions.
fn rebuild_annotations_modal(state: &AppState, current: Option<&Modal>) -> Modal {
    let (prev_selected, prev_source_focused) = match current.map(|m| &m.content) {
        Some(ModalContent::Annotations {
            selected,
            source_focused,
            ..
        }) => (*selected, *source_focused),
        _ => (0, false),
    };
    let (items, annotations, snippets) = build_annotation_list(state);
    let mut modal = Modal::annotations(
        "Annotations",
        items,
        annotations,
        snippets,
        source_label(state),
    );
    if let ModalContent::Annotations {
        selected,
        source_focused,
        items,
        ..
    } = &mut modal.content
    {
        *selected = prev_selected.min(items.len().saturating_sub(1));
        *source_focused = prev_source_focused;
    }
    modal
}

/// Pull the lines around `ann`'s anchor out of the appropriate side of the
/// diff (old vs new content), with a small context window so the reader
/// has orientation. Returns `(line_number, content)` pairs; file-level
/// annotations and ranges past the end of the file return an empty snippet.
fn extract_diff_snippet(
    file_diff: &super::types::FileDiff,
    ann: &super::state::Annotation,
) -> Vec<(usize, String)> {
    use super::state::AnnotationTarget;
    let (panel, start_line, end_line) = match &ann.target {
        AnnotationTarget::File => return Vec::new(),
        AnnotationTarget::LineRange {
            panel,
            start_line,
            end_line,
            ..
        } => (*panel, *start_line, *end_line),
    };
    let content = match panel {
        DiffPanelFocus::Old => &file_diff.old_content,
        _ => &file_diff.new_content,
    };
    let all_lines: Vec<&str> = content.lines().collect();
    if all_lines.is_empty() || start_line == 0 || end_line == 0 {
        return Vec::new();
    }

    const CONTEXT: usize = 2;
    let start_idx = start_line.saturating_sub(1).saturating_sub(CONTEXT);
    let end_idx = end_line
        .saturating_sub(1)
        .saturating_add(CONTEXT)
        .min(all_lines.len().saturating_sub(1));

    (start_idx..=end_idx)
        .filter_map(|i| all_lines.get(i).map(|l| (i + 1, l.to_string())))
        .collect()
}

pub fn run_app(options: DiffOptions, backend: &GitBackend) -> io::Result<()> {
    let file_diffs = load_file_diffs(&options, backend);
    run_app_internal(options, file_diffs, None, backend)
}

pub fn run_app_stacked(
    options: DiffOptions,
    commits: Vec<StackedCommitInfo>,
    backend: &GitBackend,
) -> io::Result<()> {
    // Load the first commit's diff
    let first_commit = &commits[0];
    let file_diffs = load_single_commit_diffs(&first_commit.commit_id, &options.file, backend);
    run_app_internal(options, file_diffs, Some(commits), backend)
}

/// Unified event source for the main loop: either a crossterm input event or
/// a file-watcher event. Bridged into a single mpsc channel so the loop can
/// block on a single `recv()`.
enum AppEvent {
    Input(Event),
    Watch(WatchEvent),
}

/// Spawn a thread that reads crossterm input events and forwards them as
/// `AppEvent::Input`. Returns a stop-flag and the join handle. The thread
/// must be stopped (flag set, then joined) before handing the tty off to a
/// child process such as `$EDITOR`; otherwise both readers race for stdin
/// and the child appears to lose keystrokes / hang.
fn spawn_input_thread(
    tx: std::sync::mpsc::Sender<AppEvent>,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let handle = std::thread::spawn(move || {
        while running_clone.load(Ordering::Relaxed) {
            // Poll with a short timeout so the stop-flag is checked promptly
            // without busy-waiting.
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(AppEvent::Input(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    (running, handle)
}

fn run_app_internal(
    mut options: DiffOptions,
    file_diffs: Vec<super::types::FileDiff>,
    stacked_commits: Option<Vec<StackedCommitInfo>>,
    backend: &GitBackend,
) -> io::Result<()> {
    theme::init(options.theme.as_deref());
    highlight::init();

    // Annotations persist in `<git_dir>/lens.db`; fall back to in-memory if
    // SQLite open fails so the TUI still works.
    let db_path = backend.git_dir().join("lens.db");
    let store: Box<dyn AnnotationStore> = match SqliteStore::open(&db_path) {
        Ok(s) => Box::new(s),
        Err(e) => {
            eprintln!(
                "\x1b[93mwarning:\x1b[0m could not open annotation DB at {}: {} \u{2014} annotations will not persist this session",
                db_path.display(),
                e
            );
            Box::new(InMemoryStore::new())
        }
    };
    let mut state = AppState::with_store(file_diffs, options.focus.as_deref(), store);

    let diff_ref_str = options.reference.as_ref().map(|r| r.as_str().to_string());
    state.set_diff_reference(diff_ref_str);

    // Initialize stacked mode if commits were provided
    if let Some(commits) = stacked_commits {
        state.init_stacked_mode(commits);
    }

    // Now enter TUI mode
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(EnableMouseCapture)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let watch_rx = if options.watch {
        setup_watcher(Some(&db_path))
    } else {
        None
    };

    let mut active_modal: Option<Modal> = None;
    let mut annotation_editor: Option<AnnotationEditor> = None;
    let mut pr_comment_editor: Option<PrCommentEditor> = None;
    let mut pending_changed_files: Option<std::collections::HashSet<String>> = None;
    let mut pending_events: VecDeque<AppEvent> = VecDeque::new();

    // Bridge crossterm input and the file watcher into a single channel so the
    // main loop can block on `recv()` and wake up only when something happens.
    let (app_tx, app_rx) = std::sync::mpsc::channel::<AppEvent>();
    let (mut input_running, mut input_handle) = spawn_input_thread(app_tx.clone());
    if let Some(rx) = watch_rx {
        let tx = app_tx.clone();
        std::thread::spawn(move || {
            for ev in rx {
                if tx.send(AppEvent::Watch(ev)).is_err() {
                    break;
                }
            }
        });
    }

    'main: loop {
        if state.needs_reload {
            let file_diffs = load_file_diffs(&options, backend);

            // Pass changed files to reload so it can unmark them from viewed
            let changed_files = pending_changed_files.take();
            state.reload(file_diffs, changed_files.as_ref());
        }

        if state.file_diffs.is_empty() {
            terminal.draw(|frame| {
                render_empty_state(frame, options.watch, &state.ref_input);
                if let Some(ref modal) = active_modal {
                    modal.render(frame);
                }
            })?;
        } else {
            // Use cached side_by_side (avoids recomputing diff every frame during drag etc.)
            state.update_search_matches();
            // Ensure highlighters are cached (only recomputed when file changes)
            state.get_highlighters();
            let diff = &state.file_diffs[state.current_file];
            let side_by_side = state.side_by_side_ref();
            let hunks = state.hunks_ref();
            let (old_hl, new_hl) = state.highlighters_ref().unwrap();
            let hunk_count = hunks.len();
            let empty_viewed_hunks: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            let viewed_hunks_for_file = state
                .viewed_hunks
                .get(&diff.filename)
                .unwrap_or(&empty_viewed_hunks);
            let branch_fallback = get_current_branch(backend);
            let commit_ref = state.diff_reference.as_deref().unwrap_or(&branch_fallback);
            // Built outside `terminal.draw` so the borrow lives across the
            // closure capture below.
            let annotation_source_label_owned = source_label(&state);
            let annotation_source_error_owned = state.annotation_source_status.clone();
            let loading_message_owned = state.loading_message.clone();
            let row_offset = std::cell::Cell::new(0usize);
            let gaps_cell = std::cell::RefCell::new(Vec::new());
            let total_ann_height = std::cell::Cell::new(0usize);
            terminal.draw(|frame| {
                let (offset, gaps, total_ann) = render_diff(
                    frame,
                    diff,
                    &state.file_diffs,
                    &state.sidebar_items,
                    &state.sidebar_visible,
                    &state.collapsed_dirs,
                    state.current_file,
                    state.scroll,
                    state.h_scroll,
                    options.watch,
                    state.show_sidebar,
                    state.focused_panel,
                    state.sidebar_selected,
                    state.sidebar_scroll,
                    state.sidebar_h_scroll,
                    &state.viewed_files,
                    &state.settings,
                    hunk_count,
                    state.diff_fullscreen,
                    &state.search_state,
                    &state.ref_input,
                    commit_ref,
                    state.focused_hunk,
                    &hunks,
                    state.stacked_mode,
                    state.current_commit(),
                    state.current_commit_index,
                    state.stacked_commits.len(),
                    &side_by_side,
                    &state.annotations,
                    &state.selection,
                    old_hl,
                    new_hl,
                    viewed_hunks_for_file,
                    state.total_added,
                    state.total_removed,
                    Some(annotation_source_label_owned.as_str()),
                    annotation_source_error_owned.as_deref(),
                    state.annotation_select.as_ref(),
                    loading_message_owned.as_deref(),
                );
                row_offset.set(offset);
                total_ann_height.set(total_ann);

                // Selection action tooltip (shown after drag completes)
                if state.show_selection_tooltip
                    && state.selection.is_active()
                    && !state.is_dragging
                    && annotation_editor.is_none()
                    && active_modal.is_none()
                {
                    let t = theme::get();
                    let term = frame.area();
                    let header_h: u16 = if state.stacked_mode { 1 } else { 0 };
                    let sidebar_w: u16 = if state.show_sidebar {
                        (term.width / 4).clamp(20, 35)
                    } else {
                        0
                    };
                    let layout = PanelLayout::calculate(
                        term.width,
                        sidebar_w,
                        state.show_sidebar,
                        state.diff_fullscreen,
                    );

                    let sel = &state.selection;
                    let (_, sel_end) = sel.normalized_range();
                    let scroll_usize = state.scroll as usize;

                    if sel_end.line >= scroll_usize {
                        let content_y = sel_end.line - scroll_usize;

                        // Account for annotation overlay gaps
                        let mut cum_gaps: u16 = 0;
                        for &(after_line, gap_h) in &gaps {
                            if after_line < content_y {
                                cum_gaps += gap_h as u16;
                            }
                        }

                        // Position below the selection end
                        let screen_y =
                            header_h + 1 + offset as u16 + content_y as u16 + cum_gaps + 1;

                        let (panel_x, panel_w) = match sel.panel {
                            DiffPanelFocus::Old => (layout.old_panel_x, layout.old_panel_width),
                            DiffPanelFocus::New => (layout.new_panel_x, layout.new_panel_width),
                            _ => (0, 0),
                        };

                        if panel_w > 0 && screen_y < term.height.saturating_sub(1) {
                            let tip_w: u16 = 27;
                            let tip_h: u16 = 1;

                            let cx = layout.content_x_offset(sel.panel);
                            let tip_x =
                                (panel_x + cx).min(panel_x + panel_w.saturating_sub(tip_w + 1));

                            let tip_area = Rect::new(tip_x, screen_y, tip_w.min(panel_w), tip_h);

                            let bg = t.ui.footer_branch_bg;
                            let key_style = Style::default().fg(t.ui.text_primary).bg(bg).bold();
                            let desc_style = Style::default().fg(t.ui.text_muted).bg(bg);
                            let tip_line = Line::from(vec![
                                Span::styled(" i", key_style),
                                Span::styled(" annotate ", desc_style),
                                Span::styled("y", key_style),
                                Span::styled(" copy ", desc_style),
                                Span::styled("esc", key_style),
                                Span::styled("   ", desc_style),
                            ]);

                            frame.render_widget(ratatui::widgets::Clear, tip_area);
                            frame.render_widget(
                                ratatui::widgets::Paragraph::new(tip_line)
                                    .style(Style::default().bg(bg)),
                                tip_area,
                            );
                        }
                    }
                }

                *gaps_cell.borrow_mut() = gaps;
                // Render annotation editor (on top of everything except modal)
                if let Some(ref editor) = annotation_editor {
                    editor.render(frame);
                }
                // PR comment editor sits above the modal because we close
                // the modal before opening the editor anyway; rendered
                // here so it overlays leftover content if both are set.
                if let Some(ref editor) = pr_comment_editor {
                    editor.render(frame);
                }
                if let Some(ref modal) = active_modal {
                    modal.render(frame);
                }
            })?;
            state.content_row_offset = row_offset.get();
            state.annotation_overlay_gaps = gaps_cell.into_inner();
            state.total_annotation_height = total_ann_height.get();
        }

        // Drain a queued blocking `gh` call now — i.e. AFTER the draw above
        // has flushed the loading indicator to screen, so the user sees it
        // for the duration of the freeze. After the call returns, clear the
        // indicator and loop back to redraw with fresh state before
        // blocking on input again.
        if let Some(action) = state.pending_gh_action.take() {
            match action {
                PendingGhAction::SwitchAnnotationSource(target) => {
                    let switch_result = state.switch_annotation_source(target, backend);
                    let (items, annotations, snippets) = build_annotation_list(&state);
                    let mut modal = Modal::annotations(
                        "Annotations",
                        items,
                        annotations,
                        snippets,
                        source_label(&state),
                    );
                    modal.annotations_focus_source();
                    if let Err(msg) = switch_result {
                        modal.annotations_set_error(msg);
                    }
                    active_modal = Some(modal);
                }
                PendingGhAction::ShowPrDescription => {
                    // Two `gh` calls + a modal build. Errors are swallowed
                    // here to match the pre-defer behaviour: refresh failures
                    // are already stashed into `annotation_source_status` by
                    // `refresh_pr_info`, surfaced by the footer next frame.
                    if let Ok(pr) = state.refresh_pr_info() {
                        let title = format!("{} · {}", pr.slug(), pr.title);
                        let body = build_pr_body_markdown(pr);
                        let inline_threads = crate::vcs::github::count_review_threads(
                            &crate::vcs::github::RealGhClient,
                            pr,
                        )
                        .ok();
                        let status_lines = build_pr_status_header(pr, inline_threads);
                        let term_width = terminal.size()?.width;
                        active_modal = Some(Modal::pr_description(
                            title,
                            status_lines,
                            &body,
                            term_width,
                        ));
                    }
                }
                PendingGhAction::SubmitPrComment { intent, body } => {
                    // Submit the comment/approval, then on success re-fetch
                    // the PR + thread count and reopen the description
                    // modal so the user sees their new entry. On failure
                    // we stash the message into `annotation_source_status`
                    // for the footer to surface.
                    let result = if let Some(pr) = state.pr_info.clone() {
                        let gh = crate::vcs::github::RealGhClient;
                        match intent {
                            PrCommentIntent::Comment => {
                                crate::vcs::github::add_pr_comment(&gh, &pr, &body)
                            }
                            PrCommentIntent::Approve => {
                                crate::vcs::github::approve_pr(&gh, &pr, &body)
                            }
                        }
                    } else {
                        Err(crate::vcs::github::GhError::GhFailure(
                            "no PR resolved".into(),
                        ))
                    };

                    match result {
                        Ok(()) => {
                            if let Ok(pr) = state.refresh_pr_info() {
                                let title = format!("{} · {}", pr.slug(), pr.title);
                                let body_md = build_pr_body_markdown(pr);
                                let inline_threads =
                                    crate::vcs::github::count_review_threads(
                                        &crate::vcs::github::RealGhClient,
                                        pr,
                                    )
                                    .ok();
                                let status_lines =
                                    build_pr_status_header(pr, inline_threads);
                                let term_width = terminal.size()?.width;
                                active_modal = Some(Modal::pr_description(
                                    title,
                                    status_lines,
                                    &body_md,
                                    term_width,
                                ));
                            }
                        }
                        Err(e) => {
                            state.annotation_source_status = Some(format!("{}", e));
                        }
                    }
                }
                PendingGhAction::CopyAnnotationToOther(annotation_id) => {
                    let push_result =
                        state.copy_annotation_to_other_source(&annotation_id, backend);
                    let prev_selected = match &active_modal {
                        Some(m) => match &m.content {
                            ModalContent::Annotations { selected, .. } => *selected,
                            _ => 0,
                        },
                        None => 0,
                    };
                    let (items, annotations, snippets) = build_annotation_list(&state);
                    let mut modal = Modal::annotations(
                        "Annotations",
                        items,
                        annotations,
                        snippets,
                        source_label(&state),
                    );
                    if let ModalContent::Annotations {
                        selected, items, ..
                    } = &mut modal.content
                    {
                        *selected = prev_selected.min(items.len().saturating_sub(1));
                    }
                    match push_result {
                        Ok(()) => {
                            if let Some(msg) = state.annotation_source_status.clone() {
                                modal.annotations_set_info(msg);
                            }
                        }
                        Err(msg) => modal.annotations_set_error(msg),
                    }
                    active_modal = Some(modal);
                }
            }
            state.loading_message = None;
            continue 'main;
        }

        // Block waiting for the next event (input or watcher). Then drain any
        // others that are immediately ready so a burst is handled together.
        if pending_events.is_empty() {
            match app_rx.recv() {
                Ok(ev) => pending_events.push_back(ev),
                Err(_) => break 'main,
            }
        }
        while let Ok(ev) = app_rx.try_recv() {
            pending_events.push_back(ev);
        }

        // Process all pending events
        while let Some(app_event) = pending_events.pop_front() {
            let current_event = match app_event {
                AppEvent::Watch(WatchEvent::Files { changed_files }) => {
                    state.needs_reload = true;
                    // Merge with any previously-pending set so multiple bursts
                    // don't lose the earlier filenames.
                    pending_changed_files
                        .get_or_insert_with(std::collections::HashSet::new)
                        .extend(changed_files);
                    continue;
                }
                AppEvent::Watch(WatchEvent::AnnotationDb) => {
                    // The DB file changed from outside (CLI/agent). Refresh the
                    // annotation cache without reloading diffs. If the
                    // annotations list modal is open against the local store,
                    // rebuild it so the user sees the external edit live.
                    // GitHub source is skipped — its store is not backed by
                    // lens.db, so a DB-file change can't have affected it.
                    state.reload_annotations();
                    let annotations_modal_open = active_modal
                        .as_ref()
                        .is_some_and(|m| matches!(m.content, ModalContent::Annotations { .. }));
                    if annotations_modal_open
                        && matches!(state.annotation_source, AnnotationSource::Local)
                    {
                        active_modal =
                            Some(rebuild_annotations_modal(&state, active_modal.as_ref()));
                    }
                    continue;
                }
                AppEvent::Input(ev) => ev,
            };
            let visible_height = terminal.size()?.height.saturating_sub(2) as usize;
            let bottom_padding = 5;
            let max_scroll = if !state.file_diffs.is_empty() {
                let total_lines = state.total_lines();
                // Annotations (file-level + inline overlays) push content rows past the
                // visible window, so add their total height to keep the bottom reachable.
                let annotation_pad = state.total_annotation_height;
                (total_lines + annotation_pad)
                    .saturating_sub(visible_height.saturating_sub(bottom_padding))
            } else {
                0
            };

            match current_event {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && state.ref_input.active =>
                {
                    match key.code {
                        KeyCode::Esc => {
                            state.ref_input.cancel();
                        }
                        KeyCode::Enter => {
                            let raw = state.ref_input.query.trim().to_string();
                            if raw.is_empty() {
                                options.reference = None;
                                state.set_diff_reference(None);
                                state.ref_input.cancel();
                                state.needs_reload = true;
                            } else if raw == "--cached" || raw == "--staged" {
                                // `git diff --cached` sentinel — bypass ref resolution.
                                match raw.parse::<CommitReference>() {
                                    Ok(reference) => {
                                        options.reference = Some(reference);
                                        state.set_diff_reference(Some(raw));
                                        state.ref_input.cancel();
                                        state.needs_reload = true;
                                    }
                                    Err(e) => {
                                        state.ref_input.error = Some(e.to_string());
                                    }
                                }
                            } else {
                                match backend.resolve_ref(&raw) {
                                    Ok(_) => match raw.parse::<CommitReference>() {
                                        Ok(reference) => {
                                            options.reference = Some(reference);
                                            state.set_diff_reference(Some(raw));
                                            state.ref_input.cancel();
                                            state.needs_reload = true;
                                        }
                                        Err(e) => {
                                            state.ref_input.error = Some(e.to_string());
                                        }
                                    },
                                    Err(e) => {
                                        state.ref_input.error = Some(e.to_string());
                                    }
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            state.ref_input.pop_char();
                        }
                        KeyCode::Char(c) => {
                            state.ref_input.push_char(c);
                        }
                        _ => {}
                    }
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && state.search_state.is_active() =>
                {
                    match key.code {
                        KeyCode::Esc => {
                            state.search_state.cancel();
                            state.mark_search_dirty();
                        }
                        KeyCode::Enter => {
                            state.search_state.confirm();
                            if state.search_state.has_query() {
                                if let Some(line) = state
                                    .search_state
                                    .jump_to_first_match(state.scroll as usize)
                                {
                                    state.scroll = line.saturating_sub(5) as u16;
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            state.search_state.pop_char();
                            state.mark_search_dirty();
                        }
                        KeyCode::Char(c) => {
                            state.search_state.push_char(c);
                            state.mark_search_dirty();
                        }
                        _ => {}
                    }
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && pr_comment_editor.is_some()
                        && active_modal.is_none() =>
                {
                    if let Some(editor) = pr_comment_editor.as_mut() {
                        match editor.handle_input(key) {
                            PrCommentEditorResult::Continue => {}
                            PrCommentEditorResult::Save => {
                                let body = editor.content();
                                let intent = editor.intent;
                                // Drop the editor before issuing the
                                // network call so the next frame doesn't
                                // re-render a stale modal underneath.
                                pr_comment_editor = None;

                                // Defer the chain (submit → refresh →
                                // count threads → rebuild modal) by one
                                // frame so the loading indicator paints
                                // first. Drained below the next draw.
                                state.loading_message = Some(match intent {
                                    PrCommentIntent::Comment => {
                                        "posting comment to github…".to_string()
                                    }
                                    PrCommentIntent::Approve => {
                                        "submitting approval to github…".to_string()
                                    }
                                });
                                state.pending_gh_action =
                                    Some(PendingGhAction::SubmitPrComment { intent, body });
                            }
                            PrCommentEditorResult::Cancel => {
                                pr_comment_editor = None;
                            }
                        }
                    }
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && annotation_editor.is_some()
                        && active_modal.is_none() =>
                {
                    if let Some(editor) = annotation_editor.as_mut() {
                        match editor.handle_input(key) {
                            AnnotationEditorResult::Continue => {}
                            AnnotationEditorResult::Save => {
                                let content = editor.content();
                                if editor.is_reply() {
                                    if let Some(ref id) = editor.id {
                                        state.add_reply(id, content);
                                    }
                                } else if let Some(ref id) = editor.id {
                                    // Editing existing annotation
                                    state.update_annotation(id, content);
                                } else {
                                    // New annotation
                                    state.add_annotation(
                                        editor.filename.clone(),
                                        editor.target.clone(),
                                        content,
                                        editor.created_at(),
                                    );
                                }
                                annotation_editor = None;
                            }
                            AnnotationEditorResult::Delete => {
                                // Delete only applies to editing an existing root
                                // annotation. Reply mode reaches here only when
                                // content is empty, in which case we drop the
                                // draft without touching the parent thread.
                                if !editor.is_reply() {
                                    if let Some(ref id) = editor.id {
                                        state.remove_annotation(id);
                                    }
                                }
                                annotation_editor = None;
                            }
                            AnnotationEditorResult::Cancel => {
                                annotation_editor = None;
                            }
                        }
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press && active_modal.is_some() => {
                    if let Some(ref mut modal) = active_modal {
                        let term_height = terminal.size()?.height;
                        if let Some(result) = modal.handle_input(key, term_height) {
                            match result {
                                ModalResult::FileSelected(file_index) => {
                                    state.reveal_file(file_index);
                                    state.select_file(file_index);
                                    if let Some(idx) =
                                        state.sidebar_visible_index_for_file(state.current_file)
                                    {
                                        state.sidebar_selected = idx;
                                        let visible_height =
                                            terminal.size()?.height.saturating_sub(5) as usize;
                                        ensure_sidebar_visible(&mut state, visible_height);
                                    }
                                    active_modal = None;
                                }
                                ModalResult::AnnotationJump { annotation_id } => {
                                    if let Some(ann) = state
                                        .get_annotation_by_id(&annotation_id)
                                        .cloned()
                                    {
                                        jump_to_annotation(
                                            &mut state,
                                            &ann,
                                            visible_height,
                                            max_scroll,
                                        );
                                    }
                                    active_modal = None;
                                }
                                ModalResult::AnnotationEdit { annotation_id } => {
                                    if let Some(ann) = state.get_annotation_by_id(&annotation_id) {
                                        let editor = AnnotationEditor::new(
                                            ann.filename.clone(),
                                            ann.target.clone(),
                                        )
                                        .with_existing(
                                            ann.id.clone(),
                                            &ann.root().content,
                                            ann.created_at(),
                                        );
                                        // Jump to the file
                                        let filename = ann.filename.clone();
                                        if let Some(file_index) = state
                                            .file_diffs
                                            .iter()
                                            .position(|f| f.filename == filename)
                                        {
                                            state.select_file(file_index);
                                        }
                                        annotation_editor = Some(editor);
                                    }
                                    active_modal = None;
                                }
                                ModalResult::AnnotationReply { annotation_id } => {
                                    if let Some(ann) = state.get_annotation_by_id(&annotation_id) {
                                        let editor = AnnotationEditor::new(
                                            ann.filename.clone(),
                                            ann.target.clone(),
                                        )
                                        .with_reply(ann.id.clone());
                                        // Jump to the file
                                        let filename = ann.filename.clone();
                                        if let Some(file_index) = state
                                            .file_diffs
                                            .iter()
                                            .position(|f| f.filename == filename)
                                        {
                                            state.select_file(file_index);
                                        }
                                        annotation_editor = Some(editor);
                                    }
                                    active_modal = None;
                                }
                                ModalResult::AnnotationDelete { annotation_id } => {
                                    state.remove_annotation(&annotation_id);
                                    let (items, annotations, snippets) = build_annotation_list(&state);
                                    active_modal = Some(Modal::annotations(
                                        "Annotations",
                                        items,
                                        annotations,
                                        snippets,
                                        source_label(&state),
                                    ));
                                }
                                ModalResult::AnnotationToggleResolved { annotation_id } => {
                                    let new_state = state
                                        .get_annotation_by_id(&annotation_id)
                                        .map(|a| !a.resolved);
                                    if let Some(resolved) = new_state {
                                        state.set_annotation_resolved(
                                            &annotation_id,
                                            resolved,
                                        );
                                    }
                                    // Rebuild the modal contents but keep the
                                    // selection on the same row — the list count
                                    // hasn't changed, only the row's badge.
                                    let prev_selected = match &active_modal {
                                        Some(m) => match &m.content {
                                            ModalContent::Annotations { selected, .. } => {
                                                *selected
                                            }
                                            _ => 0,
                                        },
                                        None => 0,
                                    };
                                    let (items, annotations, snippets) = build_annotation_list(&state);
                                    let mut modal = Modal::annotations(
                                        "Annotations",
                                        items,
                                        annotations,
                                        snippets,
                                        source_label(&state),
                                    );
                                    if let ModalContent::Annotations {
                                        selected, items, ..
                                    } = &mut modal.content
                                    {
                                        *selected = prev_selected.min(items.len().saturating_sub(1));
                                    }
                                    active_modal = Some(modal);
                                }
                                ModalResult::AnnotationSourceSwitch => {
                                    // Defer the (blocking) switch by one
                                    // frame so the loading indicator is
                                    // visible while `gh` runs. The drainer
                                    // below this match block executes the
                                    // call and rebuilds the modal.
                                    let target = state.annotation_source.next();
                                    state.loading_message =
                                        Some("fetching from github…".to_string());
                                    state.pending_gh_action =
                                        Some(PendingGhAction::SwitchAnnotationSource(target));
                                }
                                ModalResult::AnnotationCopyAll => {
                                    // Copy all annotations to clipboard
                                    let formatted = state.format_annotations_for_export();
                                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                        let _ = clipboard.set_text(&formatted);
                                    }
                                    active_modal = None;
                                }
                                ModalResult::AnnotationCopyToOther { annotation_id } => {
                                    // Defer the blocking copy by one frame
                                    // so the loading indicator is visible
                                    // while `gh` runs. Drained below.
                                    state.loading_message = Some(match state.annotation_source {
                                        AnnotationSource::Local => {
                                            "pushing annotation to github…".to_string()
                                        }
                                        AnnotationSource::Github => {
                                            "copying annotation to local…".to_string()
                                        }
                                    });
                                    state.pending_gh_action =
                                        Some(PendingGhAction::CopyAnnotationToOther(annotation_id));
                                }
                                ModalResult::AnnotationExport(filename) => {
                                    // Write annotations to file
                                    let formatted = state.format_annotations_for_export();
                                    match std::fs::write(&filename, &formatted) {
                                        Ok(_) => {
                                            active_modal = None;
                                        }
                                        Err(e) => {
                                            // Set error message on the modal
                                            if let Some(ref mut modal) = active_modal {
                                                if let ModalContent::Annotations {
                                                    error_message,
                                                    export_input,
                                                    ..
                                                } = &mut modal.content
                                                {
                                                    *error_message =
                                                        Some(format!("Failed to write: {}", e));
                                                    *export_input = None; // Close input, keep modal open
                                                }
                                            }
                                        }
                                    }
                                }
                                ModalResult::PrComment => {
                                    // Close the PR modal and open the
                                    // comment editor. The slug is read
                                    // from `state.pr_info` (populated by
                                    // the `P` handler above).
                                    let slug = state
                                        .pr_info
                                        .as_ref()
                                        .map(|p| p.slug())
                                        .unwrap_or_else(|| "?".to_string());
                                    pr_comment_editor = Some(PrCommentEditor::new(
                                        PrCommentIntent::Comment,
                                        slug,
                                    ));
                                    active_modal = None;
                                }
                                ModalResult::PrApprove => {
                                    let slug = state
                                        .pr_info
                                        .as_ref()
                                        .map(|p| p.slug())
                                        .unwrap_or_else(|| "?".to_string());
                                    pr_comment_editor = Some(PrCommentEditor::new(
                                        PrCommentIntent::Approve,
                                        slug,
                                    ));
                                    active_modal = None;
                                }
                                ModalResult::Dismissed | ModalResult::Selected(_, _) => {
                                    active_modal = None;
                                }
                            }
                        }
                    }
                }
                Event::Mouse(mouse) if active_modal.is_some() => {
                    if let Some(ref mut modal) = active_modal {
                        let term_height = terminal.size()?.height;
                        modal.handle_mouse(mouse, term_height);
                    }
                }
                Event::Mouse(mouse) if active_modal.is_none() => {
                    let term_size = terminal.size()?;
                    let footer_height = 1u16;
                    let header_height = if state.stacked_mode { 1u16 } else { 0u16 };
                    let sidebar_width = if state.show_sidebar {
                        (term_size.width / 4).clamp(20, 35)
                    } else {
                        0u16
                    };

                    // For new/deleted files the renderer uses a single full-width panel,
                    // so override diff_fullscreen so PanelLayout matches.
                    let effective_fullscreen = if !state.file_diffs.is_empty() {
                        let d = &state.file_diffs[state.current_file];
                        if d.old_content.is_empty() && !d.new_content.is_empty() {
                            DiffFullscreen::NewOnly
                        } else if !d.old_content.is_empty() && d.new_content.is_empty() {
                            DiffFullscreen::OldOnly
                        } else {
                            state.diff_fullscreen
                        }
                    } else {
                        state.diff_fullscreen
                    };

                    match mouse.kind {
                        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                            // Check for stacked mode header arrow clicks
                            if state.stacked_mode && mouse.row < header_height {
                                // Left arrow click (first 4 columns to cover " < ")
                                if mouse.column < 4 && state.current_commit_index > 0 {
                                    let new_index = state.current_commit_index - 1;
                                    navigate_stacked_commit(
                                        &mut state, new_index, &options, backend,
                                    );
                                }
                                // Right arrow click (last 4 columns to cover " > ")
                                else if mouse.column >= term_size.width.saturating_sub(4)
                                    && state.current_commit_index
                                        < state.stacked_commits.len().saturating_sub(1)
                                {
                                    let new_index = state.current_commit_index + 1;
                                    navigate_stacked_commit(
                                        &mut state, new_index, &options, backend,
                                    );
                                }
                            } else if state.show_sidebar
                                && mouse.column < sidebar_width
                                && mouse.row >= header_height
                                && mouse.row < term_size.height.saturating_sub(footer_height)
                            {
                                state.clear_selection(); // Clear selection when clicking sidebar
                                let clicked_row = (mouse.row.saturating_sub(header_height + 1))
                                    as usize
                                    + state.sidebar_scroll;
                                if clicked_row < state.sidebar_visible_len() {
                                    let item = state.sidebar_item_at_visible(clicked_row).cloned();
                                    if let Some(item) = item {
                                        state.sidebar_selected = clicked_row;
                                        match item {
                                            SidebarItem::File { file_index, .. } => {
                                                state.focused_panel = FocusedPanel::DiffView;
                                                state.select_file(file_index);
                                            }
                                            SidebarItem::Directory { path, .. } => {
                                                state.focused_panel = FocusedPanel::Sidebar;
                                                state.toggle_directory(&path);
                                                let visible_height =
                                                    term_size.height.saturating_sub(5) as usize;
                                                if state.sidebar_selected < state.sidebar_scroll {
                                                    state.sidebar_scroll = state.sidebar_selected;
                                                } else if state.sidebar_selected
                                                    >= state.sidebar_scroll + visible_height
                                                {
                                                    state.sidebar_scroll = state
                                                        .sidebar_selected
                                                        .saturating_sub(visible_height)
                                                        + 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if mouse.column >= sidebar_width
                                && mouse.row >= header_height
                                && mouse.row < term_size.height.saturating_sub(footer_height)
                                && !state.file_diffs.is_empty()
                            {
                                state.focused_panel = FocusedPanel::DiffView;
                                // Any new click in the diff area clears the previous selection
                                state.clear_selection();

                                // Calculate layout for selection
                                let layout = PanelLayout::calculate(
                                    term_size.width,
                                    sidebar_width,
                                    state.show_sidebar,
                                    effective_fullscreen,
                                );

                                if let Some(panel) = layout.panel_at_x(mouse.column) {
                                    let is_gutter = layout.is_in_gutter(mouse.column, panel);
                                    let content_start_y = header_height + 1;

                                    // Coordinate calculation accounting for context lines and annotations
                                    if mouse.row >= content_start_y {
                                        let rel_y = (mouse.row - content_start_y) as usize;

                                        // Skip clicks on context lines or file annotation rows
                                        if rel_y < state.content_row_offset {
                                            continue;
                                        }
                                        let content_y = rel_y - state.content_row_offset;
                                        // Adjust for inline annotation overlay gaps
                                        let adjusted_y =
                                            match state.adjust_for_overlay_gaps(content_y) {
                                                Some(y) => y,
                                                None => continue, // Clicked inside an annotation overlay
                                            };
                                        let line = state.scroll as usize + adjusted_y;
                                        let sbs_len = state.side_by_side_ref().len();
                                        if line >= sbs_len {
                                            continue;
                                        }

                                        let panel_x = match panel {
                                            DiffPanelFocus::Old => layout.old_panel_x,
                                            DiffPanelFocus::New => layout.new_panel_x,
                                            DiffPanelFocus::None => 0,
                                        };

                                        let content_offset = layout.content_x_offset(panel);
                                        let rel_x = mouse.column.saturating_sub(panel_x);
                                        let column = if rel_x >= content_offset {
                                            (rel_x - content_offset + state.h_scroll) as usize
                                        } else {
                                            0
                                        };

                                        let mode = if is_gutter {
                                            SelectionMode::Line
                                        } else {
                                            SelectionMode::Character
                                        };
                                        let pos = CursorPosition { line, column };
                                        state.start_selection(panel, pos, mode);
                                    }
                                }
                            } else if mouse.column >= sidebar_width {
                                state.focused_panel = FocusedPanel::DiffView;
                            }
                        }
                        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                            if state.is_dragging && !state.file_diffs.is_empty() {
                                let panel = state.selection.panel;
                                if panel != DiffPanelFocus::None {
                                    let content_start_y = header_height + 1;

                                    if mouse.row >= content_start_y {
                                        let layout = PanelLayout::calculate(
                                            term_size.width,
                                            sidebar_width,
                                            state.show_sidebar,
                                            effective_fullscreen,
                                        );

                                        let rel_y = (mouse.row - content_start_y) as usize;
                                        // Account for context lines and file annotations
                                        let content_y =
                                            rel_y.saturating_sub(state.content_row_offset);
                                        // Adjust for inline annotation overlay gaps (clamped for drag)
                                        let adjusted_y =
                                            state.adjust_for_overlay_gaps_clamped(content_y);
                                        let line = state.scroll as usize + adjusted_y;
                                        // Clamp to valid side_by_side range
                                        let sbs_len = state.side_by_side_ref().len();
                                        let line = line.min(sbs_len.saturating_sub(1));

                                        let panel_x = match panel {
                                            DiffPanelFocus::Old => layout.old_panel_x,
                                            DiffPanelFocus::New => layout.new_panel_x,
                                            DiffPanelFocus::None => 0,
                                        };

                                        let content_offset = layout.content_x_offset(panel);
                                        let rel_x = mouse.column.saturating_sub(panel_x);
                                        let column = if rel_x >= content_offset {
                                            (rel_x - content_offset + state.h_scroll) as usize
                                        } else {
                                            0
                                        };

                                        let pos = CursorPosition { line, column };
                                        state.extend_selection(pos);
                                    }
                                }
                            }
                        }
                        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                            state.end_drag();
                        }
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                            // Coalesce consecutive scroll events to handle fast scrolling.
                            // Non-scroll events are preserved in pending_events queue.
                            let mut scroll_delta: i32 = match mouse.kind {
                                MouseEventKind::ScrollDown => 3,
                                MouseEventKind::ScrollUp => -3,
                                _ => 0,
                            };

                            // Coalesce scroll events, but preserve non-scroll events
                            while let Ok(next) = app_rx.try_recv() {
                                match next {
                                    AppEvent::Input(Event::Mouse(m))
                                        if matches!(m.kind, MouseEventKind::ScrollDown) =>
                                    {
                                        scroll_delta += 3;
                                    }
                                    AppEvent::Input(Event::Mouse(m))
                                        if matches!(m.kind, MouseEventKind::ScrollUp) =>
                                    {
                                        scroll_delta -= 3;
                                    }
                                    other => {
                                        pending_events.push_back(other);
                                        break;
                                    }
                                }
                            }

                            // Apply the accumulated scroll delta
                            let in_sidebar = state.show_sidebar
                                && mouse.column < sidebar_width
                                && mouse.row < term_size.height.saturating_sub(footer_height);
                            let in_diff = mouse.column >= sidebar_width
                                && mouse.row < term_size.height.saturating_sub(footer_height);

                            if in_sidebar {
                                let max_sidebar_scroll =
                                    state.sidebar_visible_len().saturating_sub(1);
                                if scroll_delta > 0 {
                                    state.sidebar_scroll = (state.sidebar_scroll
                                        + scroll_delta as usize)
                                        .min(max_sidebar_scroll);
                                } else {
                                    state.sidebar_scroll = state
                                        .sidebar_scroll
                                        .saturating_sub((-scroll_delta) as usize);
                                }
                            } else if in_diff {
                                if scroll_delta > 0 {
                                    state.scroll =
                                        (state.scroll + scroll_delta as u16).min(max_scroll as u16);
                                } else {
                                    state.scroll =
                                        state.scroll.saturating_sub((-scroll_delta) as u16);
                                }
                            }
                        }
                        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                            // Coalesce consecutive horizontal scroll events
                            let mut h_scroll_delta: i32 = match mouse.kind {
                                MouseEventKind::ScrollRight => 4,
                                MouseEventKind::ScrollLeft => -4,
                                _ => 0,
                            };

                            // Coalesce horizontal scroll events
                            while let Ok(next) = app_rx.try_recv() {
                                match next {
                                    AppEvent::Input(Event::Mouse(m))
                                        if matches!(m.kind, MouseEventKind::ScrollRight) =>
                                    {
                                        h_scroll_delta += 4;
                                    }
                                    AppEvent::Input(Event::Mouse(m))
                                        if matches!(m.kind, MouseEventKind::ScrollLeft) =>
                                    {
                                        h_scroll_delta -= 4;
                                    }
                                    other => {
                                        pending_events.push_back(other);
                                        break;
                                    }
                                }
                            }

                            // Apply the accumulated horizontal scroll delta
                            let in_sidebar = state.show_sidebar
                                && mouse.column < sidebar_width
                                && mouse.row < term_size.height.saturating_sub(footer_height);
                            let in_diff = mouse.column >= sidebar_width
                                && mouse.row < term_size.height.saturating_sub(footer_height);

                            if in_sidebar {
                                if h_scroll_delta > 0 {
                                    state.sidebar_h_scroll = state
                                        .sidebar_h_scroll
                                        .saturating_add(h_scroll_delta as u16);
                                    clamp_sidebar_h_scroll(&mut state, term_size.width);
                                } else {
                                    state.sidebar_h_scroll = state
                                        .sidebar_h_scroll
                                        .saturating_sub((-h_scroll_delta) as u16);
                                }
                            } else if in_diff {
                                if h_scroll_delta > 0 {
                                    state.h_scroll =
                                        state.h_scroll.saturating_add(h_scroll_delta as u16);
                                    clamp_h_scroll(&mut state, term_size.width);
                                } else {
                                    state.h_scroll =
                                        state.h_scroll.saturating_sub((-h_scroll_delta) as u16);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press && active_modal.is_none() => {
                    if key.code != KeyCode::Char('g') {
                        state.pending_key = PendingKey::None;
                    }
                    state.show_selection_tooltip = false;
                    if state.annotation_select.is_some()
                        && !allowed_in_annotation_select_mode(&key)
                    {
                        // Stay in annotation-select mode; swallow the key.
                        continue;
                    }
                    match key.code {
                        KeyCode::Esc if state.annotation_select.is_some() => {
                            // Annotation-select mode: Esc exits the mode and
                            // short-circuits before any other Esc handling.
                            state.annotation_select = None;
                        }
                        KeyCode::Esc | KeyCode::Char('c')
                            if (key.code == KeyCode::Esc
                                || key.modifiers.contains(KeyModifiers::CONTROL))
                                && state.selection.is_active() =>
                        {
                            // First priority: clear selection
                            state.clear_selection();
                        }
                        KeyCode::Esc | KeyCode::Char('c')
                            if (key.code == KeyCode::Esc
                                || key.modifiers.contains(KeyModifiers::CONTROL))
                                && state.search_state.has_query() =>
                        {
                            state.search_state.clear();
                            state.mark_search_dirty();
                        }
                        KeyCode::Char('q') | KeyCode::Esc => break 'main,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break 'main
                        }
                        KeyCode::Char('1') => {
                            state.focused_panel = FocusedPanel::Sidebar;
                            state.show_sidebar = true;
                            if !matches!(
                                state.sidebar_item_at_visible(state.sidebar_selected),
                                Some(SidebarItem::File { .. })
                            ) {
                                if let Some(idx) = state.sidebar_visible.iter().position(|idx| {
                                    matches!(state.sidebar_items[*idx], SidebarItem::File { .. })
                                }) {
                                    state.sidebar_selected = idx;
                                }
                            }
                        }
                        KeyCode::Char('2') => {
                            state.focused_panel = FocusedPanel::DiffView;
                        }
                        KeyCode::Tab => {
                            state.show_sidebar = !state.show_sidebar;
                            if !state.show_sidebar {
                                state.focused_panel = FocusedPanel::DiffView;
                            }
                        }
                        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !state.file_diffs.is_empty() {
                                let mut next = state.sidebar_selected + 1;
                                while next < state.sidebar_visible_len() {
                                    if let Some(SidebarItem::File { file_index, .. }) =
                                        state.sidebar_item_at_visible(next).cloned()
                                    {
                                        state.sidebar_selected = next;
                                        state.select_file(file_index);
                                        let visible_height =
                                            terminal.size()?.height.saturating_sub(5) as usize;
                                        ensure_sidebar_visible(&mut state, visible_height);
                                        break;
                                    }
                                    next += 1;
                                }
                            }
                        }
                        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !state.file_diffs.is_empty() && state.sidebar_selected > 0 {
                                let mut prev = state.sidebar_selected - 1;
                                loop {
                                    if let Some(SidebarItem::File { file_index, .. }) =
                                        state.sidebar_item_at_visible(prev).cloned()
                                    {
                                        state.sidebar_selected = prev;
                                        state.select_file(file_index);
                                        ensure_sidebar_visible(&mut state, usize::MAX);
                                        break;
                                    }
                                    if prev == 0 {
                                        break;
                                    }
                                    prev -= 1;
                                }
                            }
                        }
                        // Stacked mode: navigate to next commit
                        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if state.stacked_mode
                                && state.current_commit_index < state.stacked_commits.len() - 1
                            {
                                let new_index = state.current_commit_index + 1;
                                navigate_stacked_commit(&mut state, new_index, &options, backend);
                            }
                        }
                        // Stacked mode: navigate to previous commit
                        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if state.stacked_mode && state.current_commit_index > 0 {
                                let new_index = state.current_commit_index - 1;
                                navigate_stacked_commit(&mut state, new_index, &options, backend);
                            }
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let half_screen = (visible_height / 2) as u16;
                            state.scroll = (state.scroll + half_screen).min(max_scroll as u16);
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let half_screen = (visible_height / 2) as u16;
                            state.scroll = state.scroll.saturating_sub(half_screen);
                        }
                        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !state.file_diffs.is_empty() {
                                let items: Vec<FilePickerItem> = state
                                    .file_diffs
                                    .iter()
                                    .enumerate()
                                    .map(|(i, diff)| {
                                        let status = match diff.status {
                                            FileStatus::Added => ModalFileStatus::Added,
                                            FileStatus::Modified => ModalFileStatus::Modified,
                                            FileStatus::Deleted => ModalFileStatus::Deleted,
                                        };
                                        FilePickerItem {
                                            name: diff.filename.clone(),
                                            file_index: i,
                                            status,
                                            viewed: state.viewed_files.contains(&i),
                                        }
                                    })
                                    .collect();
                                active_modal = Some(Modal::file_picker("Find File", items));
                            }
                        }
                        KeyCode::Char('}') => {
                            if !state.file_diffs.is_empty() {
                                let diff = &state.file_diffs[state.current_file];
                                if !diff.new_content.is_empty() {
                                    state.diff_fullscreen = match state.diff_fullscreen {
                                        DiffFullscreen::NewOnly => DiffFullscreen::None,
                                        _ => DiffFullscreen::NewOnly,
                                    };
                                    state.mark_search_dirty();
                                }
                            }
                        }
                        KeyCode::Char('{') => {
                            if !state.file_diffs.is_empty() {
                                let diff = &state.file_diffs[state.current_file];
                                if !diff.old_content.is_empty() {
                                    state.diff_fullscreen = match state.diff_fullscreen {
                                        DiffFullscreen::OldOnly => DiffFullscreen::None,
                                        _ => DiffFullscreen::OldOnly,
                                    };
                                    state.mark_search_dirty();
                                }
                            }
                        }
                        KeyCode::Char('=') => {
                            state.diff_fullscreen = DiffFullscreen::None;
                            state.mark_search_dirty();
                        }
                        KeyCode::Down | KeyCode::Char('j')
                            if state.annotation_select.is_some() =>
                        {
                            if let Some(from) = state
                                .annotation_select
                                .as_ref()
                                .and_then(|id| state.get_annotation_by_id(id))
                                .and_then(|a| annotation_sort_key(a, &state.file_diffs))
                            {
                                if let Some(next) = adjacent_annotation_to(&state, from, true) {
                                    state.annotation_select = Some(next.id.clone());
                                    jump_to_annotation(
                                        &mut state,
                                        &next,
                                        visible_height,
                                        max_scroll,
                                    );
                                }
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k')
                            if state.annotation_select.is_some() =>
                        {
                            if let Some(from) = state
                                .annotation_select
                                .as_ref()
                                .and_then(|id| state.get_annotation_by_id(id))
                                .and_then(|a| annotation_sort_key(a, &state.file_diffs))
                            {
                                if let Some(prev) = adjacent_annotation_to(&state, from, false) {
                                    state.annotation_select = Some(prev.id.clone());
                                    jump_to_annotation(
                                        &mut state,
                                        &prev,
                                        visible_height,
                                        max_scroll,
                                    );
                                }
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j')
                            if state.focused_panel == FocusedPanel::DiffView
                                && state.hunk_line_selection.is_some() =>
                        {
                            if let Some(line) = state.move_hunk_line_selection(1) {
                                state.scroll = adjust_scroll_to_line(
                                    line,
                                    state.scroll,
                                    visible_height,
                                    max_scroll,
                                );
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k')
                            if state.focused_panel == FocusedPanel::DiffView
                                && state.hunk_line_selection.is_some() =>
                        {
                            if let Some(line) = state.move_hunk_line_selection(-1) {
                                state.scroll = adjust_scroll_to_line(
                                    line,
                                    state.scroll,
                                    visible_height,
                                    max_scroll,
                                );
                            }
                        }
                        KeyCode::Down
                            if state.search_state.has_query()
                                && state.focused_panel == FocusedPanel::DiffView =>
                        {
                            if let Some(line) = state.search_state.find_next() {
                                state.scroll = adjust_scroll_to_line(
                                    line,
                                    state.scroll,
                                    visible_height,
                                    max_scroll,
                                );
                            }
                        }
                        KeyCode::Up
                            if state.search_state.has_query()
                                && state.focused_panel == FocusedPanel::DiffView =>
                        {
                            if let Some(line) = state.search_state.find_prev() {
                                state.scroll = adjust_scroll_to_line(
                                    line,
                                    state.scroll,
                                    visible_height,
                                    max_scroll,
                                );
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if state.focused_panel == FocusedPanel::Sidebar {
                                if state.sidebar_selected + 1 < state.sidebar_visible_len() {
                                    state.sidebar_selected += 1;
                                }
                                let visible_height =
                                    terminal.size()?.height.saturating_sub(5) as usize;
                                ensure_sidebar_visible(&mut state, visible_height);
                            } else {
                                state.scroll = (state.scroll + 1).min(max_scroll as u16);
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if state.focused_panel == FocusedPanel::Sidebar {
                                if state.sidebar_selected > 0 {
                                    state.sidebar_selected =
                                        state.sidebar_selected.saturating_sub(1);
                                }
                                ensure_sidebar_visible(&mut state, usize::MAX);
                            } else {
                                state.scroll = state.scroll.saturating_sub(1);
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            if state.focused_panel == FocusedPanel::DiffView {
                                state.h_scroll = state.h_scroll.saturating_sub(4);
                            } else if state.focused_panel == FocusedPanel::Sidebar {
                                state.sidebar_h_scroll = state.sidebar_h_scroll.saturating_sub(4);
                            }
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            let term_width = terminal.size()?.width;
                            if state.focused_panel == FocusedPanel::DiffView {
                                state.h_scroll = state.h_scroll.saturating_add(4);
                                clamp_h_scroll(&mut state, term_width);
                            } else if state.focused_panel == FocusedPanel::Sidebar {
                                state.sidebar_h_scroll = state.sidebar_h_scroll.saturating_add(4);
                                clamp_sidebar_h_scroll(&mut state, term_width);
                            }
                        }
                        KeyCode::Enter => {
                            if state.focused_panel == FocusedPanel::Sidebar
                                && state.sidebar_selected < state.sidebar_visible_len()
                            {
                                if let Some(item) = state
                                    .sidebar_item_at_visible(state.sidebar_selected)
                                    .cloned()
                                {
                                    match item {
                                        SidebarItem::File { file_index, .. } => {
                                            state.select_file(file_index);
                                            state.focused_panel = FocusedPanel::DiffView;
                                        }
                                        SidebarItem::Directory { path, .. } => {
                                            state.toggle_directory(&path);
                                            let visible_height =
                                                terminal.size()?.height.saturating_sub(5) as usize;
                                            if state.sidebar_selected < state.sidebar_scroll {
                                                state.sidebar_scroll = state.sidebar_selected;
                                            } else if state.sidebar_selected
                                                >= state.sidebar_scroll + visible_height
                                            {
                                                state.sidebar_scroll = state
                                                    .sidebar_selected
                                                    .saturating_sub(visible_height)
                                                    + 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char(' ') => {
                            if state.focused_panel == FocusedPanel::DiffView
                                && state.hunk_line_selection.is_some()
                            {
                                state.begin_hunk_line_range();
                            } else if state.focused_panel == FocusedPanel::Sidebar
                                && state.sidebar_selected < state.sidebar_visible_len()
                            {
                                let selected = state
                                    .sidebar_item_at_visible(state.sidebar_selected)
                                    .cloned();
                                if let Some(selected) = selected {
                                    match selected {
                                        SidebarItem::File { file_index, .. } => {
                                            let file_idx = file_index;
                                            let was_viewed = state.viewed_files.contains(&file_idx);

                                            if was_viewed {
                                                state.viewed_files.remove(&file_idx);
                                            } else {
                                                state.viewed_files.insert(file_idx);
                                            }
                                        }
                                        SidebarItem::Directory { path, .. } => {
                                            let dir_prefix = format!("{}/", path);
                                            let child_indices: Vec<usize> = state
                                                .sidebar_items
                                                .iter()
                                                .filter_map(|item| {
                                                    if let SidebarItem::File {
                                                        path: file_path,
                                                        file_index,
                                                        ..
                                                    } = item
                                                    {
                                                        if file_path.starts_with(&dir_prefix) {
                                                            return Some(*file_index);
                                                        }
                                                    }
                                                    None
                                                })
                                                .collect();

                                            let all_viewed = child_indices
                                                .iter()
                                                .all(|i| state.viewed_files.contains(i));

                                            if all_viewed {
                                                for idx in &child_indices {
                                                    state.viewed_files.remove(idx);
                                                }
                                            } else {
                                                for idx in &child_indices {
                                                    state.viewed_files.insert(*idx);
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if state.focused_panel == FocusedPanel::DiffView {
                                let current_file = state.current_file;
                                let was_viewed = state.viewed_files.contains(&current_file);

                                if was_viewed {
                                    state.viewed_files.remove(&current_file);
                                } else {
                                    state.viewed_files.insert(current_file);
                                    // Move to next unviewed file
                                    let mut next_file: Option<(usize, usize)> = None;
                                    for (visible_idx, item_idx) in state
                                        .sidebar_visible
                                        .iter()
                                        .enumerate()
                                        .skip(state.sidebar_selected + 1)
                                    {
                                        if let SidebarItem::File { file_index, .. } =
                                            &state.sidebar_items[*item_idx]
                                        {
                                            if !state.viewed_files.contains(file_index) {
                                                next_file = Some((visible_idx, *file_index));
                                                break;
                                            }
                                        }
                                    }
                                    if next_file.is_none() {
                                        for (visible_idx, item_idx) in state
                                            .sidebar_visible
                                            .iter()
                                            .enumerate()
                                            .take(state.sidebar_selected)
                                        {
                                            if let SidebarItem::File { file_index, .. } =
                                                &state.sidebar_items[*item_idx]
                                            {
                                                if !state.viewed_files.contains(file_index) {
                                                    next_file = Some((visible_idx, *file_index));
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    if let Some((idx, file_idx)) = next_file {
                                        state.sidebar_selected = idx;
                                        state.select_file(file_idx);
                                        let visible_height =
                                            terminal.size()?.height.saturating_sub(5) as usize;
                                        ensure_sidebar_visible(&mut state, visible_height);
                                    }
                                }
                            }
                        }
                        KeyCode::PageDown => {
                            state.scroll = (state.scroll + 20).min(max_scroll as u16);
                        }
                        KeyCode::PageUp => {
                            state.scroll = state.scroll.saturating_sub(20);
                        }
                        KeyCode::Char(']') => {
                            if !state.file_diffs.is_empty() {
                                state.clear_selection(); // Clear selection on hunk navigation
                                let hunks = state.get_hunks().to_vec();
                                let current_hunk = state.focused_hunk.unwrap_or(0);
                                let next_hunk = if state.focused_hunk.is_none() {
                                    hunks
                                        .iter()
                                        .position(|&h| h > state.scroll as usize + 5)
                                        .unwrap_or(0)
                                } else {
                                    (current_hunk + 1).min(hunks.len().saturating_sub(1))
                                };
                                if !hunks.is_empty() {
                                    state.focused_hunk = Some(next_hunk);
                                    state.scroll = adjust_scroll_for_hunk(
                                        hunks[next_hunk],
                                        state.scroll,
                                        visible_height,
                                        max_scroll,
                                    );
                                }
                            }
                        }
                        KeyCode::Char('[') => {
                            if !state.file_diffs.is_empty() {
                                state.clear_selection(); // Clear selection on hunk navigation
                                let hunks = state.get_hunks().to_vec();
                                let current_hunk = state.focused_hunk.unwrap_or(hunks.len());
                                let prev_hunk = if state.focused_hunk.is_none() {
                                    hunks
                                        .iter()
                                        .rposition(|&h| (h as u16) < state.scroll.saturating_sub(5))
                                        .unwrap_or(hunks.len().saturating_sub(1))
                                } else {
                                    current_hunk.saturating_sub(1)
                                };
                                if !hunks.is_empty() {
                                    state.focused_hunk = Some(prev_hunk);
                                    state.scroll = adjust_scroll_for_hunk(
                                        hunks[prev_hunk],
                                        state.scroll,
                                        visible_height,
                                        max_scroll,
                                    );
                                }
                            }
                        }
                        KeyCode::Char('v') => {
                            if state.focused_panel == FocusedPanel::DiffView
                                && !state.file_diffs.is_empty()
                            {
                                if state.hunk_line_selection.is_some() {
                                    state.clear_selection();
                                } else if let Some(hunk_index) = state.focused_hunk {
                                    let preferred_panel = {
                                        let diff = &state.file_diffs[state.current_file];
                                        preferred_hunk_annotation_panel(diff, state.diff_fullscreen)
                                    };
                                    let fallback_panel = alternate_diff_panel(preferred_panel);
                                    let selected_line = state
                                        .start_hunk_line_selection(hunk_index, preferred_panel)
                                        .or_else(|| {
                                            state.start_hunk_line_selection(
                                                hunk_index,
                                                fallback_panel,
                                            )
                                        });

                                    if let Some(line) = selected_line {
                                        state.scroll = adjust_scroll_to_line(
                                            line,
                                            state.scroll,
                                            visible_height,
                                            max_scroll,
                                        );
                                    }
                                }
                            }
                        }
                        KeyCode::Char('m') => {
                            if state.focused_panel == FocusedPanel::DiffView
                                && !state.file_diffs.is_empty()
                            {
                                let hunks = state.get_hunks().to_vec();
                                if let Some(hunk_idx) = state.focused_hunk {
                                    if hunk_idx < hunks.len() {
                                        let filename =
                                            state.file_diffs[state.current_file].filename.clone();
                                        let entry = state.viewed_hunks.entry(filename).or_default();
                                        let was_viewed = entry.contains(&hunk_idx);
                                        if was_viewed {
                                            entry.remove(&hunk_idx);
                                        } else {
                                            entry.insert(hunk_idx);
                                        }

                                        // On mark-viewed (not unmark), advance to next hunk.
                                        if !was_viewed && hunk_idx + 1 < hunks.len() {
                                            let next = hunk_idx + 1;
                                            state.focused_hunk = Some(next);
                                            state.scroll = adjust_scroll_for_hunk(
                                                hunks[next],
                                                state.scroll,
                                                visible_height,
                                                max_scroll,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char('a') => {
                            // Enter annotation-select mode: pick the
                            // annotation closest to the current viewport
                            // and jump to it. No-op if nothing to walk.
                            if state.annotation_select.is_none() && !state.annotations.is_empty() {
                                let anchor = current_anchor_line(&mut state);
                                let from = (state.current_file, anchor);
                                if let Some(ann) = nearest_annotation_to(&state, from) {
                                    state.clear_selection();
                                    state.annotation_select = Some(ann.id.clone());
                                    jump_to_annotation(
                                        &mut state,
                                        &ann,
                                        visible_height,
                                        max_scroll,
                                    );
                                }
                            }
                        }
                        KeyCode::Char('i') => {
                            if !state.file_diffs.is_empty() {
                                let file_index = state.current_file;
                                let diff = &state.file_diffs[file_index];
                                let filename = diff.filename.clone();

                                if state.selection.is_active()
                                    && !matches!(state.selection.mode, SelectionMode::None)
                                {
                                    // Tier 1: Active selection (line or character) → line-range annotation
                                    // Both line-mode and character-mode selections create full-line annotations
                                    let panel = state.selection.panel;
                                    let sel_start =
                                        state.selection.anchor.line.min(state.selection.head.line);
                                    let sel_end =
                                        state.selection.anchor.line.max(state.selection.head.line);

                                    state.ensure_cache();
                                    let sbs = state.side_by_side_ref();

                                    // Resolve side_by_side indices to file line numbers
                                    let mut start_line: Option<usize> = None;
                                    let mut end_line: Option<usize> = None;
                                    for idx in sel_start..=sel_end {
                                        if let Some(dl) = sbs.get(idx) {
                                            if let Some(n) = dl.line_number(panel) {
                                                if start_line.is_none() {
                                                    start_line = Some(n);
                                                }
                                                end_line = Some(n);
                                            }
                                        }
                                    }

                                    if let (Some(start), Some(end)) = (start_line, end_line) {
                                        let target = super::state::AnnotationTarget::LineRange {
                                            panel,
                                            start_line: start,
                                            end_line: end,
                                        };
                                        let editor = AnnotationEditor::new(filename, target);
                                        annotation_editor = Some(editor);
                                    }
                                    state.clear_selection();
                                } else if let Some(hunk_index) = state.focused_hunk {
                                    // Tier 2: Focused hunk → line-range annotation for the hunk
                                    let is_deleted =
                                        !diff.old_content.is_empty() && diff.new_content.is_empty();
                                    let hunk_panel = if is_deleted {
                                        DiffPanelFocus::Old
                                    } else {
                                        DiffPanelFocus::New
                                    };

                                    state.ensure_cache();
                                    let sbs = state.side_by_side_ref();
                                    let hunks = state.hunks_ref();
                                    let hunk_start = hunks.get(hunk_index).copied().unwrap_or(0);
                                    let next_hunk_start =
                                        hunks.get(hunk_index + 1).copied().unwrap_or(sbs.len());

                                    let mut actual_hunk_end = hunk_start;
                                    for i in hunk_start..next_hunk_start {
                                        if let Some(dl) = sbs.get(i) {
                                            if !matches!(dl.change_type, ChangeType::Equal) {
                                                actual_hunk_end = i;
                                            }
                                        }
                                    }

                                    let line_num = |dl: &super::types::DiffLine| {
                                        dl.line_number(hunk_panel)
                                            .or_else(|| dl.line_number(DiffPanelFocus::Old))
                                    };

                                    let start_line =
                                        sbs.get(hunk_start).and_then(line_num).unwrap_or(1);
                                    let end_line = sbs
                                        .get(actual_hunk_end)
                                        .and_then(line_num)
                                        .unwrap_or(start_line);

                                    let target = super::state::AnnotationTarget::LineRange {
                                        panel: hunk_panel,
                                        start_line,
                                        end_line,
                                    };
                                    let editor = AnnotationEditor::new(filename, target);
                                    annotation_editor = Some(editor);
                                } else {
                                    // Tier 3: No selection, no hunk → file-level annotation
                                    let target = super::state::AnnotationTarget::File;
                                    let editor = AnnotationEditor::new(filename, target);
                                    annotation_editor = Some(editor);
                                }
                            }
                        }
                        KeyCode::Char('I') => {
                            // Open annotations menu. Always open even when
                            // the list is empty so the user can reach the
                            // source picker to switch to GitHub.
                            let (items, annotations, snippets) = build_annotation_list(&state);
                            active_modal = Some(Modal::annotations(
                                "Annotations",
                                items,
                                annotations,
                                snippets,
                                source_label(&state),
                            ));
                        }
                        KeyCode::Char('P') => {
                            // Defer the (blocking) PR fetch by one frame so
                            // the loading indicator is visible while `gh`
                            // runs. The drainer below the draw block does
                            // the refresh + thread count and opens the
                            // modal.
                            state.loading_message =
                                Some("fetching PR from github…".to_string());
                            state.pending_gh_action = Some(PendingGhAction::ShowPrDescription);
                        }
                        KeyCode::Char('R') if state.annotation_select.is_some() => {
                            if let Some(ann) = state
                                .annotation_select
                                .as_ref()
                                .and_then(|id| state.get_annotation_by_id(id))
                                .cloned()
                            {
                                let editor = AnnotationEditor::new(
                                    ann.filename.clone(),
                                    ann.target.clone(),
                                )
                                .with_reply(ann.id.clone());
                                annotation_editor = Some(editor);
                            }
                        }
                        KeyCode::Char('X') if state.annotation_select.is_some() => {
                            if let Some(ann) = state
                                .annotation_select
                                .as_ref()
                                .and_then(|id| state.get_annotation_by_id(id))
                                .cloned()
                            {
                                state.set_annotation_resolved(&ann.id, !ann.resolved);
                            }
                        }
                        KeyCode::Char('r') => {
                            state.needs_reload = true;
                        }
                        KeyCode::Char('b') => {
                            let initial = state.diff_reference.clone().unwrap_or_default();
                            state.ref_input.start(&initial);
                        }
                        KeyCode::Char('y') => {
                            if !state.file_diffs.is_empty() {
                                // If selection is active, copy selected text
                                if state.selection.is_active() {
                                    state.ensure_cache();
                                    let side_by_side = state.side_by_side_ref();
                                    if let Some(text) =
                                        extract_selected_text(&state.selection, side_by_side)
                                    {
                                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                            let _ = clipboard.set_text(&text);
                                        }
                                    }
                                    state.clear_selection();
                                } else {
                                    // Otherwise copy filename
                                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                        let _ = clipboard.set_text(
                                            &state.file_diffs[state.current_file].filename,
                                        );
                                    }
                                }
                            }
                        }
                        KeyCode::Char('e') => {
                            if !state.file_diffs.is_empty() {
                                // Stop the background input reader so it
                                // doesn't fight the child editor for stdin.
                                input_running.store(false, Ordering::Relaxed);
                                let _ = input_handle.join();

                                io::stdout().execute(DisableMouseCapture)?;
                                io::stdout().execute(LeaveAlternateScreen)?;
                                disable_raw_mode()?;

                                let editor =
                                    std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
                                let filename =
                                    state.file_diffs[state.current_file].filename.clone();

                                let line_arg = if let Some(hunk_idx) = state.focused_hunk {
                                    state.ensure_cache();
                                    let side_by_side = state.side_by_side_ref();
                                    let hunks = state.hunks_ref();
                                    if let Some(&hunk_start) = hunks.get(hunk_idx) {
                                        side_by_side.get(hunk_start).and_then(|dl| {
                                            dl.new_line
                                                .as_ref()
                                                .map(|(n, _)| *n)
                                                .or(dl.old_line.as_ref().map(|(n, _)| *n))
                                        })
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                let status = if let Some(line) = line_arg {
                                    std::process::Command::new(&editor)
                                        .arg(format!("+{}", line))
                                        .arg(filename)
                                        .status()
                                } else {
                                    std::process::Command::new(&editor).arg(filename).status()
                                };
                                let _ = status;

                                enable_raw_mode()?;
                                io::stdout().execute(EnterAlternateScreen)?;
                                io::stdout().execute(EnableMouseCapture)?;
                                terminal.clear()?;

                                // Restart the background input reader.
                                let (r, h) = spawn_input_thread(app_tx.clone());
                                input_running = r;
                                input_handle = h;
                            }
                        }
                        KeyCode::Char('g') => {
                            if state.pending_key == PendingKey::G {
                                state.scroll = 0;
                                state.pending_key = PendingKey::None;
                            } else {
                                state.pending_key = PendingKey::G;
                            }
                        }
                        KeyCode::Char('G') => {
                            state.scroll = max_scroll as u16;
                        }
                        KeyCode::Char('/') | KeyCode::Char('f')
                            if key.code == KeyCode::Char('/')
                                || key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            state.search_state.start_forward();
                            state.mark_search_dirty();
                        }
                        KeyCode::Char('n') if state.search_state.has_query() => {
                            if let Some(line) = state.search_state.find_next() {
                                state.scroll = adjust_scroll_to_line(
                                    line,
                                    state.scroll,
                                    visible_height,
                                    max_scroll,
                                );
                            }
                        }
                        KeyCode::Char('N') if state.search_state.has_query() => {
                            if let Some(line) = state.search_state.find_prev() {
                                state.scroll = adjust_scroll_to_line(
                                    line,
                                    state.scroll,
                                    visible_height,
                                    max_scroll,
                                );
                            }
                        }
                        KeyCode::Char('?') => {
                            active_modal = Some(Modal::keybindings(
                                "Keybindings",
                                vec![
                                    KeyBindSection {
                                        title: "Global",
                                        bindings: vec![
                                            KeyBind {
                                                key: "q / esc",
                                                description: "Quit",
                                            },
                                            KeyBind {
                                                key: "tab",
                                                description: "Toggle sidebar",
                                            },
                                            KeyBind {
                                                key: "1 / 2",
                                                description: "Focus sidebar / diff",
                                            },
                                            KeyBind {
                                                key: "ctrl+j / ctrl+k",
                                                description: "Next / previous file",
                                            },
                                            KeyBind {
                                                key: "ctrl+d / ctrl+u",
                                                description: "Scroll half page down / up",
                                            },
                                            KeyBind {
                                                key: "ctrl+p",
                                                description: "Open file picker",
                                            },
                                            KeyBind {
                                                key: "r",
                                                description: "Refresh diff / PR",
                                            },
                                            KeyBind {
                                                key: "b",
                                                description: "Change diff base (empty=unstaged, --cached=staged, <ref>)",
                                            },
                                            KeyBind {
                                                key: "y",
                                                description: "Copy current filename",
                                            },
                                            KeyBind {
                                                key: "e",
                                                description: "Edit file (at hunk line if focused)",
                                            },
                                            KeyBind {
                                                key: "ctrl+l / ctrl+h",
                                                description: "Next / prev commit (stacked)",
                                            },
                                            KeyBind {
                                                key: "P",
                                                description: "Show PR description (current branch)",
                                            },
                                            KeyBind {
                                                key: "?",
                                                description: "Show keybindings",
                                            },
                                        ],
                                    },
                                    KeyBindSection {
                                        title: "Sidebar",
                                        bindings: vec![
                                            KeyBind {
                                                key: "j/k or up/down",
                                                description: "Navigate files",
                                            },
                                            KeyBind {
                                                key: "h/l or left/right",
                                                description: "Scroll horizontally",
                                            },
                                            KeyBind {
                                                key: "enter",
                                                description:
                                                    "Open file in diff view / toggle directory",
                                            },
                                            KeyBind {
                                                key: "space",
                                                description: "Toggle file as viewed",
                                            },
                                        ],
                                    },
                                    KeyBindSection {
                                        title: "Diff View",
                                        bindings: vec![
                                            KeyBind {
                                                key: "j/k or up/down",
                                                description: "Scroll vertically",
                                            },
                                            KeyBind {
                                                key: "h/l or left/right",
                                                description: "Scroll horizontally",
                                            },
                                            KeyBind {
                                                key: "gg / G",
                                                description: "Scroll to top / bottom",
                                            },
                                            KeyBind {
                                                key: "[ / ]",
                                                description: "Focus prev / next hunk",
                                            },
                                            KeyBind {
                                                key: "v",
                                                description: "Select line in focused hunk",
                                            },
                                            KeyBind {
                                                key: "pageup / pagedown",
                                                description: "Scroll by page",
                                            },
                                            KeyBind {
                                                key: "space",
                                                description: "Mark viewed & next file",
                                            },
                                            KeyBind {
                                                key: "m",
                                                description: "Mark hunk viewed & next hunk",
                                            },
                                            KeyBind {
                                                key: "}",
                                                description: "Toggle new panel fullscreen",
                                            },
                                            KeyBind {
                                                key: "{",
                                                description: "Toggle old panel fullscreen",
                                            },
                                            KeyBind {
                                                key: "=",
                                                description: "Reset fullscreen to side-by-side",
                                            },
                                        ],
                                    },
                                    KeyBindSection {
                                        title: "Search",
                                        bindings: vec![
                                            KeyBind {
                                                key: "/ or ctrl+f",
                                                description: "Start search",
                                            },
                                            KeyBind {
                                                key: "n or down",
                                                description: "Next match",
                                            },
                                            KeyBind {
                                                key: "N or up",
                                                description: "Previous match",
                                            },
                                            KeyBind {
                                                key: "ctrl+c or esc",
                                                description: "Cancel search",
                                            },
                                        ],
                                    },
                                    KeyBindSection {
                                        title: "Selection & Annotations",
                                        bindings: vec![
                                            KeyBind {
                                                key: "y",
                                                description: "Copy selection (or filename)",
                                            },
                                            KeyBind {
                                                key: "space",
                                                description: "Start range for hunk line selection",
                                            },
                                            KeyBind {
                                                key: "i",
                                                description: "Annotate selection / hunk / file",
                                            },
                                            KeyBind {
                                                key: "I",
                                                description: "View all annotations",
                                            },
                                            KeyBind {
                                                key: "a",
                                                description: "Enter annotation-select mode (j/k walk, esc exits)",
                                            },
                                            KeyBind {
                                                key: "R",
                                                description: "Reply to selected annotation (in select mode)",
                                            },
                                            KeyBind {
                                                key: "X",
                                                description: "Toggle resolved on selected annotation (in select mode)",
                                            },
                                            KeyBind {
                                                key: "r / x (in I)",
                                                description: "Reply / toggle resolved",
                                            },
                                        ],
                                    },
                                ],
                            ));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    io::stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}
