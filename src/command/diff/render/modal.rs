use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    prelude::*,
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};

use crate::command::diff::annotation::{AnnotationEditor, AnnotationEditorResult};
use crate::command::diff::highlight::highlight_line_spans;
use crate::command::diff::state::{Annotation, AnnotationId};
use crate::command::diff::theme;

/// Upper bound on the wrap column for the PR description body. We never
/// exceed this even on ultra-wide terminals — long markdown lines hurt
/// readability past ~95 columns.
const PR_DESC_MAX_WRAP_WIDTH: usize = 95;
/// Lower bound on the wrap column. On very narrow terminals the modal
/// will already be clamped to `terminal_width - 4`; anything below this
/// is unusable and we wrap at this minimum anyway.
const PR_DESC_MIN_WRAP_WIDTH: usize = 40;
/// Fraction of the terminal width the modal occupies horizontally.
const PR_DESC_MODAL_PCT: u16 = 80;

#[derive(Clone)]
pub struct KeyBind {
    pub key: &'static str,
    pub description: &'static str,
}

#[derive(Clone)]
pub struct KeyBindSection {
    pub title: &'static str,
    pub bindings: Vec<KeyBind>,
}

#[derive(Clone)]
pub struct FilePickerItem {
    pub name: String,
    pub file_index: usize,
    pub status: FileStatus,
    pub viewed: bool,
}

/// One row in the revset picker. `revset` is `None` for the synthetic
/// "working copy" row, which clears the diff base instead of setting one.
pub struct RevsetPickerItem {
    pub lines: Option<Vec<Line<'static>>>,
    pub label: String,
    pub revset: Option<String>,
}

#[derive(Clone, Copy)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

pub enum ModalContent {
    #[allow(dead_code)]
    Info { title: String, message: String },
    #[allow(dead_code)]
    Select {
        title: String,
        items: Vec<String>,
        selected: usize,
    },
    KeyBindings {
        title: String,
        sections: Vec<KeyBindSection>,
        scroll: u16,
        content_height: u16,
    },
    FilePicker {
        title: String,
        items: Vec<FilePickerItem>,
        filtered_indices: Vec<usize>,
        query: String,
        selected: usize,
    },
    RevsetPicker {
        title: String,
        items: Vec<RevsetPickerItem>,
        filtered_indices: Vec<usize>,
        query: String,
        selected: usize,
        /// Resolution failure from the last `enter`. Keeps the modal open so
        /// the user can correct the query instead of losing it.
        error: Option<String>,
    },
    PrDescription {
        /// PR slug + title rendered as the modal title bar.
        title: String,
        /// Pre-styled status header lines (state, mergeability, checks).
        /// Empty when no status was supplied.
        status_lines: Vec<Line<'static>>,
        /// Body pre-wrapped to a fixed column width at construction. Kept as
        /// a flat list of physical lines so render + input share one source
        /// of truth for content height.
        body_lines: Vec<String>,
        scroll: u16,
        content_height: u16,
    },
    Annotations {
        title: String,
        items: Vec<String>,
        annotations: Vec<Annotation>,
        /// Diff snippets parallel to `annotations`: each entry is a list of
        /// `(line_number, content)` pairs around the annotation's anchor,
        /// used to render code context in the preview pane. File-level and
        /// orphaned annotations get an empty vec.
        snippets: Vec<Vec<(usize, String)>>,
        selected: usize,
        export_input: Option<String>,
        /// Error message to display (e.g., for failed export)
        error_message: Option<String>,
        /// One-shot success / info message (rendered in the status colour
        /// rather than the deleted/red colour used for errors).
        info_message: Option<String>,
        /// Human-readable label for the active annotation source
        /// (e.g. "local", "github #1234 (read-only)"). Rendered as a
        /// dedicated picker row at the top of the list; pressing enter on
        /// it cycles the source.
        source_label: String,
        /// Whether the source-picker row currently has focus instead of an
        /// annotation row. When true, `enter` emits
        /// `ModalResult::AnnotationSourceSwitch`; arrow keys move focus
        /// back into the list.
        source_focused: bool,
        /// When `d` has been pressed once on the highlighted annotation but
        /// not yet confirmed. The footer switches to a confirmation prompt;
        /// pressing `d` again deletes, any other key cancels.
        pending_delete: Option<AnnotationId>,
        /// When true, the modal hides the list and shows only the
        /// currently-selected thread, scrollable. Entered with `l`,
        /// exited with `h`.
        focused_preview: bool,
        /// Scroll position within the focused-preview view.
        preview_scroll: u16,
        /// Total line count of the focused-preview content, computed at
        /// render time so scroll math can clamp without rebuilding.
        preview_content_height: u16,
        /// Visible body height of the focused-preview view, computed at
        /// render time. Used together with `preview_content_height` to
        /// clamp scroll bounds in input/mouse handlers.
        preview_visible_height: u16,
        /// Inline reply composer shown at the bottom of the focused-preview
        /// view when the user presses `r`. `None` outside of compose mode.
        /// Keys are forwarded to this editor while it is `Some` — the modal
        /// itself only sees the result (save/cancel) from delegation.
        reply_editor: Option<AnnotationEditor<'static>>,
    },
}

pub struct Modal {
    pub content: ModalContent,
}

#[derive(Clone)]
pub enum ModalResult {
    Dismissed,
    #[allow(dead_code)]
    Selected(usize, String),
    FileSelected(usize),
    /// A selection from the revset picker. `literal` requests immutable
    /// review, both for listed changes and for typed revsets. The synthetic
    /// working-copy row restores the live diff.
    RevsetSelected {
        revset: Option<String>,
        literal: bool,
    },
    AnnotationJump {
        annotation_id: AnnotationId,
    },
    AnnotationEdit {
        annotation_id: AnnotationId,
    },
    AnnotationReply {
        annotation_id: AnnotationId,
    },
    /// Inline reply composed inside the focused-preview view. Carries the
    /// fully-typed body so the app can append it to the thread without
    /// opening the standalone editor.
    AnnotationReplyInline {
        annotation_id: AnnotationId,
        content: String,
    },
    AnnotationDelete {
        annotation_id: AnnotationId,
    },
    AnnotationToggleResolved {
        annotation_id: AnnotationId,
    },
    AnnotationCopyAll,
    AnnotationExport(String),
    /// User asked the annotations modal to switch to the next annotation
    /// source (e.g. local ↔ github).
    AnnotationSourceSwitch,
    /// User asked to push the selected annotation to the opposite source
    /// (local ↔ github). Direction is implied by the active source.
    AnnotationCopyToOther {
        annotation_id: AnnotationId,
    },
    /// User pressed `c` in the PR description modal — opens an editor for
    /// an issue-level PR comment.
    PrComment,
    /// User pressed `a` in the PR description modal — opens an editor for
    /// an approving review (body becomes the review summary).
    PrApprove,
}

impl Modal {
    #[allow(dead_code)]
    pub fn info(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            content: ModalContent::Info {
                title: title.into(),
                message: message.into(),
            },
        }
    }

    #[allow(dead_code)]
    pub fn select(title: impl Into<String>, items: Vec<String>) -> Self {
        Self {
            content: ModalContent::Select {
                title: title.into(),
                items,
                selected: 0,
            },
        }
    }

    /// Build a PR-description modal from the title bar text, an optional
    /// pre-styled status header, and the raw body. The body is wrapped
    /// once at construction so scroll math stays stable across resizes;
    /// `terminal_width` is consulted only to pick a sensible wrap column
    /// (the modal itself is recomputed against the live `frame.area()`
    /// on every render).
    ///
    /// Empty bodies still produce one placeholder row so the modal isn't
    /// empty.
    pub fn pr_description(
        title: impl Into<String>,
        status_lines: Vec<Line<'static>>,
        body: &str,
        terminal_width: u16,
    ) -> Self {
        let wrap_width = pr_desc_wrap_width(terminal_width);
        let mut body_lines: Vec<String> = Vec::new();
        for raw_line in body.split('\n') {
            // Preserve blank lines as separators.
            if raw_line.trim().is_empty() {
                body_lines.push(String::new());
                continue;
            }
            for wrapped in wrap_preview_line(raw_line, wrap_width) {
                body_lines.push(wrapped);
            }
        }
        if body_lines.is_empty() {
            body_lines.push("(no description)".to_string());
        }
        let content_height = body_lines.len() as u16;
        Self {
            content: ModalContent::PrDescription {
                title: title.into(),
                status_lines,
                body_lines,
                scroll: 0,
                content_height,
            },
        }
    }

    pub fn keybindings(title: impl Into<String>, sections: Vec<KeyBindSection>) -> Self {
        let content_height: u16 = sections
            .iter()
            .map(|s| s.bindings.len() as u16 + 2) // +2 for section title and spacing
            .sum();
        Self {
            content: ModalContent::KeyBindings {
                title: title.into(),
                sections,
                scroll: 0,
                content_height,
            },
        }
    }

    pub fn file_picker(title: impl Into<String>, items: Vec<FilePickerItem>) -> Self {
        let filtered_indices: Vec<usize> = (0..items.len()).collect();
        Self {
            content: ModalContent::FilePicker {
                title: title.into(),
                items,
                filtered_indices,
                query: String::new(),
                selected: 0,
            },
        }
    }

    pub fn revset_picker(title: impl Into<String>, items: Vec<RevsetPickerItem>) -> Self {
        let filtered_indices: Vec<usize> = (0..items.len()).collect();
        Self {
            content: ModalContent::RevsetPicker {
                title: title.into(),
                items,
                filtered_indices,
                query: String::new(),
                selected: 0,
                error: None,
            },
        }
    }

    /// Select the active change without filtering the list. Include it when
    /// it falls outside the initial revision list.
    pub fn select_revset(&mut self, item: RevsetPickerItem) {
        if let ModalContent::RevsetPicker {
            items,
            filtered_indices,
            selected,
            ..
        } = &mut self.content
        {
            let index = items
                .iter()
                .position(|existing| match (&existing.revset, &item.revset) {
                    (Some(prefix), Some(id)) => id.starts_with(prefix),
                    (None, None) => true,
                    _ => false,
                })
                .unwrap_or_else(|| {
                    items.push(item);
                    items.len() - 1
                });
            *filtered_indices = (0..items.len()).collect();
            *selected = index;
        }
    }

    /// Surface a failed revset resolution without rebuilding the modal, so
    /// the typed query and selection survive.
    pub fn set_revset_error(&mut self, msg: impl Into<String>) {
        if let ModalContent::RevsetPicker { error, .. } = &mut self.content {
            *error = Some(msg.into());
        }
    }

    /// Resolve ID-shaped queries before drawing, using the same backend as
    /// Enter. A failed/ambiguous prefix must not highlight an unrelated row.
    pub fn update_prefix_preview(
        &mut self,
        resolve: impl FnOnce(&str) -> Result<RevsetPickerItem, String>,
    ) {
        let ModalContent::RevsetPicker {
            items,
            filtered_indices,
            query,
            selected,
            error,
            ..
        } = &mut self.content
        else {
            return;
        };
        if !is_change_id_prefix(query.trim()) {
            return;
        }
        filtered_indices.clear();
        *selected = 0;
        match resolve(query.trim()) {
            Ok(item) => {
                let index = items
                    .iter()
                    .position(|existing| match (&existing.revset, &item.revset) {
                        (Some(prefix), Some(id)) => id.starts_with(prefix),
                        _ => false,
                    })
                    .unwrap_or_else(|| {
                        items.push(item);
                        items.len() - 1
                    });
                filtered_indices.push(index);
                *error = None;
            }
            Err(message) => *error = Some(message),
        }
    }

    pub fn annotations(
        title: impl Into<String>,
        items: Vec<String>,
        annotations: Vec<Annotation>,
        snippets: Vec<Vec<(usize, String)>>,
        source_label: impl Into<String>,
    ) -> Self {
        Self {
            content: ModalContent::Annotations {
                title: title.into(),
                items,
                annotations,
                snippets,
                selected: 0,
                export_input: None,
                error_message: None,
                info_message: None,
                source_label: source_label.into(),
                source_focused: false,
                pending_delete: None,
                focused_preview: false,
                preview_scroll: 0,
                preview_content_height: 0,
                preview_visible_height: 0,
                reply_editor: None,
            },
        }
    }

    /// Move focus to the source-picker row. Used after a rebuild that
    /// followed a source switch so the user stays on the picker.
    pub fn annotations_focus_source(&mut self) {
        if let ModalContent::Annotations { source_focused, .. } = &mut self.content {
            *source_focused = true;
        }
    }

    /// Seed the error-message slot, surfaced as a one-line "Error: ..."
    /// row inside the annotations modal.
    pub fn annotations_set_error(&mut self, msg: impl Into<String>) {
        if let ModalContent::Annotations { error_message, .. } = &mut self.content {
            *error_message = Some(msg.into());
        }
    }

    /// Seed the info-message slot for successful operations. Rendered in the
    /// non-error colour so users don't misread a success as a failure.
    pub fn annotations_set_info(&mut self, msg: impl Into<String>) {
        if let ModalContent::Annotations { info_message, .. } = &mut self.content {
            *info_message = Some(msg.into());
        }
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        let (modal_width, modal_height) = match &self.content {
            ModalContent::Info { message, .. } => {
                let width = 80.min(area.width.saturating_sub(4));
                let lines = message.lines().count() as u16;
                let height = (lines + 4).min(area.height * 80 / 100).max(5);
                (width, height)
            }
            ModalContent::Select { items, .. } => {
                let width = 80.min(area.width.saturating_sub(4));
                let items_count = items.len() as u16;
                let height = (items_count + 4).min(area.height * 80 / 100).max(5);
                (width, height)
            }
            ModalContent::KeyBindings { sections, .. } => {
                // Cap at 100 cols on widescreens so the modal stays
                // readable, but always fit within the terminal. Long
                // descriptions need ~75+ cols to avoid truncation, so we
                // give the modal more horizontal room than the old 60.
                let width = area.width.saturating_sub(4).min(100);
                let total_lines: usize = sections
                    .iter()
                    .map(|s| s.bindings.len() + 2) // +2 for section title and spacing
                    .sum();
                // +5 = top/bottom borders (2) + footer hint row (1) +
                // a small breathing buffer (2). Allow up to 90% of the
                // terminal so the help screen can grow with the list.
                let height = (total_lines as u16 + 5).min(area.height * 90 / 100).max(8);
                (width, height)
            }
            ModalContent::PrDescription {
                content_height,
                status_lines,
                ..
            } => {
                // Take most of the terminal — PR descriptions benefit from
                // breathing room, and we want the modal to feel substantial
                // rather than postage-stamp-sized.
                let width = (area.width * PR_DESC_MODAL_PCT / 100)
                    .min(area.width.saturating_sub(4))
                    .max(60);
                let header_rows = if status_lines.is_empty() {
                    0
                } else {
                    status_lines.len() as u16 + 1 // +1 for divider row
                };
                // +5 covers top/bottom borders + top/bottom inner padding
                // + 1 bottom hint row.
                let desired = content_height + header_rows + 5;
                // Floor at ~55% of terminal so the modal feels solid
                // without dominating the screen; cap at ~85% so a giant
                // body doesn't push off the visible area.
                let max_h = area.height * 85 / 100;
                let min_h = (area.height * 55 / 100)
                    .min(area.height.saturating_sub(2))
                    .max(10);
                let height = desired.min(max_h).max(min_h);
                (width, height)
            }
            ModalContent::FilePicker {
                filtered_indices, ..
            } => {
                let width = 80.min(area.width.saturating_sub(4));
                let items_count = filtered_indices.len().min(15) as u16;
                let height = (items_count + 5).min(area.height * 80 / 100).max(8);
                (width, height)
            }
            ModalContent::RevsetPicker {
                filtered_indices, ..
            } => {
                // Wider than the file picker: rows carry a change id, a
                // bookmark column and a description.
                let width = 120.min(area.width.saturating_sub(4));
                let items_count = filtered_indices.len().min(15) as u16;
                let height = (items_count * 2 + 5).min(area.height * 80 / 100).max(8);
                (width, height)
            }
            ModalContent::Annotations {
                items,
                export_input,
                ..
            } => {
                // Take most of the terminal so threads and previews have
                // room to breathe — capped to leave a small margin.
                let width = (area.width * 92 / 100)
                    .min(area.width.saturating_sub(4))
                    .max(60);
                // Each annotation row now spans 2 lines (title + location),
                // so account for that when sizing the modal to fit content.
                let items_lines = (items.len() as u16).saturating_mul(2);
                let extra = if export_input.is_some() { 5 } else { 3 };
                let desired = items_lines + extra + 2;
                let height = desired
                    .min(area.height * 92 / 100)
                    .max((area.height * 70 / 100).min(area.height.saturating_sub(2)))
                    .max(12);
                (width, height)
            }
        };

        let modal_x = (area.width.saturating_sub(modal_width)) / 2;
        let modal_y = (area.height.saturating_sub(modal_height)) / 2;
        let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

        frame.render_widget(Clear, modal_area);

        match &mut self.content {
            ModalContent::Info { title, message } => {
                Self::render_info(frame, modal_area, title, message);
            }
            ModalContent::Select {
                title,
                items,
                selected,
            } => {
                Self::render_select(frame, modal_area, title, items, *selected);
            }
            ModalContent::KeyBindings {
                title,
                sections,
                scroll,
                content_height,
            } => {
                Self::render_keybindings(
                    frame,
                    modal_area,
                    title,
                    sections,
                    *scroll,
                    *content_height,
                );
            }
            ModalContent::PrDescription {
                title,
                status_lines,
                body_lines,
                scroll,
                content_height,
            } => {
                Self::render_pr_description(
                    frame,
                    modal_area,
                    title,
                    status_lines,
                    body_lines,
                    *scroll,
                    *content_height,
                );
            }
            ModalContent::FilePicker {
                title,
                items,
                filtered_indices,
                query,
                selected,
            } => {
                Self::render_file_picker(
                    frame,
                    modal_area,
                    title,
                    items,
                    filtered_indices,
                    query,
                    *selected,
                );
            }
            ModalContent::RevsetPicker {
                title,
                items,
                filtered_indices,
                query,
                selected,
                error,
            } => {
                Self::render_revset_picker(
                    frame,
                    modal_area,
                    title,
                    items,
                    filtered_indices,
                    query,
                    *selected,
                    error.as_deref(),
                );
            }
            ModalContent::Annotations {
                title,
                items,
                annotations,
                snippets,
                selected,
                export_input,
                error_message,
                info_message,
                source_label,
                source_focused,
                pending_delete,
                focused_preview,
                preview_scroll,
                preview_content_height,
                preview_visible_height,
                reply_editor,
            } => {
                Self::render_annotations(
                    frame,
                    modal_area,
                    title,
                    items,
                    annotations,
                    snippets,
                    *selected,
                    export_input.as_deref(),
                    error_message.as_deref(),
                    info_message.as_deref(),
                    source_label,
                    *source_focused,
                    pending_delete.is_some(),
                    *focused_preview,
                    preview_scroll,
                    preview_content_height,
                    preview_visible_height,
                    reply_editor.as_ref(),
                );
            }
        }
    }

    fn render_info(frame: &mut Frame, area: Rect, title: &str, message: &str) {
        let t = theme::get();
        let block = Block::default()
            .title(format!(" {} ", title))
            .title_style(Style::default().fg(t.ui.border_focused).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_unfocused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line> = message
            .lines()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(t.ui.text_primary))))
            .collect();

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }

    fn render_select(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: &[String],
        selected: usize,
    ) {
        let t = theme::get();
        let block = Block::default()
            .title(format!(" {} ", title))
            .title_style(Style::default().fg(t.ui.border_focused).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_unfocused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let list_items: Vec<ListItem> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let style = if i == selected {
                    Style::default().fg(t.ui.selection_fg).bg(t.ui.selection_bg)
                } else {
                    Style::default().fg(t.ui.text_primary)
                };
                ListItem::new(format!("  {} ", item)).style(style)
            })
            .collect();

        let list = List::new(list_items);
        frame.render_widget(list, inner);
    }

    fn render_keybindings(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        sections: &[KeyBindSection],
        scroll: u16,
        content_height: u16,
    ) {
        let t = theme::get();
        let block = Block::default()
            .title(format!(" {} ", title))
            .title_style(Style::default().fg(t.ui.border_focused).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_unfocused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let key_width = sections
            .iter()
            .flat_map(|s| s.bindings.iter())
            .map(|b| b.key.len())
            .max()
            .unwrap_or(0);

        let mut lines: Vec<Line> = Vec::new();
        for (i, section) in sections.iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(""));
            }
            let section_label = format!("[{}]", section.title);
            lines.push(Line::from(Span::styled(
                format!("{:>width$}", section_label, width = key_width),
                Style::default().fg(t.ui.highlight).bold(),
            )));
            for bind in &section.bindings {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:>width$}", bind.key, width = key_width),
                        Style::default().fg(t.ui.status_added),
                    ),
                    Span::styled("   ", Style::default()),
                    Span::styled(bind.description, Style::default().fg(t.ui.text_primary)),
                ]));
            }
        }

        // Reserve the bottom row for a scroll-hint footer. Because the
        // help screen itself documents keys, we surface its own scroll
        // controls so users don't have to guess that they can scroll.
        let hint_height: u16 = 1;
        let body_height = inner.height.saturating_sub(hint_height);

        // Body area: reserves 1 col on the right for the scrollbar gutter.
        let content_area = Rect::new(inner.x, inner.y, inner.width.saturating_sub(1), body_height);

        let para = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(para, content_area);

        // Render scrollbar if content exceeds visible area. Constrain to
        // the body area so it doesn't overlap the footer hint row.
        let visible_height = body_height;
        if content_height > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█");

            let mut scrollbar_state =
                ScrollbarState::new(content_height.saturating_sub(visible_height) as usize)
                    .position(scroll as usize);

            let scrollbar_area = Rect::new(inner.x, inner.y, inner.width, body_height);
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }

        let hint_area = Rect::new(inner.x, inner.y + body_height, inner.width, hint_height);
        let key_style = Style::default().fg(t.ui.highlight).bold();
        let desc_style = Style::default().fg(t.ui.text_muted);
        let sep_style = Style::default().fg(t.ui.border_unfocused);
        let hint = Line::from(vec![
            Span::styled("j/k", key_style),
            Span::styled(" scroll  ", desc_style),
            Span::styled("│  ", sep_style),
            Span::styled("ctrl+d/u", key_style),
            Span::styled(" half-page  ", desc_style),
            Span::styled("│  ", sep_style),
            Span::styled("g/G", key_style),
            Span::styled(" top/bottom  ", desc_style),
            Span::styled("│  ", sep_style),
            Span::styled("esc", key_style),
            Span::styled(" close", desc_style),
        ]);
        frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), hint_area);
    }

    fn render_pr_description(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        status_lines: &[Line<'static>],
        body_lines: &[String],
        scroll: u16,
        content_height: u16,
    ) {
        let t = theme::get();
        let block = Block::default()
            .title(format!(" {} ", title))
            .title_style(Style::default().fg(t.ui.border_focused).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_unfocused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Inset by one column on each side and one row top/bottom so the
        // text isn't flush against the rounded border. Right-side inset
        // also doubles as scrollbar gutter so wrapped text doesn't collide.
        let padded = Rect::new(
            inner.x + 1,
            inner.y + 1,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(2),
        );

        // Status header (state badge, mergeability, checks). Sits above the
        // body separated by a divider row. Empty when we couldn't fetch
        // status fields (e.g., older `gh`).
        let header_height: u16 = if status_lines.is_empty() {
            0
        } else {
            status_lines.len() as u16 + 1 // status rows + divider
        };

        // Reserve a row at the bottom of the padded area for the action
        // hints (c/a/j/k/esc). Body shrinks to make room.
        let hint_height: u16 = 1;
        let body_area = Rect::new(
            padded.x,
            padded.y + header_height,
            padded.width,
            padded
                .height
                .saturating_sub(header_height)
                .saturating_sub(hint_height),
        );

        // Bottom hint row.
        let hint_y = padded.y + padded.height.saturating_sub(hint_height);
        let hint_area = Rect::new(padded.x, hint_y, padded.width, hint_height);
        let key_style = Style::default().fg(t.ui.highlight).bold();
        let desc_style = Style::default().fg(t.ui.text_muted);
        let sep_style = Style::default().fg(t.ui.border_unfocused);
        let hint = Line::from(vec![
            Span::styled("c", key_style),
            Span::styled(" comment  ", desc_style),
            Span::styled("│  ", sep_style),
            Span::styled("a", key_style),
            Span::styled(" approve  ", desc_style),
            Span::styled("│  ", sep_style),
            Span::styled("j/k", key_style),
            Span::styled(" scroll  ", desc_style),
            Span::styled("│  ", sep_style),
            Span::styled("esc", key_style),
            Span::styled(" close", desc_style),
        ]);
        frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), hint_area);

        if !status_lines.is_empty() {
            let header_area =
                Rect::new(padded.x, padded.y, padded.width, status_lines.len() as u16);
            frame.render_widget(Paragraph::new(status_lines.to_vec()), header_area);

            // Divider row between header and body.
            let divider_y = padded.y + status_lines.len() as u16;
            let divider_area = Rect::new(padded.x, divider_y, padded.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(padded.width as usize),
                    Style::default().fg(t.ui.border_unfocused),
                ))),
                divider_area,
            );
        }

        // Render body with markdown syntax highlighting per wrapped line.
        // We feed each wrapped line through the markdown grammar — multi-
        // line constructs (fenced code blocks spanning wraps) won't be
        // perfectly highlighted, but inline tokens (headers, emphasis,
        // inline code, links) come through correctly.
        let lines: Vec<Line> = body_lines
            .iter()
            .map(|line| {
                if line.is_empty() {
                    Line::from("")
                } else {
                    Line::from(highlight_line_spans(line, "_.md", None))
                }
            })
            .collect();

        let para = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(para, body_area);

        let visible_height = body_area.height;
        if content_height > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█");

            let mut scrollbar_state =
                ScrollbarState::new(content_height.saturating_sub(visible_height) as usize)
                    .position(scroll as usize);

            // Draw scrollbar inside the inner area (not the padded one) so
            // it hugs the right border instead of floating mid-modal.
            frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_file_picker(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: &[FilePickerItem],
        filtered_indices: &[usize],
        query: &str,
        selected: usize,
    ) {
        let t = theme::get();
        let block = Block::default()
            .title(format!(" {} ", title))
            .title_style(Style::default().fg(t.ui.border_focused).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_unfocused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        use ratatui::layout::{Constraint, Direction, Layout};
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);

        let input_line = Line::from(vec![
            Span::styled("> ", Style::default().fg(t.ui.status_added)),
            Span::styled(query, Style::default().fg(t.ui.text_primary)),
            Span::styled("_", Style::default().fg(t.ui.text_muted)),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);

        let visible_count = chunks[2].height as usize;
        let scroll_offset = if selected >= visible_count {
            selected - visible_count + 1
        } else {
            0
        };

        let list_items: Vec<ListItem> = filtered_indices
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_count)
            .map(|(i, &idx)| {
                let item = &items[idx];
                let is_selected = i == selected;

                let (status_char, status_color) = match item.status {
                    FileStatus::Added => ("A", t.ui.status_added),
                    FileStatus::Modified => ("M", t.ui.status_modified),
                    FileStatus::Deleted => ("D", t.ui.status_deleted),
                };

                let viewed_char = if item.viewed { "✓" } else { " " };

                let spans = if is_selected {
                    let selected_style =
                        Style::default().fg(t.ui.selection_fg).bg(t.ui.selection_bg);
                    vec![
                        Span::styled(format!(" {} ", viewed_char), selected_style),
                        Span::styled(format!("{} ", status_char), selected_style),
                        Span::styled(item.name.clone(), selected_style),
                    ]
                } else {
                    vec![
                        Span::styled(
                            format!(" {} ", viewed_char),
                            Style::default().fg(t.ui.viewed),
                        ),
                        Span::styled(
                            format!("{} ", status_char),
                            Style::default().fg(status_color),
                        ),
                        Span::styled(item.name.clone(), Style::default().fg(t.ui.text_primary)),
                    ]
                };

                ListItem::new(Line::from(spans))
            })
            .collect();

        let list = List::new(list_items);
        frame.render_widget(list, chunks[2]);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_revset_picker(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: &[RevsetPickerItem],
        filtered_indices: &[usize],
        query: &str,
        selected: usize,
        error: Option<&str>,
    ) {
        let t = theme::get();
        let block = Block::default()
            .title(format!(" {} ", title))
            .title_style(Style::default().fg(t.ui.border_focused).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_unfocused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        use ratatui::layout::{Constraint, Direction, Layout};
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);

        let input_line = Line::from(vec![
            Span::styled("> ", Style::default().fg(t.ui.status_added)),
            Span::styled(query, Style::default().fg(t.ui.text_primary)),
            Span::styled("_", Style::default().fg(t.ui.text_muted)),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);

        // Spacer row doubles as the status line: an error when the last
        // submission failed, otherwise a hint that the query can explicitly
        // be reviewed as an immutable revset even when it matches a row.
        let status_line = if let Some(err) = error {
            Line::from(Span::styled(
                format!("  {}", err),
                Style::default().fg(t.ui.stats_removed),
            ))
        } else if filtered_indices.is_empty() && !query.is_empty() {
            Line::from(Span::styled(
                "  no match — enter reviews the query as a revset",
                Style::default().fg(t.ui.text_muted),
            ))
        } else if !query.is_empty() {
            Line::from(Span::styled(
                "  enter reviews selected change; ctrl+enter reviews typed revset",
                Style::default().fg(t.ui.text_muted),
            ))
        } else {
            Line::from("  enter reviews selected change")
        };
        frame.render_widget(Paragraph::new(status_line), chunks[1]);

        let visible_count = chunks[2].height as usize;
        let mut scroll_offset = selected.min(filtered_indices.len().saturating_sub(1));
        let row_height = |index: usize| {
            items[filtered_indices[index]]
                .lines
                .as_ref()
                .map_or(1, Vec::len)
        };
        if !filtered_indices.is_empty() {
            let mut height = row_height(scroll_offset);
            while scroll_offset > 0 && height + row_height(scroll_offset - 1) <= visible_count {
                scroll_offset -= 1;
                height += row_height(scroll_offset);
            }
        }

        let list_items: Vec<ListItem> = filtered_indices
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_count)
            .map(|(i, &idx)| {
                let item = &items[idx];
                let style = if i == selected {
                    Style::default().fg(t.ui.selection_fg).bg(t.ui.selection_bg)
                } else {
                    Style::default().fg(t.ui.text_primary)
                };
                let lines = item
                    .lines
                    .clone()
                    .unwrap_or_else(|| vec![Line::from(format!(" {} ", item.label))]);
                ListItem::new(lines).style(style)
            })
            .collect();

        let list = List::new(list_items);
        frame.render_widget(list, chunks[2]);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_annotations(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: &[String],
        annotations: &[Annotation],
        snippets: &[Vec<(usize, String)>],
        selected: usize,
        export_input: Option<&str>,
        error_message: Option<&str>,
        info_message: Option<&str>,
        source_label: &str,
        source_focused: bool,
        delete_pending: bool,
        focused_preview: bool,
        preview_scroll: &mut u16,
        preview_content_height: &mut u16,
        preview_visible_height: &mut u16,
        reply_editor: Option<&AnnotationEditor<'static>>,
    ) {
        let t = theme::get();

        if focused_preview {
            render_annotations_focused(
                frame,
                area,
                title,
                annotations,
                snippets,
                selected,
                preview_scroll,
                preview_content_height,
                preview_visible_height,
                reply_editor,
                t,
            );
            return;
        }

        let title_line = Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(title, Style::default().fg(t.ui.text_primary).bold()),
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("{}", items.len()),
                Style::default().fg(t.ui.highlight).bold(),
            ),
            Span::styled(
                if items.len() == 1 {
                    " thread "
                } else {
                    " threads "
                },
                Style::default().fg(t.ui.text_muted),
            ),
        ]);

        let block = Block::default()
            .title(title_line)
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_focused));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        use ratatui::layout::{Constraint, Direction, Layout};

        // Vertical split: content (list + preview) above, optional export, then footer.
        let (content_area, footer_area) = if export_input.is_some() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(inner);
            (chunks[0], chunks[2])
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(inner);
            (chunks[0], chunks[1])
        };

        // Horizontal split: list on the left, preview thread on the right.
        // Drop the preview pane on narrow terminals where it can't render anything useful.
        let show_preview = content_area.width >= 60;
        let (list_area, preview_area) = if show_preview {
            let list_w = (content_area.width * 45 / 100).max(30);
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(list_w),
                    Constraint::Length(1),
                    Constraint::Min(10),
                ])
                .split(content_area);
            // Draw vertical divider between list and preview.
            let divider = Paragraph::new("│").style(Style::default().fg(t.ui.border_unfocused));
            for y in chunks[1].y..(chunks[1].y + chunks[1].height) {
                let row = Rect::new(chunks[1].x, y, 1, 1);
                frame.render_widget(divider.clone(), row);
            }
            (chunks[0], Some(chunks[2]))
        } else {
            (content_area, None)
        };

        // Split the list pane into the source-picker row + the actual list.
        let source_row = Rect::new(list_area.x, list_area.y, list_area.width, 1);
        let list_body = Rect::new(
            list_area.x,
            list_area.y + 1,
            list_area.width,
            list_area.height.saturating_sub(1),
        );
        render_source_picker_row(frame, source_row, source_label, source_focused, t);

        // Render the annotation list (left pane).
        render_annotation_list(
            frame,
            list_body,
            items,
            annotations,
            selected,
            source_focused,
            t,
        );

        // Render the thread preview (right pane).
        if let Some(area) = preview_area {
            let preview = annotations.get(selected);
            let snippet = snippets.get(selected).map(|s| s.as_slice()).unwrap_or(&[]);
            render_annotation_preview(frame, area, preview, snippet, t);
        }

        // Render export input if active.
        if let Some(input) = export_input {
            let input_area = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(inner)[1];

            let input_block = Block::default()
                .title(" Export path ")
                .title_style(Style::default().fg(t.ui.text_muted))
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(t.ui.border_unfocused));

            let input_inner = input_block.inner(input_area);
            frame.render_widget(input_block, input_area);

            let input_line = Line::from(vec![
                Span::styled(input, Style::default().fg(t.ui.text_primary)),
                Span::styled("_", Style::default().fg(t.ui.text_muted)),
            ]);
            frame.render_widget(Paragraph::new(input_line), input_inner);
        }

        if let Some(error) = error_message {
            let error_line = Line::from(vec![
                Span::styled("Error: ", Style::default().fg(t.ui.status_deleted).bold()),
                Span::styled(error, Style::default().fg(t.ui.status_deleted)),
            ]);
            let error_para =
                Paragraph::new(error_line).alignment(ratatui::prelude::Alignment::Center);
            let error_area = Rect::new(
                list_area.x,
                list_area.y + list_area.height.saturating_sub(1),
                list_area.width,
                1,
            );
            frame.render_widget(error_para, error_area);
        } else if let Some(info) = info_message {
            let info_line = Line::from(Span::styled(
                info,
                Style::default().fg(t.ui.status_added).bold(),
            ));
            let info_para =
                Paragraph::new(info_line).alignment(ratatui::prelude::Alignment::Center);
            let info_area = Rect::new(
                list_area.x,
                list_area.y + list_area.height.saturating_sub(1),
                list_area.width,
                1,
            );
            frame.render_widget(info_para, info_area);
        }

        // Footer — keys in the highlight colour, descriptions muted, with
        // dim vertical separators between groups so the eye can scan the
        // available actions quickly.
        let key_style = Style::default().fg(t.ui.highlight).bold();
        let desc_style = Style::default().fg(t.ui.text_muted);
        let sep_style = Style::default().fg(t.ui.border_unfocused);
        let mut spans: Vec<Span> = Vec::new();
        let push_pair = |spans: &mut Vec<Span>, key: &str, desc: &str, last: bool| {
            spans.push(Span::styled(key.to_string(), key_style));
            spans.push(Span::styled(format!(" {}", desc), desc_style));
            if !last {
                spans.push(Span::styled("  ·  ", sep_style));
            }
        };
        if export_input.is_some() {
            push_pair(&mut spans, "↵", "confirm", false);
            push_pair(&mut spans, "esc", "cancel", true);
        } else if delete_pending {
            spans.push(Span::styled(
                "delete this annotation?  ",
                Style::default().fg(t.ui.status_deleted).bold(),
            ));
            push_pair(&mut spans, "d", "confirm", false);
            push_pair(&mut spans, "esc", "cancel", true);
        } else {
            let copy_to = if source_label.contains("github") {
                "copy to local"
            } else {
                "copy to github"
            };
            push_pair(&mut spans, "↵", "jump", false);
            push_pair(&mut spans, "e", "edit", false);
            push_pair(&mut spans, "r", "reply", false);
            push_pair(&mut spans, "x", "resolve", false);
            push_pair(&mut spans, "d", "del", false);
            push_pair(&mut spans, "y", "copy", false);
            push_pair(&mut spans, "p", copy_to, false);
            push_pair(&mut spans, "o", "export", true);
        }
        let footer =
            Paragraph::new(Line::from(spans)).alignment(ratatui::prelude::Alignment::Center);
        frame.render_widget(footer, footer_area);
    }

    /// Handle mouse scroll for the modal.
    /// Returns true if the scroll was handled.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, terminal_height: u16) -> bool {
        let (scroll, content_height, visible_height) = match &mut self.content {
            ModalContent::KeyBindings {
                scroll,
                content_height,
                ..
            } => {
                let visible =
                    calculate_keybindings_visible_height(terminal_height, *content_height);
                (scroll, *content_height, visible)
            }
            ModalContent::PrDescription {
                scroll,
                content_height,
                ..
            } => {
                let visible =
                    calculate_pr_description_visible_height(terminal_height, *content_height);
                (scroll, *content_height, visible)
            }
            ModalContent::Annotations {
                focused_preview,
                preview_scroll,
                preview_content_height,
                preview_visible_height,
                ..
            } if *focused_preview => (
                preview_scroll,
                *preview_content_height,
                (*preview_visible_height).max(1),
            ),
            _ => return false,
        };

        let max_scroll = content_height.saturating_sub(visible_height);

        match mouse.kind {
            MouseEventKind::ScrollDown => {
                *scroll = (*scroll + 3).min(max_scroll);
                true
            }
            MouseEventKind::ScrollUp => {
                *scroll = scroll.saturating_sub(3);
                true
            }
            _ => false,
        }
    }

    /// Handle keyboard input for the modal.
    /// Returns Some(ModalResult) if the modal should close.
    pub fn handle_input(&mut self, key: KeyEvent, terminal_height: u16) -> Option<ModalResult> {
        // FilePicker, RevsetPicker and Annotations handle their own dismiss logic
        if !matches!(
            self.content,
            ModalContent::FilePicker { .. }
                | ModalContent::RevsetPicker { .. }
                | ModalContent::Annotations { .. }
        ) {
            // Close on Esc, q, or Ctrl+C
            if key.code == KeyCode::Esc
                || key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Some(ModalResult::Dismissed);
            }
        }

        match &mut self.content {
            ModalContent::Info { .. } => {
                // Any key closes info modal
                if key.code == KeyCode::Enter {
                    return Some(ModalResult::Dismissed);
                }
                None
            }
            ModalContent::Select {
                items, selected, ..
            } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected < items.len().saturating_sub(1) {
                        *selected += 1;
                    }
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    None
                }
                KeyCode::Enter => {
                    let idx = *selected;
                    let value = items.get(idx).cloned().unwrap_or_default();
                    Some(ModalResult::Selected(idx, value))
                }
                _ => None,
            },
            ModalContent::KeyBindings {
                scroll,
                content_height,
                ..
            } => {
                let visible_height =
                    calculate_keybindings_visible_height(terminal_height, *content_height);
                let max_scroll = content_height.saturating_sub(visible_height);

                match key.code {
                    KeyCode::Enter => Some(ModalResult::Dismissed),
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = (*scroll + 1).min(max_scroll);
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll = scroll.saturating_sub(1);
                        None
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Half-page down
                        *scroll = (*scroll + visible_height / 2).min(max_scroll);
                        None
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Half-page up
                        *scroll = scroll.saturating_sub(visible_height / 2);
                        None
                    }
                    KeyCode::Char('g') => {
                        // Go to top
                        *scroll = 0;
                        None
                    }
                    KeyCode::Char('G') => {
                        // Go to bottom
                        *scroll = max_scroll;
                        None
                    }
                    _ => None,
                }
            }
            ModalContent::PrDescription {
                scroll,
                content_height,
                ..
            } => {
                let visible_height =
                    calculate_pr_description_visible_height(terminal_height, *content_height);
                let max_scroll = content_height.saturating_sub(visible_height);

                match key.code {
                    KeyCode::Enter => Some(ModalResult::Dismissed),
                    // Conversation actions — bubble up; the app layer
                    // closes this modal and opens the comment editor.
                    KeyCode::Char('c') => Some(ModalResult::PrComment),
                    KeyCode::Char('a') => Some(ModalResult::PrApprove),
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = (*scroll + 1).min(max_scroll);
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll = scroll.saturating_sub(1);
                        None
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *scroll = (*scroll + visible_height / 2).min(max_scroll);
                        None
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *scroll = scroll.saturating_sub(visible_height / 2);
                        None
                    }
                    KeyCode::Char('g') => {
                        *scroll = 0;
                        None
                    }
                    KeyCode::Char('G') => {
                        *scroll = max_scroll;
                        None
                    }
                    _ => None,
                }
            }
            ModalContent::FilePicker {
                items,
                filtered_indices,
                query,
                selected,
                ..
            } => match key.code {
                KeyCode::Esc => Some(ModalResult::Dismissed),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(ModalResult::Dismissed)
                }
                KeyCode::Down | KeyCode::Char('j')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.code == KeyCode::Down =>
                {
                    if *selected < filtered_indices.len().saturating_sub(1) {
                        *selected += 1;
                    }
                    None
                }
                KeyCode::Up | KeyCode::Char('k')
                    if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Up =>
                {
                    *selected = selected.saturating_sub(1);
                    None
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if *selected < filtered_indices.len().saturating_sub(1) {
                        *selected += 1;
                    }
                    None
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    *selected = selected.saturating_sub(1);
                    None
                }
                KeyCode::Enter => {
                    if let Some(&file_idx) = filtered_indices.get(*selected) {
                        Some(ModalResult::FileSelected(items[file_idx].file_index))
                    } else {
                        Some(ModalResult::Dismissed)
                    }
                }
                KeyCode::Backspace => {
                    query.pop();
                    Self::update_filtered_indices(items, query, filtered_indices, selected);
                    None
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    Self::update_filtered_indices(items, query, filtered_indices, selected);
                    None
                }
                _ => None,
            },
            ModalContent::RevsetPicker {
                items,
                filtered_indices,
                query,
                selected,
                error,
                ..
            } => match key.code {
                KeyCode::Esc => Some(ModalResult::Dismissed),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(ModalResult::Dismissed)
                }
                KeyCode::Down => {
                    if *selected < filtered_indices.len().saturating_sub(1) {
                        *selected += 1;
                    }
                    None
                }
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    None
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if *selected < filtered_indices.len().saturating_sub(1) {
                        *selected += 1;
                    }
                    None
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    *selected = selected.saturating_sub(1);
                    None
                }
                KeyCode::Enter
                    if !query.trim().is_empty()
                        && (key.modifiers.contains(KeyModifiers::CONTROL)
                            || is_change_id_prefix(query.trim())
                            || crate::commit_reference::split_endpoint_range(query.trim())
                                .is_some()) =>
                {
                    Some(ModalResult::RevsetSelected {
                        revset: Some(query.trim().to_string()),
                        literal: true,
                    })
                }
                KeyCode::Enter => match filtered_indices.get(*selected) {
                    Some(&idx) => Some(ModalResult::RevsetSelected {
                        revset: items[idx].revset.clone(),
                        literal: items[idx].revset.is_some(),
                    }),
                    // Nothing matched the filter: treat what was typed as a
                    // literal revset so `@---` and friends still work.
                    None if !query.trim().is_empty() => Some(ModalResult::RevsetSelected {
                        revset: Some(query.trim().to_string()),
                        literal: true,
                    }),
                    None => Some(ModalResult::Dismissed),
                },
                KeyCode::Backspace => {
                    query.pop();
                    *error = None;
                    Self::update_revset_filtered_indices(items, query, filtered_indices, selected);
                    None
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    *error = None;
                    Self::update_revset_filtered_indices(items, query, filtered_indices, selected);
                    None
                }
                _ => None,
            },
            ModalContent::Annotations {
                items,
                annotations,
                selected,
                export_input,
                error_message,
                source_focused,
                pending_delete,
                focused_preview,
                preview_scroll,
                preview_content_height,
                preview_visible_height,
                reply_editor,
                ..
            } => {
                // Focused-preview mode: list is hidden, only the thread is
                // shown, scrollable. Most action keys are disabled — `h`
                // returns to the list view, scroll keys move within the
                // thread, and esc/q close the modal.
                if *focused_preview {
                    // When the inline reply composer is active, forward
                    // keys to it. Save emits a result the app handles by
                    // adding the reply and rebuilding the modal; cancel /
                    // delete just close the composer.
                    if let Some(editor) = reply_editor.as_mut() {
                        return match editor.handle_input(key) {
                            AnnotationEditorResult::Continue => None,
                            AnnotationEditorResult::Cancel | AnnotationEditorResult::Delete => {
                                *reply_editor = None;
                                None
                            }
                            AnnotationEditorResult::Save => {
                                let content = editor.content();
                                let annotation_id = editor.id.clone();
                                *reply_editor = None;
                                annotation_id.map(|id| ModalResult::AnnotationReplyInline {
                                    annotation_id: id,
                                    content,
                                })
                            }
                        };
                    }

                    let content_height = *preview_content_height;
                    let visible_height = (*preview_visible_height).max(1);
                    let max_scroll = content_height.saturating_sub(visible_height);
                    return match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => Some(ModalResult::Dismissed),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            Some(ModalResult::Dismissed)
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            *focused_preview = false;
                            *preview_scroll = 0;
                            None
                        }
                        // Open the inline reply composer for the currently
                        // displayed annotation. Skipped silently when the
                        // list is empty — no thread = nothing to reply to.
                        KeyCode::Char('r') => {
                            if let Some(ann) = annotations.get(*selected) {
                                *reply_editor = Some(
                                    AnnotationEditor::new(ann.filename.clone(), ann.target.clone())
                                        .with_reply(ann.id.clone()),
                                );
                            }
                            None
                        }
                        // Toggle resolved state on the currently displayed
                        // annotation. App rebuilds the modal so the
                        // resolved badge updates in place.
                        KeyCode::Char('x') => annotations.get(*selected).map(|ann| {
                            ModalResult::AnnotationToggleResolved {
                                annotation_id: ann.id.clone(),
                            }
                        }),
                        // Jump to the annotation in the diff — same primary
                        // action as `enter` in the list view.
                        KeyCode::Enter => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationJump {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            *preview_scroll = (preview_scroll.saturating_add(1)).min(max_scroll);
                            None
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            *preview_scroll = preview_scroll.saturating_sub(1);
                            None
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            *preview_scroll = preview_scroll
                                .saturating_add(visible_height / 2)
                                .min(max_scroll);
                            None
                        }
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            *preview_scroll = preview_scroll.saturating_sub(visible_height / 2);
                            None
                        }
                        KeyCode::Char('g') => {
                            *preview_scroll = 0;
                            None
                        }
                        KeyCode::Char('G') => {
                            *preview_scroll = max_scroll;
                            None
                        }
                        _ => None,
                    };
                }

                // Export input mode
                if let Some(ref mut input) = export_input {
                    match key.code {
                        KeyCode::Esc => {
                            *export_input = None;
                            *error_message = None;
                            None
                        }
                        KeyCode::Enter => {
                            let filename = input.trim();
                            // Basic path validation
                            if filename.is_empty() {
                                *error_message = Some("Path cannot be empty".to_string());
                                return None;
                            }
                            if filename.contains("..") {
                                *error_message = Some("Path cannot contain '..'".to_string());
                                return None;
                            }
                            // Clear any previous error and proceed
                            *error_message = None;
                            Some(ModalResult::AnnotationExport(filename.to_string()))
                        }
                        KeyCode::Backspace => {
                            input.pop();
                            *error_message = None; // Clear error on edit
                            None
                        }
                        KeyCode::Char(c) => {
                            input.push(c);
                            *error_message = None; // Clear error on edit
                            None
                        }
                        _ => None,
                    }
                } else if let Some(target_id) = pending_delete.clone() {
                    // Delete-confirmation mode: only `d` confirms; anything
                    // else cancels and is swallowed so the user can't
                    // accidentally chain into another destructive action.
                    match key.code {
                        KeyCode::Char('d') if !*source_focused => {
                            *pending_delete = None;
                            Some(ModalResult::AnnotationDelete {
                                annotation_id: target_id,
                            })
                        }
                        _ => {
                            *pending_delete = None;
                            None
                        }
                    }
                } else {
                    // Normal mode
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c')
                            if key.code == KeyCode::Esc
                                || key.code == KeyCode::Char('q')
                                || key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            Some(ModalResult::Dismissed)
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if *source_focused {
                                // Move focus from picker row into the list.
                                *source_focused = false;
                            } else if *selected < items.len().saturating_sub(1) {
                                *selected += 1;
                            }
                            None
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if !*source_focused && *selected == 0 {
                                // Top of list — move focus up to the picker row.
                                *source_focused = true;
                            } else if !*source_focused {
                                *selected -= 1;
                            }
                            None
                        }
                        KeyCode::Enter if *source_focused => {
                            Some(ModalResult::AnnotationSourceSwitch)
                        }
                        KeyCode::Enter => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationJump {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        // Action keys are list-only: ignore while the
                        // source row is focused so we don't act on the
                        // currently-selected annotation by accident.
                        KeyCode::Char('e') if !*source_focused => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationEdit {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('r') if !*source_focused => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationReply {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('d') if !*source_focused => {
                            if let Some(ann) = annotations.get(*selected) {
                                *pending_delete = Some(ann.id.clone());
                            }
                            None
                        }
                        KeyCode::Char('x') if !*source_focused => {
                            annotations.get(*selected).map(|ann| {
                                ModalResult::AnnotationToggleResolved {
                                    annotation_id: ann.id.clone(),
                                }
                            })
                        }
                        KeyCode::Char('y') if !*source_focused => {
                            Some(ModalResult::AnnotationCopyAll)
                        }
                        KeyCode::Char('p') if !*source_focused => {
                            annotations.get(*selected).map(|ann| {
                                ModalResult::AnnotationCopyToOther {
                                    annotation_id: ann.id.clone(),
                                }
                            })
                        }
                        KeyCode::Char('o') if !*source_focused => {
                            *export_input = Some(String::from("annotations.txt"));
                            None
                        }
                        // `l` / Right enters the focused (full-modal,
                        // scrollable) preview of the currently selected
                        // thread. Disabled when the source-picker row is
                        // focused or the list is empty — otherwise nothing
                        // would be shown.
                        KeyCode::Char('l') | KeyCode::Right
                            if !*source_focused && !annotations.is_empty() =>
                        {
                            *focused_preview = true;
                            *preview_scroll = 0;
                            None
                        }
                        _ => None,
                    }
                }
            }
        }
    }

    fn update_revset_filtered_indices(
        items: &[RevsetPickerItem],
        query: &str,
        filtered_indices: &mut Vec<usize>,
        selected: &mut usize,
    ) {
        let query_lower = query.to_lowercase();
        *filtered_indices = items
            .iter()
            .enumerate()
            .filter(|(_, item)| fuzzy_match(&item.label.to_lowercase(), &query_lower))
            .map(|(i, _)| i)
            .collect();
        if *selected >= filtered_indices.len() {
            *selected = filtered_indices.len().saturating_sub(1);
        }
    }

    fn update_filtered_indices(
        items: &[FilePickerItem],
        query: &str,
        filtered_indices: &mut Vec<usize>,
        selected: &mut usize,
    ) {
        let query_lower = query.to_lowercase();
        *filtered_indices = items
            .iter()
            .enumerate()
            .filter(|(_, item)| fuzzy_match(&item.name.to_lowercase(), &query_lower))
            .map(|(i, _)| i)
            .collect();
        if *selected >= filtered_indices.len() {
            *selected = filtered_indices.len().saturating_sub(1);
        }
    }
}

// jj change IDs use the reverse-hex alphabet k-z, case-insensitively.
// Submit ID-shaped input verbatim so fuzzy matches cannot hijack a prefix,
// including an ambiguous prefix or a change outside the initial picker rows.
fn is_change_id_prefix(query: &str) -> bool {
    !query.is_empty()
        && query
            .bytes()
            .all(|c| (b'k'..=b'z').contains(&c.to_ascii_lowercase()))
}

fn fuzzy_match(text: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    let mut pattern_chars = pattern.chars().peekable();
    for c in text.chars() {
        if pattern_chars.peek() == Some(&c) {
            pattern_chars.next();
        }
        if pattern_chars.peek().is_none() {
            return true;
        }
    }
    pattern_chars.peek().is_none()
}

/// Calculate visible height for keybindings modal based on terminal size.
/// Pick the body wrap column for a given terminal width. Mirrors the
/// modal-sizing math in `Modal::render` so the wrapped body fills the
/// inner padded area without overflow:
///   inner_width = modal_width - 2 (borders)
///   body_width  = inner_width - 2 (left/right padding)
///                  - 1 (right scrollbar gutter)
/// Clamped between min and max so very narrow or ultra-wide terminals
/// still produce readable output.
fn pr_desc_wrap_width(terminal_width: u16) -> usize {
    let modal_width = (terminal_width as usize).saturating_mul(PR_DESC_MODAL_PCT as usize) / 100;
    let modal_width = modal_width.min(terminal_width.saturating_sub(4) as usize);
    // 2 for borders + 2 for inner padding + 1 for scrollbar gutter = 5.
    let body_width = modal_width.saturating_sub(5);
    body_width
        .min(PR_DESC_MAX_WRAP_WIDTH)
        .max(PR_DESC_MIN_WRAP_WIDTH)
}

fn calculate_keybindings_visible_height(terminal_height: u16, content_height: u16) -> u16 {
    // Mirrors the modal sizing in `Modal::render` for KeyBindings so
    // scroll math matches the rendered viewport:
    //   modal_height = (content + 5).min(terminal * 90 / 100).max(8)
    //   visible      = modal_height - 2 (borders) - 1 (footer hint)
    let modal_height = (content_height + 5).min(terminal_height * 90 / 100).max(8);
    modal_height.saturating_sub(3)
}

/// Approximate visible body height for the PR description modal. Mirrors
/// the prior shared formula — only used for scroll-math clamping, so an
/// off-by-a-few-rows result just means `j/k` stop a row early or late.
fn calculate_pr_description_visible_height(terminal_height: u16, content_height: u16) -> u16 {
    let modal_height = (content_height + 4).min(terminal_height * 80 / 100).max(5);
    modal_height.saturating_sub(2)
}

/// Render the source-picker row (`Source: <label>`) at the top of the
/// annotations modal. The row highlights when focused so the user can tell
/// they're about to switch sources rather than act on an annotation.
fn render_source_picker_row(
    frame: &mut Frame,
    area: Rect,
    source_label: &str,
    focused: bool,
    t: &theme::Theme,
) {
    // Render as a small "tab" — accent bar on the left, label tag, value
    // segment, then a subtle hint. The focused state inverts the value
    // segment so it visually pops as the active row.
    let bar_style = if focused {
        Style::default().fg(t.ui.selection_bg).bold()
    } else {
        Style::default().fg(t.ui.border_unfocused)
    };
    let label_tag_style = Style::default().fg(t.ui.text_muted).bold();
    let value_style = if focused {
        Style::default()
            .fg(t.ui.selection_fg)
            .bg(t.ui.selection_bg)
            .bold()
    } else {
        Style::default().fg(t.ui.text_primary).bold()
    };
    let hint_style = Style::default().fg(t.ui.text_muted).italic();

    let spans = vec![
        Span::styled("▎", bar_style),
        Span::styled(" SOURCE ", label_tag_style),
        Span::styled(format!(" {} ", source_label), value_style),
        Span::styled("   ", Style::default()),
        Span::styled(
            if focused {
                "↵ switch"
            } else {
                "↑↓ navigate · ↵ switch"
            },
            hint_style,
        ),
    ];
    let para = Paragraph::new(Line::from(spans));
    frame.render_widget(para, area);
}

/// Render the left-hand annotation picker list. Each row spans **two** lines:
///
/// ```text
///  ▎ ● <title (first line of root comment)>          💬 N · HH:MM
///  ▎     <filename> · <line range> · <panel>
/// ```
///
/// The selection bar extends across both lines, the status glyph reflects
/// open/resolved/orphaned, and the right-hand badges show comment count and
/// time. Line 2 carries the file location in a muted style so it reads as a
/// subtitle. Scroll math is in items (not lines), so `selected` indexes into
/// the items vector directly.
fn render_annotation_list(
    frame: &mut Frame,
    area: Rect,
    items: &[String],
    annotations: &[Annotation],
    selected: usize,
    source_focused: bool,
    t: &theme::Theme,
) {
    const ROW_HEIGHT: usize = 2;

    let visible_count = ((area.height as usize) / ROW_HEIGHT).max(1);
    let scroll_offset = if selected >= visible_count {
        selected - visible_count + 1
    } else {
        0
    };

    let list_items: Vec<ListItem> = annotations
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(i, ann)| {
            // Suppress the highlight while the source-picker row is focused
            // so it's obvious which row `enter` acts on.
            let is_selected = i == selected && !source_focused;

            // Orphaned status isn't carried on Annotation itself — it's
            // derived in `build_annotation_list` and surfaced in the items
            // string via "(orphaned)". Parse it back out here.
            let orphaned = items
                .get(i)
                .map(|s| s.contains("(orphaned)"))
                .unwrap_or(false);
            let resolved = ann.resolved;
            let comment_count = ann.comments.len();

            // Status glyph + colour. Resolved beats orphaned for visual
            // hierarchy — a resolved orphan is "done", not a warning.
            let (status_glyph, status_color) = if resolved {
                ("✓", t.ui.status_added)
            } else if orphaned {
                ("◌", t.ui.text_muted)
            } else {
                ("●", t.ui.status_modified)
            };

            // Title = first non-empty line of the root comment, trimmed,
            // then cut after the first sentence/clause terminator so the
            // list row reads as a short summary rather than a full thought.
            // Falls back to a placeholder if the body is somehow empty.
            let title_line = ann
                .root()
                .content
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim())
                .unwrap_or("(empty)");
            let title_raw = title_line
                .find(|c: char| matches!(c, '.' | '!' | '?' | ':' | ';'))
                .map(|i| &title_line[..=i])
                .unwrap_or(title_line);

            // Subtitle = filename · line range · panel. File-level
            // annotations skip the line range so the subtitle stays clean.
            let line_range = ann.line_range_display();
            let label = ann.target_label();
            let subtitle = if line_range.is_empty() {
                format!("{} · {}", ann.filename, label)
            } else {
                format!("{} · {} · {}", ann.filename, line_range, label)
            };

            // Right-hand badges on line 1: comment count + time. Pluralise
            // "comment(s)" so a single-comment thread doesn't read awkwardly.
            // No emoji — they render at uncertain widths across terminals,
            // which throws off the padding math. Plain text keeps layout
            // deterministic.
            let time = ann.format_time();
            let count_word = if comment_count == 1 {
                "comment"
            } else {
                "comments"
            };
            let count_text = format!("{} {}", comment_count, count_word);
            let right_text = format!("{} · {}", count_text, time);
            let right_w_estimate = right_text.chars().count();

            // Layout for line 1: ` ▎ ● <title> … <right_text> `.
            // Fixed prefix = bar(1) + space(1) + glyph(1) + space(1) = 4.
            // RIGHT_MARGIN is the minimum gap between the (possibly
            // truncated) title and the right-aligned badges — it has to be
            // reserved out of `title_max` so the ellipsis never butts up
            // against the count, and it must NOT be added back into `used`
            // (padding is computed from the actually rendered spans).
            let available = area.width as usize;
            const PREFIX_W: usize = 4;
            const RIGHT_MARGIN: usize = 4;
            let title_max = available
                .saturating_sub(PREFIX_W + right_w_estimate + RIGHT_MARGIN)
                .max(8);
            let truncated_title = truncate_with_ellipsis(title_raw, title_max);
            let used = PREFIX_W + truncated_title.chars().count() + right_w_estimate;
            let padding = available.saturating_sub(used).max(RIGHT_MARGIN);

            // Line 2 layout: ` ▎   <subtitle> ` — same bar column, indented
            // to align under the title text on line 1 (bar+space+glyph+space
            // = 4 cols, so subtitle starts at column 4 too).
            const SUBTITLE_INDENT: usize = 3;
            let subtitle_max = available.saturating_sub(SUBTITLE_INDENT + 2).max(8);
            let truncated_subtitle = truncate_with_ellipsis(&subtitle, subtitle_max);

            // Per-state styling. Selected rows get a *subtle* background
            // tint (footer_branch_bg) — the previous selection_bg was the
            // theme's bright accent colour, which made the highlighted row
            // dominate the modal. The dim tint reads as "selected" without
            // shouting. A bright left-edge bar still marks the selection
            // unambiguously.
            let row_bg = if is_selected {
                Some(t.ui.footer_branch_bg)
            } else {
                None
            };
            let with_bg = |s: Style| match row_bg {
                Some(bg) => s.bg(bg),
                None => s,
            };
            // Selection bar: bright accent in col 0 on both lines when this
            // row is selected, blank when not. Bar also picks up the dim
            // row bg so col 0 is part of the highlighted block.
            let (bar_glyph, bar_style) = if is_selected {
                (
                    "▎",
                    Style::default()
                        .fg(t.ui.selection_bg)
                        .bg(t.ui.footer_branch_bg),
                )
            } else {
                (" ", Style::default())
            };
            let status_style = Style::default().fg(status_color).bold();

            // With a subtle row bg, body text keeps its normal colour and
            // relies on the bg + bar to signal selection. Orphaned rows
            // stay muted regardless.
            let title_fg = if orphaned {
                t.ui.text_muted
            } else {
                t.ui.text_primary
            };
            let subtitle_fg = if is_selected {
                t.ui.text_secondary
            } else {
                t.ui.text_muted
            };
            let count_fg = t.ui.highlight;
            let time_fg = t.ui.text_muted;

            // Build line 1 spans. The right-text is split into separately
            // coloured spans so the count badge stands out from the time.
            let line1_spans = vec![
                Span::styled(bar_glyph, bar_style),
                Span::styled(" ", with_bg(Style::default())),
                Span::styled(format!("{} ", status_glyph), with_bg(status_style)),
                Span::styled(
                    truncated_title,
                    with_bg(Style::default().fg(title_fg).bold()),
                ),
                Span::styled(
                    format!("{:>width$}", "", width = padding),
                    with_bg(Style::default()),
                ),
                Span::styled(
                    count_text.clone(),
                    with_bg(Style::default().fg(count_fg).bold()),
                ),
                Span::styled(" · ", with_bg(Style::default().fg(t.ui.border_unfocused))),
                Span::styled(time.clone(), with_bg(Style::default().fg(time_fg))),
            ];

            // Build line 2 spans. The bar carries across both lines so a
            // selected row reads as a single 2-line block. A trailing
            // padding span fills the remainder of the row width so the
            // selected-row background extends edge-to-edge instead of
            // ending where the subtitle text ends.
            let subtitle_used = 1 + SUBTITLE_INDENT + truncated_subtitle.chars().count();
            let subtitle_padding = available.saturating_sub(subtitle_used);
            let line2_spans = vec![
                Span::styled(bar_glyph, bar_style),
                Span::styled("   ", with_bg(Style::default())),
                Span::styled(
                    truncated_subtitle,
                    with_bg(Style::default().fg(subtitle_fg).italic()),
                ),
                Span::styled(
                    format!("{:>width$}", "", width = subtitle_padding),
                    with_bg(Style::default()),
                ),
            ];

            ListItem::new(vec![Line::from(line1_spans), Line::from(line2_spans)])
        })
        .collect();

    frame.render_widget(List::new(list_items), area);
}

/// Truncate `s` to at most `max` chars, appending an ellipsis when it has
/// to cut. Returns the original string when it fits.
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max || max < 2 {
        s.to_string()
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{}…", head)
    }
}

/// Expand `\t` characters in span text to spaces aligned to `tab_width`
/// stops, preserving per-span styling. Mirrors the diff view's tab handling
/// so the snippet column math matches the rendered output exactly.
fn expand_tabs_in_spans<'a>(spans: Vec<Span<'a>>, tab_width: usize) -> Vec<Span<'a>> {
    if tab_width == 0 {
        return spans
            .into_iter()
            .map(|span| {
                if !span.content.contains('\t') {
                    span
                } else {
                    let buf: String = span.content.chars().filter(|c| *c != '\t').collect();
                    Span::styled(buf, span.style)
                }
            })
            .collect();
    }
    let mut col = 0usize;
    let mut out: Vec<Span<'a>> = Vec::with_capacity(spans.len());
    for span in spans {
        if !span.content.contains('\t') {
            col += span.content.chars().count();
            out.push(span);
            continue;
        }
        let mut buf = String::new();
        for ch in span.content.chars() {
            if ch == '\t' {
                let spaces = tab_width - (col % tab_width);
                for _ in 0..spaces {
                    buf.push(' ');
                }
                col += spaces;
            } else {
                buf.push(ch);
                col += 1;
            }
        }
        out.push(Span::styled(buf, span.style));
    }
    out
}

/// Truncate a span list to at most `max_cols` visual columns (chars).
/// Returns the truncated spans plus a flag indicating whether truncation
/// happened, so callers can append their own ellipsis. Empty / over-budget
/// spans are dropped cleanly without producing zero-content spans.
fn truncate_spans_to_width<'a>(spans: Vec<Span<'a>>, max_cols: usize) -> (Vec<Span<'a>>, bool) {
    let mut out: Vec<Span<'a>> = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    let mut truncated = false;
    for span in spans {
        let span_len = span.content.chars().count();
        if used + span_len <= max_cols {
            used += span_len;
            out.push(span);
            continue;
        }
        // This span overflows — keep what fits, drop the rest.
        let remaining = max_cols.saturating_sub(used);
        if remaining > 0 {
            let head: String = span.content.chars().take(remaining).collect();
            out.push(Span::styled(head, span.style));
        }
        truncated = true;
        break;
    }
    (out, truncated)
}

/// Render the right-hand preview pane for the currently-selected annotation:
/// an accent-barred header (anchor + resolved/reply badges), followed by
/// a code snippet showing the diff lines being annotated, then each comment
/// in the thread with author/time and a wrapped body. Replies are visually
/// indented and prefixed with a thread guide.
fn render_annotation_preview(
    frame: &mut Frame,
    area: Rect,
    annotation: Option<&Annotation>,
    snippet: &[(usize, String)],
    t: &theme::Theme,
) {
    let Some(ann) = annotation else {
        // Center "no annotations" vertically as well — feels less stark
        // than pinning it to the top of an empty pane.
        let mid_y = area.y + area.height / 2;
        let row = Rect::new(area.x, mid_y, area.width, 1);
        let empty = Paragraph::new(Line::from(Span::styled(
            "— no annotations —",
            Style::default().fg(t.ui.text_muted).italic(),
        )))
        .alignment(ratatui::prelude::Alignment::Center);
        frame.render_widget(empty, row);
        return;
    };

    let lines = build_annotation_preview_lines(ann, snippet, area.width, t);

    // Clamp to the area height so we don't overflow.
    let visible: Vec<Line> = lines.into_iter().take(area.height as usize).collect();
    let para = Paragraph::new(visible);
    let inset = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    frame.render_widget(para, inset);
}

/// Build the line list used by both the right-pane preview and the
/// focused (full-modal) preview. Lifetime is `'static` because every span
/// owns its content via clone/format/string conversion.
fn build_annotation_preview_lines(
    ann: &Annotation,
    snippet: &[(usize, String)],
    area_width: u16,
    t: &theme::Theme,
) -> Vec<Line<'static>> {
    use crate::command::diff::state::AnnotationTarget;

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header row: accent bar + location + badges.
    let location = match &ann.target {
        AnnotationTarget::File => format!("{} · file", ann.filename),
        AnnotationTarget::LineRange { .. } => {
            format!("{} · {}", ann.filename, ann.line_range_display())
        }
    };
    let mut header_spans: Vec<Span> = vec![
        Span::styled("▎ ", Style::default().fg(t.ui.border_focused).bold()),
        Span::styled(location, Style::default().fg(t.ui.text_primary).bold()),
    ];
    if ann.resolved {
        header_spans.push(Span::styled("  ", Style::default()));
        header_spans.push(Span::styled(
            " ✓ resolved ",
            Style::default()
                .fg(t.ui.selection_fg)
                .bg(t.ui.status_added)
                .bold(),
        ));
    }
    let reply_count = ann.comments.len().saturating_sub(1);
    if reply_count > 0 {
        header_spans.push(Span::styled("  ", Style::default()));
        header_spans.push(Span::styled(
            format!(
                " ↳ {} {} ",
                reply_count,
                if reply_count == 1 { "reply" } else { "replies" }
            ),
            Style::default()
                .fg(t.ui.selection_fg)
                .bg(t.ui.highlight)
                .bold(),
        ));
    }
    lines.push(Line::from(header_spans));
    // Thin separator under the header.
    let sep_width = (area_width.saturating_sub(2) as usize).max(4);
    let separator = || -> Line<'static> {
        Line::from(Span::styled(
            "─".repeat(sep_width),
            Style::default().fg(t.ui.border_unfocused),
        ))
    };
    lines.push(separator());
    lines.push(Line::from(""));

    // Code snippet (only for line-range anchors with content available).
    // Lines inside the annotation's anchor range get a `▸` marker and full
    // colour; context lines get a blank prefix and muted colour, so the
    // reader can immediately tell which lines are being commented on.
    if !snippet.is_empty() {
        let (anchor_start, anchor_end) = match &ann.target {
            AnnotationTarget::LineRange {
                start_line,
                end_line,
                ..
            } => (*start_line, *end_line),
            AnnotationTarget::File => (0, 0),
        };
        let max_line_no = snippet.iter().map(|(n, _)| *n).max().unwrap_or(0);
        let gutter_w = max_line_no.to_string().chars().count().max(2);
        // Cap snippet height so a long anchor range doesn't push the
        // thread off-screen. If we cap, replace the middle with "…".
        const SNIPPET_MAX_ROWS: usize = 8;
        let render_snippet = if snippet.len() <= SNIPPET_MAX_ROWS {
            snippet.iter().cloned().map(Some).collect::<Vec<_>>()
        } else {
            let head = SNIPPET_MAX_ROWS / 2;
            let tail = SNIPPET_MAX_ROWS - head - 1;
            let mut out: Vec<Option<(usize, String)>> =
                snippet.iter().take(head).cloned().map(Some).collect();
            out.push(None); // ellipsis row
            out.extend(snippet.iter().skip(snippet.len() - tail).cloned().map(Some));
            out
        };
        // Width budget for the code column. The preview pane is rendered
        // into an inset (`area.x + 1`, width `area.width - 1`), so we have
        // `area.width - 1` columns to play with; subtract the row prefix to
        // get how many visual columns the code itself can occupy.
        let prefix_w = 1 + 1 + gutter_w + 3; // marker + sp + gutter + ' │ '
        let inset_width = (area_width as usize).saturating_sub(1);
        let content_w = inset_width.saturating_sub(prefix_w).max(8);
        const TAB_WIDTH: usize = 4;

        for entry in render_snippet {
            let Some((line_no, content)) = entry else {
                // Ellipsis row for the elided middle of a long snippet.
                lines.push(Line::from(Span::styled(
                    format!("{:>width$}", "…", width = prefix_w),
                    Style::default().fg(t.ui.text_muted),
                )));
                continue;
            };
            let in_anchor = line_no >= anchor_start && line_no <= anchor_end;
            let (marker, marker_style) = if in_anchor {
                ("▸", Style::default().fg(t.ui.highlight).bold())
            } else {
                (" ", Style::default())
            };
            let gutter_style = if in_anchor {
                Style::default().fg(t.ui.text_secondary)
            } else {
                Style::default().fg(t.ui.line_number)
            };

            // Strip trailing `\r` from CRLF line endings — leaving it in
            // would render as a control char and corrupt the row.
            let cleaned = content.trim_end_matches('\r');
            // Highlight the *full* line so multi-token syntax (strings,
            // identifiers) tokenises correctly, then expand tabs to spaces
            // (matching the diff view's behaviour) so column math lines up,
            // then truncate to the visible width. We reserve one col for
            // the ellipsis when truncation is needed so the total visible
            // width stays exactly at `content_w`.
            let highlighted = highlight_line_spans(cleaned, &ann.filename, None);
            let highlighted = expand_tabs_in_spans(highlighted, TAB_WIDTH);
            let total_cols: usize = highlighted.iter().map(|s| s.content.chars().count()).sum();
            let did_truncate = total_cols > content_w;
            let budget = if did_truncate {
                content_w.saturating_sub(1)
            } else {
                content_w
            };
            let (code_spans, _) = truncate_spans_to_width(highlighted, budget);

            let mut spans = vec![
                Span::styled(marker.to_string(), marker_style),
                Span::styled(" ", Style::default()),
                Span::styled(
                    format!("{:>width$}", line_no, width = gutter_w),
                    gutter_style,
                ),
                Span::styled(" │ ", Style::default().fg(t.ui.border_unfocused)),
            ];
            spans.extend(code_spans);
            if did_truncate {
                spans.push(Span::styled("…", Style::default().fg(t.ui.text_muted)));
            }
            lines.push(Line::from(spans));
        }
        lines.push(separator());
        lines.push(Line::from(""));
    }

    // Render every comment in the thread. Replies get an indent + thread
    // guide so it's obvious they're hanging off the root.
    for (i, comment) in ann.comments.iter().enumerate() {
        let is_reply = i > 0;
        let (header_prefix, body_prefix, wrap_width) = if is_reply {
            (
                vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("│ ", Style::default().fg(t.ui.border_unfocused)),
                    Span::styled("↳ ", Style::default().fg(t.ui.text_muted)),
                ],
                vec![
                    Span::styled("  ", Style::default()),
                    Span::styled("│ ", Style::default().fg(t.ui.border_unfocused)),
                    Span::styled("  ", Style::default()),
                ],
                (area_width.saturating_sub(8) as usize).max(10),
            )
        } else {
            (
                vec![Span::styled(
                    "● ",
                    Style::default().fg(t.ui.highlight).bold(),
                )],
                vec![Span::styled("  ", Style::default())],
                (area_width.saturating_sub(4) as usize).max(10),
            )
        };

        if is_reply {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled("│", Style::default().fg(t.ui.border_unfocused)),
            ]));
        }

        let mut header_line = header_prefix.clone();
        header_line.push(Span::styled(
            comment.author.login.clone(),
            Style::default().fg(t.ui.text_primary).bold(),
        ));
        header_line.push(Span::styled("  ", Style::default()));
        header_line.push(Span::styled(
            format_comment_time(comment.created_at),
            Style::default().fg(t.ui.text_muted),
        ));
        lines.push(Line::from(header_line));

        for body_line in comment.content.lines() {
            if body_line.is_empty() {
                lines.push(Line::from(body_prefix.clone()));
                continue;
            }
            for wrapped in wrap_preview_line(body_line, wrap_width) {
                let mut spans = body_prefix.clone();
                spans.push(Span::styled(
                    wrapped,
                    Style::default().fg(t.ui.text_primary),
                ));
                lines.push(Line::from(spans));
            }
        }
    }

    lines
}

/// Render the focused (full-modal) preview of a single annotation thread.
/// Layout mirrors the right-pane preview but takes the full inner area and
/// supports scrolling. `preview_scroll` is clamped against the freshly
/// computed visible/content heights so resizes never leave the view stuck
/// past the last line.
#[allow(clippy::too_many_arguments)]
fn render_annotations_focused(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    annotations: &[Annotation],
    snippets: &[Vec<(usize, String)>],
    selected: usize,
    preview_scroll: &mut u16,
    preview_content_height: &mut u16,
    preview_visible_height: &mut u16,
    reply_editor: Option<&AnnotationEditor<'static>>,
    t: &theme::Theme,
) {
    let annotation = annotations.get(selected);
    let title_line = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(title, Style::default().fg(t.ui.text_primary).bold()),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{}/{}", selected + 1, annotations.len().max(1)),
            Style::default().fg(t.ui.highlight).bold(),
        ),
        Span::styled(" thread ", Style::default().fg(t.ui.text_muted)),
    ]);

    let block = Block::default()
        .title(title_line)
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(t.ui.border_focused));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    use ratatui::layout::{Constraint, Direction, Layout};
    // When the inline reply composer is active, carve a fixed-height block
    // off the bottom of the inner area for it (above the footer). The
    // preview shrinks to fit so the thread + draft are visible together.
    const REPLY_BLOCK_HEIGHT: u16 = 7;
    let reply_visible = reply_editor.is_some();
    let constraints: Vec<Constraint> = if reply_visible {
        vec![
            Constraint::Min(1),
            Constraint::Length(REPLY_BLOCK_HEIGHT),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(1), Constraint::Length(1)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    let body_area = chunks[0];
    let (reply_area, footer_area) = if reply_visible {
        (Some(chunks[1]), chunks[2])
    } else {
        (None, chunks[1])
    };

    if let Some(ann) = annotation {
        let snippet = snippets.get(selected).map(|s| s.as_slice()).unwrap_or(&[]);
        // Reserve a 1-col gutter on the right for the scrollbar and a 1-col
        // inset on the left so text doesn't kiss the border.
        let content_width = body_area.width.saturating_sub(2);
        let lines = build_annotation_preview_lines(ann, snippet, content_width, t);

        let content_height = lines.len() as u16;
        let visible_height = body_area.height;
        let max_scroll = content_height.saturating_sub(visible_height);
        if *preview_scroll > max_scroll {
            *preview_scroll = max_scroll;
        }

        let inset = Rect::new(
            body_area.x + 1,
            body_area.y,
            body_area.width.saturating_sub(2),
            body_area.height,
        );
        let para = Paragraph::new(lines).scroll((*preview_scroll, 0));
        frame.render_widget(para, inset);

        if content_height > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█");

            let mut scrollbar_state =
                ScrollbarState::new((content_height.saturating_sub(visible_height)) as usize)
                    .position(*preview_scroll as usize);

            frame.render_stateful_widget(scrollbar, body_area, &mut scrollbar_state);
        }

        *preview_content_height = content_height;
        *preview_visible_height = visible_height;
    } else {
        let mid_y = body_area.y + body_area.height / 2;
        let row = Rect::new(body_area.x, mid_y, body_area.width, 1);
        let empty = Paragraph::new(Line::from(Span::styled(
            "— no annotations —",
            Style::default().fg(t.ui.text_muted).italic(),
        )))
        .alignment(ratatui::prelude::Alignment::Center);
        frame.render_widget(empty, row);
        *preview_content_height = 0;
        *preview_visible_height = body_area.height;
    }

    // Inline reply composer (bordered box). The editor owns its own
    // TextArea; we just provide the frame + label + footer hints.
    if let (Some(area), Some(editor)) = (reply_area, reply_editor) {
        let reply_block = Block::default()
            .title(" Reply ")
            .title_style(Style::default().fg(t.ui.text_secondary))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_focused));
        let reply_inner = reply_block.inner(area);
        frame.render_widget(reply_block, area);
        frame.render_widget(editor.textarea(), reply_inner);
    }

    // Footer with key hints for the focused view. When the reply composer
    // is active, the hints reflect the editor's own keys instead.
    let key_style = Style::default().fg(t.ui.highlight).bold();
    let desc_style = Style::default().fg(t.ui.text_muted);
    let sep_style = Style::default().fg(t.ui.border_unfocused);
    let footer_spans: Vec<Span> = if reply_visible {
        vec![
            Span::styled("↵", key_style),
            Span::styled(" send  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("ctrl+j", key_style),
            Span::styled(" newline  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("esc", key_style),
            Span::styled(" cancel", desc_style),
        ]
    } else {
        vec![
            Span::styled("h", key_style),
            Span::styled(" back  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("j/k", key_style),
            Span::styled(" scroll  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("r", key_style),
            Span::styled(" reply  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("↵", key_style),
            Span::styled(" jump  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("x", key_style),
            Span::styled(" resolve  ", desc_style),
            Span::styled("·  ", sep_style),
            Span::styled("esc", key_style),
            Span::styled(" close", desc_style),
        ]
    };
    let footer =
        Paragraph::new(Line::from(footer_spans)).alignment(ratatui::prelude::Alignment::Center);
    frame.render_widget(footer, footer_area);
}

fn format_comment_time(t: std::time::SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    format!("{:02}:{:02}", hours, minutes)
}

/// Word-wrap a single body line into chunks no wider than `width` chars,
/// breaking on word boundaries where possible.
fn wrap_preview_line(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width || width < 4 {
        return vec![line.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let needed = if current.is_empty() {
            word.chars().count()
        } else {
            current.chars().count() + 1 + word.chars().count()
        };
        if needed > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if word.chars().count() > width {
            // Hard-break a single overlong word.
            let mut chunk = String::new();
            for c in word.chars() {
                if chunk.chars().count() == width {
                    out.push(std::mem::take(&mut chunk));
                }
                chunk.push(c);
            }
            if !chunk.is_empty() {
                if current.is_empty() {
                    current = chunk;
                } else {
                    out.push(std::mem::take(&mut current));
                    current = chunk;
                }
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod revset_picker_tests {
    use super::*;

    #[test]
    fn reopening_selects_active_change_without_filtering_other_changes() {
        for (id, expected_index, expected_count) in [("vpsntpyqlqup", 1, 2), ("zzzzzzzzzzzz", 2, 3)]
        {
            let mut modal = Modal::revset_picker(
                "revset",
                vec![
                    RevsetPickerItem {
                        lines: None,
                        label: "working copy".into(),
                        revset: None,
                    },
                    RevsetPickerItem {
                        lines: None,
                        label: "listed change".into(),
                        revset: Some("vpsntpyq".into()),
                    },
                ],
            );
            modal.select_revset(RevsetPickerItem {
                lines: None,
                label: "active change".into(),
                revset: Some(id.into()),
            });
            assert!(matches!(
                &modal.content,
                ModalContent::RevsetPicker { selected, filtered_indices, items, query, .. }
                    if *selected == expected_index
                        && filtered_indices.len() == expected_count
                        && items.len() == expected_count
                        && query.is_empty()
            ));
            assert!(matches!(
                modal.handle_input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 40),
                Some(ModalResult::RevsetSelected { revset: Some(selected), literal: true })
                    if id.starts_with(&selected)
            ));
        }
    }

    #[test]
    fn range_expression_wins_over_description_match() {
        let mut modal = Modal::revset_picker(
            "revset",
            vec![RevsetPickerItem {
                lines: None,
                label: "wip: support trunk()..@".into(),
                revset: Some("abcdef".into()),
            }],
        );
        for c in "trunk()..@".chars() {
            modal.handle_input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), 40);
        }
        assert!(
            matches!(modal.handle_input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 40),
            Some(ModalResult::RevsetSelected { revset: Some(raw), literal: true }) if raw == "trunk()..@")
        );
    }

    #[test]
    fn prefix_preview_highlights_resolved_change_and_clears_ambiguity() {
        let mut modal = Modal::revset_picker(
            "revset",
            vec![
                RevsetPickerItem {
                    lines: None,
                    label: "vqpsvyom".into(),
                    revset: Some("vqpsvyom".into()),
                },
                RevsetPickerItem {
                    lines: None,
                    label: "vpsntpyq".into(),
                    revset: Some("vpsntpyq".into()),
                },
            ],
        );
        for c in "vp".chars() {
            modal.handle_input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), 40);
        }
        modal.update_prefix_preview(|query| {
            assert_eq!(query, "vp");
            Ok(RevsetPickerItem {
                lines: None,
                label: "resolved".into(),
                revset: Some("vpsntpyqlqup".into()),
            })
        });
        assert!(
            matches!(&modal.content, ModalContent::RevsetPicker { filtered_indices, selected: 0, error: None, .. } if filtered_indices == &[1])
        );
        modal.update_prefix_preview(|_| Err("ambiguous prefix".into()));
        assert!(
            matches!(&modal.content, ModalContent::RevsetPicker { filtered_indices, error: Some(_), .. } if filtered_indices.is_empty())
        );
        modal.update_prefix_preview(|_| {
            Ok(RevsetPickerItem {
                lines: None,
                label: "outside initial rows".into(),
                revset: Some("vpxxxxx".into()),
            })
        });
        assert!(
            matches!(&modal.content, ModalContent::RevsetPicker { filtered_indices, selected: 0, error: None, .. } if filtered_indices == &[2])
        );
    }

    #[test]
    fn typed_prefix_wins_over_fuzzy_row_match() {
        for query in ["vp", "VP", "v", "zz"] {
            let mut modal = Modal::revset_picker(
                "revset",
                vec![
                    RevsetPickerItem {
                        lines: None,
                        label: "vqpsvyom current".into(),
                        revset: Some("vqpsvyom".into()),
                    },
                    RevsetPickerItem {
                        lines: None,
                        label: "vpsntpyq explorer".into(),
                        revset: Some("vpsntpyq".into()),
                    },
                ],
            );
            for c in query.chars() {
                modal.handle_input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), 40);
            }
            assert!(
                matches!(modal.handle_input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 40),
                Some(ModalResult::RevsetSelected { revset: Some(id), literal: true }) if id == query)
            );
        }
    }

    #[test]
    fn selecting_a_change_requests_immutable_review() {
        let mut modal = Modal::revset_picker(
            "revset",
            vec![
                RevsetPickerItem {
                    lines: None,
                    label: "working copy".into(),
                    revset: None,
                },
                RevsetPickerItem {
                    lines: None,
                    label: "older change".into(),
                    revset: Some("abc".into()),
                },
            ],
        );
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            modal.handle_input(enter, 40),
            Some(ModalResult::RevsetSelected {
                revset: None,
                literal: false
            })
        ));
        modal.handle_input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 40);
        assert!(
            matches!(modal.handle_input(enter, 40), Some(ModalResult::RevsetSelected { revset: Some(id), literal: true }) if id == "abc")
        );
    }
}
