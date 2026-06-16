use std::collections::{HashMap, HashSet};

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

use crate::command::diff::context::{compute_context_lines, ContextLine};
use crate::command::diff::highlight::{highlight_line_spans, FileHighlighter};
use crate::command::diff::search::{MatchPanel, SearchState};
use crate::command::diff::state::{Annotation, AnnotationId, AnnotationTarget, RefInputState};
use crate::command::diff::theme;
use crate::command::diff::types::{
    ChangeType, DiffFullscreen, DiffLine, DiffPanelFocus, DiffViewSettings, FileDiff, FocusedPanel,
    InlineSegment, Selection, SelectionMode, SidebarItem,
};

use super::footer::{render_footer, FooterData};
use super::sidebar::render_sidebar;

/// Render the header bar for stacked diff mode showing commit info with navigation arrows
fn render_stacked_header(
    frame: &mut Frame,
    area: Rect,
    commit: Option<&StackedCommitInfo>,
    index: usize,
    total: usize,
) {
    let t = theme::get();
    let bg = t.ui.bg;

    let can_go_prev = index > 0;
    let can_go_next = index < total.saturating_sub(1);

    // Styles for arrows and hints
    let active_style = Style::default().fg(t.ui.text_primary).bg(bg);
    let dimmed_style = Style::default().fg(t.ui.text_muted).bg(bg);

    let left_style = if can_go_prev {
        active_style
    } else {
        dimmed_style
    };
    let right_style = if can_go_next {
        active_style
    } else {
        dimmed_style
    };

    let (commit_id_label, commit_msg) = if let Some(c) = commit {
        (c.short_id.clone(), c.summary.clone())
    } else {
        ("?".to_string(), "No commit".to_string())
    };

    // Build center content: [1/6]  id  message
    let nav_indicator = format!(" {}/{} ", index + 1, total);
    let id_label = format!(" {} ", commit_id_label);

    // Reserve space for arrows, hints, navigation, and id
    let available_for_msg =
        (area.width as usize).saturating_sub(60 + nav_indicator.len() + id_label.len());

    let truncated_msg = if commit_msg.len() > available_for_msg {
        format!(
            "{}...",
            &commit_msg[..available_for_msg.saturating_sub(3).max(0)]
        )
    } else {
        commit_msg
    };

    // Build center spans: [1/6] [id] message
    let badge_style = Style::default().bg(t.ui.footer_branch_bg);
    let spacer_style = Style::default().bg(bg);
    let center_spans = vec![
        Span::styled(&nav_indicator, badge_style.fg(t.ui.highlight)),
        Span::styled(" ", spacer_style),
        Span::styled(&id_label, badge_style.fg(t.ui.footer_branch_fg)),
        Span::styled("  ", spacer_style),
        Span::styled(
            &truncated_msg,
            Style::default().fg(t.ui.text_secondary).bg(bg),
        ),
    ];

    // Calculate widths for centering
    let center_width: usize =
        nav_indicator.len() + 1 + id_label.len() + 2 + truncated_msg.chars().count();
    // " ‹ " + " ctrl+h " = 12 chars, same for right side
    let side_width = 12;

    let total_content_width = side_width * 2 + center_width;
    let total_padding = (area.width as usize).saturating_sub(total_content_width);
    let left_padding = total_padding / 2;
    let right_padding = total_padding - left_padding;

    // Build final line with centered content
    let mut spans = vec![
        // Left side: arrow and hint
        Span::styled(" ‹ ", left_style),
        Span::styled(" ctrl+h ", dimmed_style),
        // Left padding
        Span::styled(" ".repeat(left_padding), Style::default().bg(bg)),
    ];

    // Add center content
    spans.extend(center_spans);

    // Right padding and right side
    spans.push(Span::styled(
        " ".repeat(right_padding),
        Style::default().bg(bg),
    ));
    spans.push(Span::styled(" ctrl+l ", dimmed_style));
    spans.push(Span::styled(" › ", right_style));

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(bg));
    frame.render_widget(header, area);
}

/// Generates a diagonal stripe pattern for empty placeholder lines in the diff view.
/// The pattern uses forward slashes to create a visual distinction for empty areas.
fn generate_stripe_pattern(width: usize) -> String {
    "╱".repeat(width)
}

/// Append a trailing span of bg-colored spaces so the line fills `target_width` cells.
/// Used so diff line backgrounds extend to the right edge of the panel even when the
/// content is shorter than the viewport or scrolled horizontally.
fn pad_line_bg<'a>(mut spans: Vec<Span<'a>>, target_width: usize, bg: Color) -> Vec<Span<'a>> {
    let current: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if current < target_width {
        spans.push(Span::styled(
            " ".repeat(target_width - current),
            Style::default().bg(bg),
        ));
    }
    spans
}

pub struct LineStats {
    pub added: usize,
    pub removed: usize,
}

fn expand_tabs_in_spans<'a>(spans: Vec<Span<'a>>, tab_width: usize) -> Vec<Span<'a>> {
    if tab_width == 0 {
        let mut col = 0;
        return spans
            .into_iter()
            .map(|span| {
                if !span.content.contains('\t') {
                    col += span.content.chars().count();
                    return span;
                }
                let mut buf = String::new();
                for ch in span.content.chars() {
                    if ch == '\t' {
                        continue;
                    }
                    buf.push(ch);
                    col += 1;
                }
                Span::styled(buf, span.style)
            })
            .collect();
    }

    let mut col = 0;
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

fn apply_search_highlight<'a>(
    text: &str,
    filename: &str,
    bg: Option<Color>,
    match_ranges: &[(usize, usize, bool)],
    highlighter: Option<&FileHighlighter>,
    line_number: Option<usize>,
    tab_width: usize,
) -> Vec<Span<'a>> {
    let t = theme::get();

    // Use FileHighlighter if available for proper multi-line construct highlighting
    let base_spans = if let (Some(hl), Some(line_num)) = (highlighter, line_number) {
        let spans = hl.get_line_spans(line_num, bg);
        if spans.is_empty() {
            // Fallback if highlighter doesn't have this line
            highlight_line_spans(text, filename, bg)
        } else {
            spans
        }
    } else {
        highlight_line_spans(text, filename, bg)
    };
    let base_spans = expand_tabs_in_spans(base_spans, tab_width);

    if match_ranges.is_empty() {
        return base_spans;
    }
    let mut result: Vec<Span<'a>> = Vec::new();
    let mut char_pos = 0;

    for span in base_spans {
        let span_text = span.content.to_string();
        let span_len = span_text.len();
        let span_end = char_pos + span_len;

        let mut current_pos = 0;
        let mut remaining = span_text.as_str();

        for &(match_start, match_end, is_current) in match_ranges {
            if match_end <= char_pos || match_start >= span_end {
                continue;
            }

            let rel_start = match_start.saturating_sub(char_pos);
            let rel_end = (match_end - char_pos).min(span_len);

            if rel_start > current_pos {
                let before = &remaining[..(rel_start - current_pos)];
                if !before.is_empty() {
                    result.push(Span::styled(before.to_string(), span.style));
                }
            }

            let match_portion_start = rel_start.max(current_pos) - current_pos;
            let match_portion_end = rel_end - current_pos;
            if match_portion_end > match_portion_start {
                let match_text = &remaining[match_portion_start..match_portion_end];
                if !match_text.is_empty() {
                    let (fg, bg) = if is_current {
                        (t.ui.search_current_fg, t.ui.search_current_bg)
                    } else {
                        (t.ui.search_match_fg, t.ui.search_match_bg)
                    };
                    result.push(Span::styled(
                        match_text.to_string(),
                        Style::default().fg(fg).bg(bg).bold(),
                    ));
                }
            }

            remaining = &remaining[(rel_end - current_pos).min(remaining.len())..];
            current_pos = rel_end;
        }

        if !remaining.is_empty() {
            result.push(Span::styled(remaining.to_string(), span.style));
        }

        char_pos = span_end;
    }

    result
}

/// Convert InlineSegments to emphasis ranges (start, end) positions.
fn segments_to_emphasis_ranges(segments: &[InlineSegment]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut pos = 0;
    for segment in segments {
        let len = segment.text.len();
        if segment.emphasized {
            ranges.push((pos, pos + len));
        }
        pos += len;
    }
    ranges
}

/// Check if a color is "muted" (low luminosity) and would have poor contrast
/// on a colored background. Returns true for grays and dark colors.
fn is_muted_color(color: Color) -> bool {
    match color {
        Color::Rgb(r, g, b) => {
            // Calculate relative luminance (simplified)
            let luminance = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
            // Also check if it's grayish (low saturation)
            let max = r.max(g).max(b);
            let min = r.min(g).min(b);
            let saturation = if max == 0 {
                0
            } else {
                (max - min) as u32 * 100 / max as u32
            };
            // Muted = low luminance OR (medium luminance AND low saturation)
            luminance < 140 || (luminance < 180 && saturation < 30)
        }
        Color::DarkGray | Color::Gray => true,
        _ => false,
    }
}

/// Boost a muted foreground color to improve contrast on emphasized backgrounds.
fn boost_muted_fg(fg: Color, default_text: Color) -> Color {
    if is_muted_color(fg) {
        // Use a brighter version - the default text color is usually good
        default_text
    } else {
        fg
    }
}

/// Apply syntax highlighting with word-level emphasis backgrounds.
/// This preserves syntax colors while overlaying emphasis backgrounds for changed words.
fn apply_word_emphasis_highlight<'a>(
    text: &str,
    filename: &str,
    line_bg: Option<Color>,
    word_emphasis_bg: Color,
    emphasis_ranges: &[(usize, usize)],
    search_ranges: &[(usize, usize, bool)],
    highlighter: Option<&FileHighlighter>,
    line_number: Option<usize>,
    tab_width: usize,
) -> Vec<Span<'a>> {
    let t = theme::get();

    // Get syntax-highlighted base spans
    let base_spans = if let (Some(hl), Some(line_num)) = (highlighter, line_number) {
        let spans = hl.get_line_spans(line_num, line_bg);
        if spans.is_empty() {
            highlight_line_spans(text, filename, line_bg)
        } else {
            spans
        }
    } else {
        highlight_line_spans(text, filename, line_bg)
    };
    let base_spans = expand_tabs_in_spans(base_spans, tab_width);

    if emphasis_ranges.is_empty() && search_ranges.is_empty() {
        return base_spans;
    }

    let mut result: Vec<Span<'a>> = Vec::new();
    let mut byte_pos = 0;

    for span in base_spans {
        let span_text = span.content.to_string();
        let span_byte_len = span_text.len();

        // Build a list of (byte_offset, char) for safe UTF-8 iteration
        let char_indices: Vec<(usize, char)> = span_text.char_indices().collect();
        if char_indices.is_empty() {
            byte_pos += span_byte_len;
            continue;
        }

        let mut idx = 0;
        while idx < char_indices.len() {
            let (byte_offset, _) = char_indices[idx];
            let global_pos = byte_pos + byte_offset;

            // Check if we're in a search match (takes priority)
            let search_match = search_ranges
                .iter()
                .find(|(start, end, _)| global_pos >= *start && global_pos < *end);

            // Check if we're in an emphasis range
            let in_emphasis = emphasis_ranges
                .iter()
                .any(|(start, end)| global_pos >= *start && global_pos < *end);

            // Determine background and style for this character
            let (bg, fg, bold) = if let Some((_, _, is_current)) = search_match {
                if *is_current {
                    (t.ui.search_current_bg, t.ui.search_current_fg, true)
                } else {
                    (t.ui.search_match_bg, t.ui.search_match_fg, true)
                }
            } else if in_emphasis {
                // Emphasis: use word highlight background, boost muted fg colors for contrast
                let original_fg = span.style.fg.unwrap_or(t.syntax.default_text);
                let boosted_fg = boost_muted_fg(original_fg, t.syntax.default_text);
                (word_emphasis_bg, boosted_fg, false)
            } else {
                // Normal: use line background with original foreground
                (
                    span.style.bg.unwrap_or(line_bg.unwrap_or(Color::Reset)),
                    span.style.fg.unwrap_or(t.syntax.default_text),
                    false,
                )
            };

            // Find the end of this run (same style)
            let mut run_end_idx = idx + 1;
            while run_end_idx < char_indices.len() {
                let (next_byte_offset, _) = char_indices[run_end_idx];
                let next_global_pos = byte_pos + next_byte_offset;

                let next_search = search_ranges
                    .iter()
                    .find(|(start, end, _)| next_global_pos >= *start && next_global_pos < *end);

                let next_in_emphasis = emphasis_ranges
                    .iter()
                    .any(|(start, end)| next_global_pos >= *start && next_global_pos < *end);

                let same_style = match (search_match, next_search) {
                    (Some((_, _, c1)), Some((_, _, c2))) => c1 == c2,
                    (None, None) => in_emphasis == next_in_emphasis,
                    _ => false,
                };

                if !same_style {
                    break;
                }
                run_end_idx += 1;
            }

            // Get the byte range for this run
            let run_start_byte = byte_offset;
            let run_end_byte = if run_end_idx < char_indices.len() {
                char_indices[run_end_idx].0
            } else {
                span_byte_len
            };

            // Push this run
            let run_text = &span_text[run_start_byte..run_end_byte];
            let mut style = Style::default().fg(fg).bg(bg);
            if bold {
                style = style.bold();
            }
            result.push(Span::styled(run_text.to_string(), style));

            idx = run_end_idx;
        }

        byte_pos += span_byte_len;
    }

    result
}

/// Selection tint color - a subtle blue that blends with any diff background
const SELECTION_TINT: Color = Color::Rgb(80, 120, 180);
const SELECTION_ALPHA: f32 = 0.4;

/// Blend a base background color with a selection tint.
#[inline]
fn blend_with_selection(base: Color) -> Color {
    match base {
        Color::Rgb(br, bg, bb) => {
            let Color::Rgb(sr, sg, sb) = SELECTION_TINT else {
                return base;
            };
            let r = ((br as f32) * (1.0 - SELECTION_ALPHA) + (sr as f32) * SELECTION_ALPHA) as u8;
            let g = ((bg as f32) * (1.0 - SELECTION_ALPHA) + (sg as f32) * SELECTION_ALPHA) as u8;
            let b = ((bb as f32) * (1.0 - SELECTION_ALPHA) + (sb as f32) * SELECTION_ALPHA) as u8;
            Color::Rgb(r, g, b)
        }
        _ => SELECTION_TINT,
    }
}

/// Check if a line position is within the selection range for a given panel.
/// Returns the column range that's selected on this line, or None if not selected.
#[inline]
fn get_selection_range_for_line(
    line_idx: usize,
    panel: DiffPanelFocus,
    selection: &Selection,
) -> Option<(usize, usize)> {
    if !selection.is_active() || selection.panel != panel {
        return None;
    }

    let (start, end) = selection.normalized_range();

    if line_idx < start.line || line_idx > end.line {
        return None;
    }

    match selection.mode {
        SelectionMode::Line => {
            // Line mode: entire line is selected
            Some((0, usize::MAX))
        }
        SelectionMode::Character => {
            if start.line == end.line {
                // Single line selection
                Some((start.column, end.column))
            } else if line_idx == start.line {
                // First line of multi-line selection
                Some((start.column, usize::MAX))
            } else if line_idx == end.line {
                // Last line of multi-line selection
                Some((0, end.column))
            } else {
                // Middle line - entire line selected
                Some((0, usize::MAX))
            }
        }
        SelectionMode::None => None,
    }
}

/// Apply selection highlighting to spans. Only processes spans if selection is active.
/// For efficiency, spans fully outside the selection range are passed through unchanged.
#[inline]
fn apply_selection_to_spans<'a>(
    spans: Vec<Span<'a>>,
    selection_range: Option<(usize, usize)>,
    default_bg: Color,
) -> Vec<Span<'a>> {
    let Some((sel_start, sel_end)) = selection_range else {
        return spans;
    };

    // Line mode or full line selected - apply to all spans (fast path)
    if sel_start == 0 && sel_end == usize::MAX {
        return spans
            .into_iter()
            .map(|span| {
                let bg = span.style.bg.unwrap_or(default_bg);
                Span::styled(span.content, span.style.bg(blend_with_selection(bg)))
            })
            .collect();
    }

    // Character mode - need to apply selection per-character
    let mut result = Vec::with_capacity(spans.len() * 2);
    let mut col = 0usize;

    for span in spans {
        let text = span.content.to_string();
        let span_len = text.chars().count();
        let span_end = col + span_len;

        // Fast path: span fully before or after selection
        if span_end <= sel_start || col >= sel_end {
            result.push(Span::styled(text, span.style));
            col = span_end;
            continue;
        }

        // Span intersects with selection - need to split
        let bg = span.style.bg.unwrap_or(default_bg);
        let selected_bg = blend_with_selection(bg);
        let chars: Vec<char> = text.chars().collect();

        // Part before selection
        if col < sel_start {
            let before_len = sel_start - col;
            let before: String = chars[..before_len].iter().collect();
            result.push(Span::styled(before, span.style));
        }

        // Selected part
        let sel_start_in_span = sel_start.saturating_sub(col);
        let sel_end_in_span = (sel_end - col).min(span_len);
        if sel_start_in_span < sel_end_in_span {
            let selected: String = chars[sel_start_in_span..sel_end_in_span].iter().collect();
            result.push(Span::styled(selected, span.style.bg(selected_bg)));
        }

        // Part after selection
        if sel_end < span_end {
            let after_start = sel_end - col;
            let after: String = chars[after_start..].iter().collect();
            result.push(Span::styled(after, span.style));
        }

        col = span_end;
    }

    result
}

pub fn compute_line_stats(side_by_side: &[DiffLine]) -> LineStats {
    let mut added = 0;
    let mut removed = 0;
    for line in side_by_side {
        match line.change_type {
            ChangeType::Insert => added += 1,
            ChangeType::Delete => removed += 1,
            ChangeType::Modified => {
                added += 1;
                removed += 1;
            }
            ChangeType::Equal => {}
        }
    }
    LineStats { added, removed }
}

/// Style configuration for rendering a diff line's gutter and background.
struct DiffLineStyle {
    old_bg: Option<Color>,
    old_gutter_bg: Option<Color>,
    old_gutter_fg: Option<Color>,
    new_bg: Option<Color>,
    new_gutter_bg: Option<Color>,
    new_gutter_fg: Option<Color>,
}

impl DiffLineStyle {
    fn for_change_type(
        change_type: ChangeType,
        bg: Color,
        t: &crate::command::diff::theme::Theme,
    ) -> Self {
        match change_type {
            ChangeType::Equal => Self {
                old_bg: Some(bg),
                old_gutter_bg: Some(bg),
                old_gutter_fg: Some(t.ui.line_number),
                new_bg: Some(bg),
                new_gutter_bg: Some(bg),
                new_gutter_fg: Some(t.ui.line_number),
            },
            ChangeType::Delete => Self {
                old_bg: Some(t.diff.deleted_bg),
                old_gutter_bg: Some(t.diff.deleted_gutter_bg),
                old_gutter_fg: Some(t.diff.deleted_gutter_fg),
                new_bg: None,
                new_gutter_bg: None,
                new_gutter_fg: None,
            },
            ChangeType::Insert => Self {
                old_bg: None,
                old_gutter_bg: None,
                old_gutter_fg: None,
                new_bg: Some(t.diff.added_bg),
                new_gutter_bg: Some(t.diff.added_gutter_bg),
                new_gutter_fg: Some(t.diff.added_gutter_fg),
            },
            ChangeType::Modified => Self {
                old_bg: Some(t.diff.deleted_bg),
                old_gutter_bg: Some(t.diff.deleted_gutter_bg),
                old_gutter_fg: Some(t.diff.deleted_gutter_fg),
                new_bg: Some(t.diff.added_bg),
                new_gutter_bg: Some(t.diff.added_gutter_bg),
                new_gutter_fg: Some(t.diff.added_gutter_fg),
            },
        }
    }
}

pub fn render_empty_state(frame: &mut Frame, watching: bool, ref_input: &RefInputState) {
    let area = frame.area();
    let (msg_area, footer_area) = if ref_input.active {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let watch_hint = if watching {
        " (watching for changes...)"
    } else {
        ""
    };
    let msg = Paragraph::new(format!("No changes detected.{}", watch_hint))
        .block(Block::default().title(" Git Review ").borders(Borders::ALL));
    frame.render_widget(msg, msg_area);

    if let Some(footer_area) = footer_area {
        let empty_search = SearchState::default();
        let empty_files: HashSet<usize> = HashSet::new();
        render_footer(
            frame,
            footer_area,
            FooterData {
                filename: "",
                commit_ref: "",
                watching,
                current_file: 0,
                viewed_files: &empty_files,
                line_stats_added: 0,
                line_stats_removed: 0,
                hunk_count: 0,
                focused_hunk: None,
                search_state: &empty_search,
                ref_input,
                area_width: area.width,
                annotation_source_label: None,
                annotation_source_error: None,
                annotation_select_active: false,
                loading_message: None,
            },
        );
    }
}

fn render_context_lines(
    context: &[ContextLine],
    total_count: usize,
    lines: &mut Vec<Line>,
    filename: &str,
    highlighter: &FileHighlighter,
    tab_width: usize,
) {
    let t = theme::get();
    let context_bg = t.diff.context_bg;

    for i in 0..total_count {
        if let Some(cl) = context.get(i) {
            let prefix = format!("{:4} ~ ", cl.line_number);
            let mut spans: Vec<Span> = vec![Span::styled(
                prefix,
                Style::default().fg(t.ui.line_number).bg(context_bg),
            )];
            // Use FileHighlighter for proper multi-line construct highlighting
            let hl_spans = expand_tabs_in_spans(
                highlighter.get_line_spans(cl.line_number, Some(context_bg)),
                tab_width,
            );
            if hl_spans.is_empty() {
                // Fallback to line-by-line highlighting
                spans.extend(highlight_line_spans(
                    &cl.content,
                    filename,
                    Some(context_bg),
                ));
            } else {
                spans.extend(hl_spans);
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![Span::styled(
                "     ~".to_string(),
                Style::default().fg(t.ui.line_number).bg(context_bg),
            )]));
        }
    }
}

use crate::vcs::StackedCommitInfo;

/// Compute side_by_side index ranges for line-range annotations.
/// Returns `(first_sbs_idx, last_sbs_idx, panel)` for each annotation that has matching lines.
fn compute_ann_index_ranges(
    line_annotations: &[&Annotation],
    side_by_side: &[DiffLine],
) -> Vec<(usize, usize, DiffPanelFocus)> {
    let mut ranges = Vec::new();
    for ann in line_annotations {
        if let AnnotationTarget::LineRange {
            panel,
            start_line,
            end_line,
            ..
        } = &ann.target
        {
            let mut first_idx: Option<usize> = None;
            let mut last_idx: Option<usize> = None;
            for (idx, dl) in side_by_side.iter().enumerate() {
                if let Some(n) = dl.line_number(*panel) {
                    if n >= *start_line && n <= *end_line {
                        if first_idx.is_none() {
                            first_idx = Some(idx);
                        }
                        last_idx = Some(idx);
                    }
                }
            }
            if let (Some(first), Some(last)) = (first_idx, last_idx) {
                ranges.push((first, last, *panel));
            }
        }
    }
    ranges
}

/// Check if a side_by_side index falls within any annotation range, optionally filtering by panel.
fn is_in_ann_range(
    sbs_idx: usize,
    panel: Option<DiffPanelFocus>,
    ranges: &[(usize, usize, DiffPanelFocus)],
) -> bool {
    ranges.iter().any(|(first, last, ann_panel)| {
        sbs_idx >= *first && sbs_idx <= *last && panel.map_or(true, |p| *ann_panel == p)
    })
}

/// Side_by_side index range for the annotation currently selected via
/// annotation-select mode, if any. Used to highlight that annotation's
/// gutter marker distinctly from the others.
fn selected_ann_range(
    selected_id: Option<&AnnotationId>,
    line_annotations: &[&Annotation],
    side_by_side: &[DiffLine],
) -> Option<(usize, usize, DiffPanelFocus)> {
    let id = selected_id?;
    let ann = line_annotations.iter().find(|a| &a.id == id)?;
    if let AnnotationTarget::LineRange {
        panel,
        start_line,
        end_line,
        ..
    } = &ann.target
    {
        let mut first_idx: Option<usize> = None;
        let mut last_idx: Option<usize> = None;
        for (idx, dl) in side_by_side.iter().enumerate() {
            if let Some(n) = dl.line_number(*panel) {
                if n >= *start_line && n <= *end_line {
                    if first_idx.is_none() {
                        first_idx = Some(idx);
                    }
                    last_idx = Some(idx);
                }
            }
        }
        return first_idx.zip(last_idx).map(|(f, l)| (f, l, *panel));
    }
    None
}

fn is_in_selected_ann_range(
    sbs_idx: usize,
    panel: Option<DiffPanelFocus>,
    range: Option<(usize, usize, DiffPanelFocus)>,
) -> bool {
    if let Some((first, last, ann_panel)) = range {
        sbs_idx >= first && sbs_idx <= last && panel.map_or(true, |p| ann_panel == p)
    } else {
        false
    }
}

/// Build the focus/annotation indicator span for a diff line.
fn make_indicator_span(
    in_focused: bool,
    in_annotation: bool,
    in_selected_annotation: bool,
    line_selected: bool,
    bg: Color,
    focus_style: Style,
    annotation_style: Style,
) -> Span<'static> {
    let indicator = if in_selected_annotation {
        // Stronger marker for the annotation targeted in annotation-select
        // mode so it's visually distinct from other annotations and from
        // the hunk focus indicator.
        Span::styled("▌", annotation_style.add_modifier(Modifier::BOLD))
    } else if in_focused {
        Span::styled("▎", focus_style)
    } else if in_annotation {
        Span::styled("▍", annotation_style)
    } else {
        Span::styled(" ", Style::default())
    };
    if line_selected {
        let ind_bg = indicator.style.bg.unwrap_or(bg);
        Span::styled(
            indicator.content,
            indicator.style.bg(blend_with_selection(ind_bg)),
        )
    } else {
        indicator
    }
}

/// One row of the inner (between borders) area of an annotation overlay.
///
/// Built from the annotation's comments: the root's content lines, then for
/// each reply a `Separator` row labelling the author/time followed by the
/// reply's content lines.
enum OverlayRow {
    Text(String),
    Separator(String),
}

fn annotation_text_width(content_width: u16) -> usize {
    content_width.saturating_sub(4).max(1) as usize
}

fn split_at_char_count(s: &str, count: usize) -> (&str, &str) {
    if count == 0 {
        return ("", s);
    }

    match s.char_indices().nth(count) {
        Some((idx, _)) => s.split_at(idx),
        None => (s, ""),
    }
}

fn wrap_annotation_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.trim().is_empty() {
        return vec![String::new()];
    }

    let mut rows = Vec::new();
    let mut current = String::new();

    for word in line.split_whitespace() {
        let mut remaining = word;
        while !remaining.is_empty() {
            if current.is_empty() {
                if remaining.chars().count() <= width {
                    current.push_str(remaining);
                    break;
                }

                let (chunk, rest) = split_at_char_count(remaining, width);
                rows.push(chunk.to_string());
                remaining = rest;
            } else if current.chars().count() + 1 + remaining.chars().count() <= width {
                current.push(' ');
                current.push_str(remaining);
                break;
            } else {
                rows.push(std::mem::take(&mut current));
            }
        }
    }

    if !current.is_empty() {
        rows.push(current);
    }

    if rows.is_empty() {
        rows.push(String::new());
    }

    rows
}

fn wrap_annotation_text(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        rows.extend(wrap_annotation_line(line, width));
    }

    if rows.is_empty() {
        rows.push(String::new());
    }

    rows
}

fn fit_to_char_width(text: &str, width: usize) -> String {
    let mut fitted: String = text.chars().take(width).collect();
    let fitted_len = fitted.chars().count();
    if fitted_len < width {
        fitted.push_str(&" ".repeat(width - fitted_len));
    }
    fitted
}

/// Build the inner rows of an annotation overlay: root content + one separator
/// per reply followed by the reply content.
fn annotation_inner_rows(annotation: &Annotation, text_width: usize) -> Vec<OverlayRow> {
    let mut rows: Vec<OverlayRow> = Vec::new();
    for line in wrap_annotation_text(&annotation.root().content, text_width) {
        rows.push(OverlayRow::Text(line));
    }
    for reply in annotation.comments.iter().skip(1) {
        let time_str = format_clock(reply.created_at);
        let label = format!("@{} · {}", reply.author.login, time_str);
        rows.push(OverlayRow::Separator(label));
        for line in wrap_annotation_text(&reply.content, text_width) {
            rows.push(OverlayRow::Text(line));
        }
    }
    rows
}

/// Total rendered height of a single annotation overlay (inner rows + borders).
/// Resolved annotations collapse to a single row to minimise diff clutter.
fn annotation_overlay_height(annotation: &Annotation, content_width: u16) -> usize {
    if annotation.resolved {
        return 1;
    }
    annotation_inner_rows(annotation, annotation_text_width(content_width)).len() + 2
}

/// HH:MM formatter mirroring `Annotation::format_time` but for a single comment.
fn format_clock(t: std::time::SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let duration = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    format!("{:02}:{:02}", hours, minutes)
}

/// Per-line overlay heights for line-range annotations, keyed by side_by_side
/// index (the anchor row where the overlay is drawn — the index whose line
/// matches the annotation's `end_line` on the configured panel). Used by
/// scroll handlers to drive sub-row snap scrolling regardless of whether the
/// overlay is currently in the viewport.
fn compute_overlay_heights(
    line_annotations: &[&Annotation],
    side_by_side: &[DiffLine],
    old_area: Option<Rect>,
    new_area: Option<Rect>,
    main_area: Rect,
) -> HashMap<usize, usize> {
    let mut heights: HashMap<usize, usize> = HashMap::new();
    for ann in line_annotations {
        if let AnnotationTarget::LineRange {
            panel, end_line, ..
        } = &ann.target
        {
            let width = overlay_content_width(ann, old_area, new_area, main_area);
            let h = annotation_overlay_height(ann, width);
            for (idx, dl) in side_by_side.iter().enumerate() {
                if dl.line_number(*panel) == Some(*end_line) {
                    *heights.entry(idx).or_insert(0) += h;
                    break;
                }
            }
        }
    }
    heights
}

/// Total rendered height of file-level annotation overlays.
fn file_annotation_height(annotations: &[&Annotation], content_width: u16) -> usize {
    annotations
        .iter()
        .copied()
        .map(|annotation| annotation_overlay_height(annotation, content_width))
        .sum()
}

fn overlay_content_width(
    annotation: &Annotation,
    old_area: Option<Rect>,
    new_area: Option<Rect>,
    main_area: Rect,
) -> u16 {
    let is_side_by_side = old_area.is_some() && new_area.is_some();
    match &annotation.target {
        AnnotationTarget::File => {
            let render_area = old_area.or(new_area).unwrap_or(main_area);
            if is_side_by_side {
                old_area
                    .unwrap()
                    .width
                    .saturating_add(new_area.unwrap().width)
                    .saturating_sub(2)
            } else {
                render_area.width.saturating_sub(2)
            }
        }
        AnnotationTarget::LineRange { panel, .. } => {
            let area = match panel {
                DiffPanelFocus::Old => old_area.unwrap_or(main_area),
                _ => new_area.unwrap_or(main_area),
            };
            area.width.saturating_sub(2)
        }
    }
}

/// Render annotation overlays at specified positions.
///
/// This function renders annotation boxes that can span single or multiple panels.
/// The `content_x`, `content_start_y`, `content_width`, and `max_area` parameters
/// allow flexible positioning for both single-panel and side-by-side views.
// Each `overlays` entry is `(line_pos, annotation, shift)`:
// - `line_pos` is the paragraph-relative row where the overlay's first row sits at shift=0.
// - `shift` is the number of rows to scroll this overlay up. The overlay's top is clipped
//   off `min_y` when `shift > 0`; only the tail starting from row `shift` is drawn. File-level
//   overlays in the pinned prefix get `shift = 0`; inline (line-range) overlays get `shift = sub_scroll`.
// `min_y` is the lowest screen Y the overlay is allowed to draw at — for prefix overlays
// it's the top of the inner content area; for inline overlays it's the top of the inline area
// (so an overlay scrolled up by sub_scroll gets clipped against the inline boundary, not the box).
fn render_annotation_overlays(
    frame: &mut Frame,
    overlays: &[(usize, &Annotation, u16)],
    content_x: u16,
    content_start_y: u16,
    content_width: u16,
    max_area: Rect,
    bg: Color,
    t: &crate::command::diff::theme::Theme,
    suppress_gutter: bool,
    selected_annotation_id: Option<&AnnotationId>,
    min_y: u16,
) {
    for (line_pos, annotation, shift) in overlays {
        let shift = *shift as i32;
        // Selected annotation gets the stronger focus color and bold
        // borders so it stands out from neighbours when several annotations
        // are stacked close together.
        let is_selected = selected_annotation_id.map_or(false, |id| &annotation.id == id);
        let ann_accent = if is_selected {
            t.ui.border_focused
        } else {
            t.ui.highlight
        };
        let extra_modifier = if is_selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };
        let content_width_inner = annotation_text_width(content_width);
        let inner_rows = if annotation.resolved {
            Vec::new()
        } else {
            annotation_inner_rows(annotation, content_width_inner)
        };
        let total_height: i32 = if annotation.resolved {
            1
        } else {
            inner_rows.len() as i32 + 2
        };
        // Shift the overlay up by `shift` rows. If the result puts the overlay's
        // top above `min_y`, top-clip: render only the tail starting from row
        // `top_skip` of the overlay's internal lines.
        let signed_screen_y = content_start_y as i32 + *line_pos as i32 - shift;
        // Entirely above viewport?
        if signed_screen_y + total_height <= min_y as i32 {
            continue;
        }
        let top_skip = (min_y as i32 - signed_screen_y).max(0) as usize;
        let screen_y = (signed_screen_y.max(min_y as i32)) as u16;

        // Check if annotation is visible
        if screen_y >= max_area.y + max_area.height {
            continue;
        }

        let available_height = (max_area.y + max_area.height).saturating_sub(screen_y) as usize;
        if available_height == 0 {
            continue;
        }

        // Resolved annotations render as a single compact row so they don't
        // crowd the diff. The full thread remains available in the modal.
        if annotation.resolved {
            // top_skip == 0 always here (total_height = 1), but guard anyway.
            if top_skip > 0 {
                continue;
            }
            let has_gutter =
                !suppress_gutter && matches!(annotation.target, AnnotationTarget::LineRange { .. });
            let overlay_area = Rect::new(content_x, screen_y, content_width, 1);
            frame.render_widget(ratatui::widgets::Clear, overlay_area);

            let muted = Style::default().fg(t.ui.text_muted);
            let check = Style::default().fg(t.ui.status_added);
            let accent = Style::default().fg(ann_accent).add_modifier(extra_modifier);

            let preview = annotation.root().content.replace('\n', " ");
            let preview = preview.split_whitespace().collect::<Vec<_>>().join(" ");
            let author = &annotation.root().author.login;
            let time = annotation.format_time();
            let body = if preview.is_empty() {
                format!("✓ resolved · @{} · {}", author, time)
            } else {
                format!("✓ resolved · @{} · \"{}\" · {}", author, preview, time)
            };
            let body = fit_to_char_width(&body, content_width_inner);

            let left: Span<'static> = if has_gutter {
                Span::styled("▍ ", accent)
            } else {
                Span::styled("  ", Style::default())
            };
            // Style the leading ✓ separately so it reads as a status badge.
            let (head, rest) = if let Some(idx) = body.find(' ') {
                (&body[..idx], &body[idx..])
            } else {
                (body.as_str(), "")
            };
            let line = Line::from(vec![
                left,
                Span::styled(head.to_string(), check),
                Span::styled(rest.to_string(), muted),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(bg)),
                overlay_area,
            );
            continue;
        }

        let num_lines = inner_rows.len() + 2; // +2 for top and bottom borders
                                              // top_skip rows have already been scrolled off the top of the overlay.
        let visible_after_skip = num_lines.saturating_sub(top_skip);

        let overlay_height = visible_after_skip.min(available_height) as u16;
        if overlay_height == 0 {
            continue;
        }
        let overlay_area = Rect::new(content_x, screen_y, content_width, overlay_height);

        // Clear the area first
        frame.render_widget(ratatui::widgets::Clear, overlay_area);

        // Build annotation lines
        let mut ann_lines: Vec<Line> = Vec::new();
        let note_style = Style::default().fg(t.ui.text_secondary);
        let separator_style = Style::default().fg(t.ui.text_muted);
        let border_style_ann = Style::default().fg(ann_accent).add_modifier(extra_modifier);
        let indicator_style = Style::default().fg(ann_accent).add_modifier(extra_modifier);
        let border_width = content_width.saturating_sub(3) as usize;

        // For line-range annotations, show a gutter indicator connecting to the lines above.
        // Suppressed for new-panel annotations in side-by-side mode, where the indicator
        // is rendered on the shared border via buffer_mut() instead.
        let has_gutter =
            !suppress_gutter && matches!(annotation.target, AnnotationTarget::LineRange { .. });

        // Add top border — gutter indicator continues into the box
        if has_gutter {
            ann_lines.push(Line::from(vec![
                Span::styled("▍", indicator_style),
                Span::styled(format!("┌{}┐", "─".repeat(border_width)), border_style_ann),
            ]));
        } else {
            ann_lines.push(Line::from(vec![Span::styled(
                format!(" ┌{}┐", "─".repeat(border_width)),
                border_style_ann,
            )]));
        }

        // Add inner rows (root content, separators, replies). Build them all;
        // top_skip / available_height clipping happens below so the bottom
        // border still renders when the top is scrolled off.
        for row in inner_rows.iter() {
            let (text_span, is_separator) = match row {
                OverlayRow::Text(line) => (
                    Span::styled(fit_to_char_width(line, content_width_inner), note_style),
                    false,
                ),
                OverlayRow::Separator(label) => {
                    // Render as: "── label ─────────"
                    let label_with_pad = format!("── {} ", label);
                    let label_chars = label_with_pad.chars().count();
                    let separator = if label_chars >= content_width_inner {
                        fit_to_char_width(&label_with_pad, content_width_inner)
                    } else {
                        let trailing = content_width_inner - label_chars;
                        format!("{}{}", label_with_pad, "─".repeat(trailing))
                    };
                    (Span::styled(separator, separator_style), true)
                }
            };

            let left_indicator: Span<'static> = if has_gutter {
                Span::styled("▍", indicator_style)
            } else {
                Span::styled(" ", Style::default())
            };
            // Use a thinner left/right bar for separators so they read as
            // dividers inside the thread rather than another comment line.
            let (left_bar, right_bar) = if is_separator {
                ("├─", "┤")
            } else {
                ("│ ", "│")
            };
            ann_lines.push(Line::from(vec![
                left_indicator,
                Span::styled(left_bar, border_style_ann),
                text_span,
                Span::styled(right_bar, border_style_ann),
            ]));
        }

        // Add bottom border with time
        let time_str = annotation.format_time();
        let time_with_padding = format!(" {} ", time_str);
        let time_len = time_with_padding.len();
        let dashes_before = border_width.saturating_sub(time_len + 1);
        if has_gutter {
            ann_lines.push(Line::from(vec![
                Span::styled("▍", indicator_style),
                Span::styled(format!("└{}", "─".repeat(dashes_before)), border_style_ann),
                Span::styled(time_with_padding, Style::default().fg(t.ui.text_muted)),
                Span::styled("─┘", border_style_ann),
            ]));
        } else {
            ann_lines.push(Line::from(vec![
                Span::styled(format!(" └{}", "─".repeat(dashes_before)), border_style_ann),
                Span::styled(time_with_padding, Style::default().fg(t.ui.text_muted)),
                Span::styled("─┘", border_style_ann),
            ]));
        }

        // Drop top_skip rows from the top (already scrolled off above the
        // viewport), then truncate to what fits in available_height.
        if top_skip > 0 {
            ann_lines.drain(..top_skip.min(ann_lines.len()));
        }
        ann_lines.truncate(available_height);

        let ann_para = Paragraph::new(ann_lines).style(Style::default().bg(bg));
        frame.render_widget(ann_para, overlay_area);
    }
}

#[allow(clippy::too_many_arguments)]
/// Returns `(content_row_offset, annotation_overlay_gaps, total_annotation_height, overlay_heights)`.
/// - `content_row_offset`: the number of non-diff rows at the top
///   (context lines + file annotation placeholders), used for mouse coordinate mapping.
/// - `annotation_overlay_gaps`: list of `(content_line_after, gap_height)` pairs describing
///   inline annotation overlay gaps within the content area.
/// - `total_annotation_height`: combined height of every annotation overlay attached
///   to the current file (file-level + inline), used to extend `max_scroll` so the bottom
///   of the diff stays reachable even when annotations push rows past the visible window.
/// - `overlay_heights`: per-line overlay heights, keyed by side_by_side index. Drives
///   sub-row snap scrolling through tall annotation overlays.
#[allow(clippy::too_many_arguments)]
pub fn render_diff(
    frame: &mut Frame,
    diff: &FileDiff,
    _file_diffs: &[FileDiff],
    sidebar_items: &[SidebarItem],
    sidebar_visible: &[usize],
    collapsed_dirs: &HashSet<String>,
    current_file: usize,
    scroll: u16,
    sub_scroll: u16,
    h_scroll: u16,
    watching: bool,
    show_sidebar: bool,
    focused_panel: FocusedPanel,
    sidebar_selected: usize,
    sidebar_scroll: usize,
    sidebar_h_scroll: u16,
    viewed_files: &HashSet<usize>,
    settings: &DiffViewSettings,
    hunk_count: usize,
    diff_fullscreen: DiffFullscreen,
    search_state: &SearchState,
    ref_input: &RefInputState,
    commit_ref: &str,
    focused_hunk: Option<usize>,
    hunks: &[usize],
    stacked_mode: bool,
    stacked_commit: Option<&StackedCommitInfo>,
    stacked_index: usize,
    stacked_total: usize,
    side_by_side: &[DiffLine],
    annotations: &[Annotation],
    selection: &Selection,
    old_highlighter: &FileHighlighter,
    new_highlighter: &FileHighlighter,
    viewed_hunks: &HashSet<usize>,
    total_added: usize,
    total_removed: usize,
    annotation_source_label: Option<&str>,
    annotation_source_error: Option<&str>,
    selected_annotation_id: Option<&AnnotationId>,
    loading_message: Option<&str>,
) -> (usize, Vec<(usize, usize)>, usize, HashMap<usize, usize>) {
    let area = frame.area();
    let t = theme::get();
    let bg = t.ui.bg;

    // Layout: header (if stacked) + main content + footer
    let (content_area, footer_area) = if stacked_mode {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Header
                Constraint::Min(0),    // Main content
                Constraint::Length(1), // Footer
            ])
            .split(area);

        // Render stacked header
        render_stacked_header(
            frame,
            chunks[0],
            stacked_commit,
            stacked_index,
            stacked_total,
        );

        (chunks[1], chunks[2])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        (chunks[0], chunks[1])
    };

    let main_area = if show_sidebar {
        let sidebar_width = (area.width / 4).clamp(20, 35);
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(0)])
            .split(content_area);

        render_sidebar(
            frame,
            main_chunks[0],
            sidebar_items,
            sidebar_visible,
            collapsed_dirs,
            current_file,
            sidebar_selected,
            sidebar_scroll,
            sidebar_h_scroll,
            viewed_files,
            focused_panel == FocusedPanel::Sidebar,
            _file_diffs.len(),
            total_added,
            total_removed,
        );

        main_chunks[1]
    } else {
        content_area
    };

    // Handle binary files - show a message instead of trying to diff
    if diff.is_binary {
        let border_style = Style::default().fg(t.ui.border_unfocused);
        let title_style = if focused_panel == FocusedPanel::DiffView {
            Style::default().fg(t.ui.border_focused)
        } else {
            Style::default().fg(t.ui.border_unfocused)
        };

        let message = Line::from(vec![Span::styled(
            "Binary file - not displayed",
            Style::default().fg(t.ui.text_muted),
        )]);
        let para = Paragraph::new(vec![message])
            .alignment(ratatui::layout::Alignment::Center)
            .block(
                Block::default()
                    .title(Line::styled(format!(" {} ", diff.filename), title_style))
                    .borders(Borders::ALL)
                    .border_style(border_style),
            );
        frame.render_widget(para, main_area);

        render_footer(
            frame,
            footer_area,
            FooterData {
                filename: &diff.filename,
                commit_ref,
                watching,
                current_file,
                viewed_files,
                line_stats_added: 0,
                line_stats_removed: 0,
                hunk_count: 0,
                focused_hunk: None,
                search_state,
                ref_input,
                area_width: area.width,
                annotation_source_label,
                annotation_source_error,
                annotation_select_active: selected_annotation_id.is_some(),
                loading_message,
            },
        );
        return (0, Vec::new(), 0, HashMap::new());
    }

    // side_by_side is now passed as a parameter (pre-computed and cached)
    let line_stats = compute_line_stats(side_by_side);

    let is_new_file = diff.old_content.is_empty() && !diff.new_content.is_empty();
    let is_deleted_file = !diff.old_content.is_empty() && diff.new_content.is_empty();

    // Track how many non-diff rows are at the top (context lines + file annotations)
    let content_row_offset: usize;
    // Track inline annotation overlay gaps for mouse coordinate mapping
    let mut overlay_gaps: Vec<(usize, usize)> = Vec::new();
    // Total rows consumed by every annotation overlay on this file (file + inline).
    // Returned so the input loop can extend `max_scroll` past the naive line count.
    let total_annotation_height: usize;
    // Per-line overlay heights, returned so the scroll handlers can drive
    // sub-row snap scrolling through tall overlays.
    let overlay_heights: HashMap<usize, usize>;

    let border_style = Style::default().fg(t.ui.border_unfocused);
    let title_style = if focused_panel == FocusedPanel::DiffView {
        Style::default().fg(t.ui.border_focused)
    } else {
        Style::default().fg(t.ui.border_unfocused)
    };

    if is_new_file {
        let visible_height = main_area.height.saturating_sub(2) as usize;
        let new_context = compute_context_lines(
            &diff.new_content,
            &diff.filename,
            scroll as usize,
            &settings.context,
            settings.tab_width,
        );
        let context_count = new_context.len();
        let scroll_usize = scroll as usize;

        // Get file-level annotations for this file
        let file_annotations: Vec<&Annotation> = annotations
            .iter()
            .filter(|a| a.filename == diff.filename && matches!(a.target, AnnotationTarget::File))
            .collect();

        // Collect line-range annotations for this file (New panel only)
        let line_annotations: Vec<&Annotation> = annotations
            .iter()
            .filter(|a| {
                a.filename == diff.filename
                    && matches!(a.target, AnnotationTarget::LineRange { .. })
            })
            .collect();

        let overlay_width = main_area.width.saturating_sub(2);
        let annotation_height = file_annotation_height(&file_annotations, overlay_width);
        let line_annotation_total: usize = line_annotations
            .iter()
            .map(|a| annotation_overlay_height(a, overlay_width))
            .sum();
        total_annotation_height = annotation_height + line_annotation_total;
        content_row_offset = context_count + annotation_height;
        overlay_heights = compute_overlay_heights(
            &line_annotations,
            side_by_side,
            None,
            Some(main_area),
            main_area,
        );

        let base_content_height = visible_height.saturating_sub(context_count);
        let content_height = base_content_height.saturating_sub(annotation_height);

        // Take sub_scroll extra diff lines so Paragraph.scroll can slide them in.
        let visible_lines: Vec<&DiffLine> = side_by_side
            .iter()
            .skip(scroll_usize)
            .take(content_height + sub_scroll as usize)
            .collect();

        let mut new_lines: Vec<Line> = Vec::new();
        let mut annotation_overlays: Vec<(usize, &Annotation)> = Vec::new();

        if settings.context.enabled && context_count > 0 {
            render_context_lines(
                &new_context,
                context_count,
                &mut new_lines,
                &diff.filename,
                new_highlighter,
                settings.tab_width,
            );
        }

        // Show file-level annotations at top
        for annotation in &file_annotations {
            let num_lines = annotation_overlay_height(annotation, overlay_width);
            let annotation_start = new_lines.len();
            for _ in 0..num_lines {
                new_lines.push(Line::from(vec![Span::raw("")]));
            }
            annotation_overlays.push((annotation_start, annotation));
        }

        let ann_index_ranges = compute_ann_index_ranges(&line_annotations, side_by_side);
        let selected_range =
            selected_ann_range(selected_annotation_id, &line_annotations, side_by_side);
        let annotation_indicator_style = Style::default().fg(t.ui.highlight);
        let focus_style = Style::default().fg(t.ui.border_focused);

        let row_target_width = (main_area.width as usize).saturating_sub(2) + h_scroll as usize;

        for (i, diff_line) in visible_lines.iter().enumerate() {
            let line_idx = scroll_usize + i;
            let new_selection_range =
                get_selection_range_for_line(line_idx, DiffPanelFocus::New, selection);
            let in_annotation = is_in_ann_range(line_idx, None, &ann_index_ranges);
            let in_selected_annotation = is_in_selected_ann_range(line_idx, None, selected_range);
            let new_line_selected =
                new_selection_range.map_or(false, |(s, e)| s == 0 && e == usize::MAX);
            let hunk_viewed = {
                let h_idx = hunks.iter().rposition(|&h| h <= line_idx);
                !matches!(diff_line.change_type, ChangeType::Equal)
                    && h_idx.map_or(false, |idx| viewed_hunks.contains(&idx))
            };

            if let Some((num, text)) = &diff_line.new_line {
                let mut spans: Vec<Span> = Vec::new();
                spans.push(make_indicator_span(
                    false,
                    in_annotation,
                    in_selected_annotation,
                    new_line_selected,
                    bg,
                    focus_style,
                    annotation_indicator_style,
                ));

                let prefix = format!("{:4} ", num);
                let gutter_bg = t.diff.added_gutter_bg;
                let gutter_bg = if new_line_selected {
                    blend_with_selection(gutter_bg)
                } else {
                    gutter_bg
                };
                spans.push(Span::styled(
                    prefix,
                    Style::default().fg(t.diff.added_gutter_fg).bg(gutter_bg),
                ));
                let matches = search_state.get_matches_for_line(line_idx, MatchPanel::New);
                let line_bg = if hunk_viewed { bg } else { t.diff.added_bg };
                let content_spans = apply_search_highlight(
                    text,
                    &diff.filename,
                    Some(line_bg),
                    &matches,
                    Some(new_highlighter),
                    Some(*num),
                    settings.tab_width,
                );
                let content_spans =
                    apply_selection_to_spans(content_spans, new_selection_range, line_bg);
                spans.extend(content_spans);
                let spans = pad_line_bg(spans, row_target_width, line_bg);
                new_lines.push(Line::from(spans));
            }

            // Check if this line is the end_line for any line-range annotation
            for annotation in &line_annotations {
                if let AnnotationTarget::LineRange {
                    panel, end_line, ..
                } = &annotation.target
                {
                    if diff_line.line_number(*panel) == Some(*end_line) {
                        let num_ann_lines = annotation_overlay_height(annotation, overlay_width);
                        let line_pos = new_lines.len();
                        overlay_gaps.push((i, num_ann_lines));
                        for _ in 0..num_ann_lines {
                            new_lines.push(Line::from(vec![Span::raw("")]));
                        }
                        annotation_overlays.push((line_pos, annotation));
                    }
                }
            }
        }

        // Split the paragraph into a pinned prefix (context + file annotations)
        // and a scrollable inline portion (diff lines + line-range overlays),
        // so sub_scroll moves only the inline portion. Borders are drawn once
        // around the whole area.
        let new_block = Block::default()
            .title(Line::styled(" [2] New File ", title_style))
            .borders(Borders::ALL)
            .border_style(border_style);
        frame.render_widget(new_block, main_area);
        let inner_x = main_area.x + 1;
        let inner_y = main_area.y + 1;
        let inner_width = main_area.width.saturating_sub(2);
        let inner_height = main_area.height.saturating_sub(2);
        let prefix_rows = (context_count + annotation_height) as u16;
        let prefix_height = prefix_rows.min(inner_height);
        let inline_y = inner_y + prefix_height;
        let inline_height = inner_height.saturating_sub(prefix_height);

        let prefix_lines: Vec<Line> = new_lines
            .iter()
            .take(prefix_rows as usize)
            .cloned()
            .collect();
        let inline_lines: Vec<Line> = new_lines
            .iter()
            .skip(prefix_rows as usize)
            .cloned()
            .collect();

        if prefix_height > 0 {
            let prefix_area = Rect::new(inner_x, inner_y, inner_width, prefix_height);
            let prefix_para = Paragraph::new(prefix_lines)
                .style(Style::default().bg(bg))
                .scroll((0, h_scroll));
            frame.render_widget(prefix_para, prefix_area);
        }
        if inline_height > 0 {
            let inline_area = Rect::new(inner_x, inline_y, inner_width, inline_height);
            let inline_para = Paragraph::new(inline_lines)
                .style(Style::default().bg(bg))
                .scroll((sub_scroll, h_scroll));
            frame.render_widget(inline_para, inline_area);
        }

        // Split overlays into prefix (file-level, unshifted, clamped at inner_y) and
        // inline (line-range, shifted by sub_scroll, clamped at inline_y).
        let prefix_rows_usize = prefix_rows as usize;
        let prefix_overlays: Vec<(usize, &Annotation, u16)> = annotation_overlays
            .iter()
            .filter(|&&(pos, _)| pos < prefix_rows_usize)
            .map(|&(pos, ann)| (pos, ann, 0u16))
            .collect();
        let inline_overlays: Vec<(usize, &Annotation, u16)> = annotation_overlays
            .iter()
            .filter(|&&(pos, _)| pos >= prefix_rows_usize)
            .map(|&(pos, ann)| (pos, ann, sub_scroll))
            .collect();
        let content_x = main_area.x + 1;
        let content_start_y = main_area.y + 1;
        let content_width = main_area.width.saturating_sub(2);
        render_annotation_overlays(
            frame,
            &prefix_overlays,
            content_x,
            content_start_y,
            content_width,
            main_area,
            bg,
            t,
            false,
            selected_annotation_id,
            inner_y,
        );
        render_annotation_overlays(
            frame,
            &inline_overlays,
            content_x,
            content_start_y,
            content_width,
            main_area,
            bg,
            t,
            false,
            selected_annotation_id,
            inline_y,
        );
    } else if is_deleted_file {
        let visible_height = main_area.height.saturating_sub(2) as usize;
        let old_context = compute_context_lines(
            &diff.old_content,
            &diff.filename,
            scroll as usize,
            &settings.context,
            settings.tab_width,
        );
        let context_count = old_context.len();
        let scroll_usize = scroll as usize;

        // Get file-level annotations for this file
        let file_annotations: Vec<&Annotation> = annotations
            .iter()
            .filter(|a| a.filename == diff.filename && matches!(a.target, AnnotationTarget::File))
            .collect();

        // Collect line-range annotations for this file (Old panel only)
        let line_annotations: Vec<&Annotation> = annotations
            .iter()
            .filter(|a| {
                a.filename == diff.filename
                    && matches!(a.target, AnnotationTarget::LineRange { .. })
            })
            .collect();

        let overlay_width = main_area.width.saturating_sub(2);
        let annotation_height = file_annotation_height(&file_annotations, overlay_width);
        let line_annotation_total: usize = line_annotations
            .iter()
            .map(|a| annotation_overlay_height(a, overlay_width))
            .sum();
        total_annotation_height = annotation_height + line_annotation_total;
        content_row_offset = context_count + annotation_height;
        overlay_heights = compute_overlay_heights(
            &line_annotations,
            side_by_side,
            Some(main_area),
            None,
            main_area,
        );

        let base_content_height = visible_height.saturating_sub(context_count);
        let content_height = base_content_height.saturating_sub(annotation_height);

        let visible_lines: Vec<&DiffLine> = side_by_side
            .iter()
            .skip(scroll_usize)
            .take(content_height + sub_scroll as usize)
            .collect();

        let mut old_lines: Vec<Line> = Vec::new();
        let mut annotation_overlays: Vec<(usize, &Annotation)> = Vec::new();

        if settings.context.enabled && context_count > 0 {
            render_context_lines(
                &old_context,
                context_count,
                &mut old_lines,
                &diff.filename,
                old_highlighter,
                settings.tab_width,
            );
        }

        // Show file-level annotations at top
        for annotation in &file_annotations {
            let num_lines = annotation_overlay_height(annotation, overlay_width);
            let annotation_start = old_lines.len();
            for _ in 0..num_lines {
                old_lines.push(Line::from(vec![Span::raw("")]));
            }
            annotation_overlays.push((annotation_start, annotation));
        }

        let ann_index_ranges = compute_ann_index_ranges(&line_annotations, side_by_side);
        let selected_range =
            selected_ann_range(selected_annotation_id, &line_annotations, side_by_side);
        let annotation_indicator_style = Style::default().fg(t.ui.highlight);
        let focus_style = Style::default().fg(t.ui.border_focused);

        let row_target_width = (main_area.width as usize).saturating_sub(2) + h_scroll as usize;

        for (i, diff_line) in visible_lines.iter().enumerate() {
            let line_idx = scroll_usize + i;
            let old_selection_range =
                get_selection_range_for_line(line_idx, DiffPanelFocus::Old, selection);
            let in_annotation = is_in_ann_range(line_idx, None, &ann_index_ranges);
            let in_selected_annotation = is_in_selected_ann_range(line_idx, None, selected_range);
            let old_line_selected =
                old_selection_range.map_or(false, |(s, e)| s == 0 && e == usize::MAX);
            let hunk_viewed = {
                let h_idx = hunks.iter().rposition(|&h| h <= line_idx);
                !matches!(diff_line.change_type, ChangeType::Equal)
                    && h_idx.map_or(false, |idx| viewed_hunks.contains(&idx))
            };

            if let Some((num, text)) = &diff_line.old_line {
                let mut spans: Vec<Span> = Vec::new();
                spans.push(make_indicator_span(
                    false,
                    in_annotation,
                    in_selected_annotation,
                    old_line_selected,
                    bg,
                    focus_style,
                    annotation_indicator_style,
                ));

                let prefix = format!("{:4} ", num);
                let gutter_bg = t.diff.deleted_gutter_bg;
                let gutter_bg = if old_line_selected {
                    blend_with_selection(gutter_bg)
                } else {
                    gutter_bg
                };
                spans.push(Span::styled(
                    prefix,
                    Style::default().fg(t.diff.deleted_gutter_fg).bg(gutter_bg),
                ));
                let matches = search_state.get_matches_for_line(line_idx, MatchPanel::Old);
                let line_bg = if hunk_viewed { bg } else { t.diff.deleted_bg };
                let content_spans = apply_search_highlight(
                    text,
                    &diff.filename,
                    Some(line_bg),
                    &matches,
                    Some(old_highlighter),
                    Some(*num),
                    settings.tab_width,
                );
                let content_spans =
                    apply_selection_to_spans(content_spans, old_selection_range, line_bg);
                spans.extend(content_spans);
                let spans = pad_line_bg(spans, row_target_width, line_bg);
                old_lines.push(Line::from(spans));
            }

            // Check if this line is the end_line for any line-range annotation
            for annotation in &line_annotations {
                if let AnnotationTarget::LineRange {
                    panel, end_line, ..
                } = &annotation.target
                {
                    if diff_line.line_number(*panel) == Some(*end_line) {
                        let num_ann_lines = annotation_overlay_height(annotation, overlay_width);
                        let line_pos = old_lines.len();
                        overlay_gaps.push((i, num_ann_lines));
                        for _ in 0..num_ann_lines {
                            old_lines.push(Line::from(vec![Span::raw("")]));
                        }
                        annotation_overlays.push((line_pos, annotation));
                    }
                }
            }
        }

        // Split into pinned prefix + scrollable inline area (see new-file branch).
        let old_block = Block::default()
            .title(Line::styled(" [2] Deleted File ", title_style))
            .borders(Borders::ALL)
            .border_style(border_style);
        frame.render_widget(old_block, main_area);
        let inner_x = main_area.x + 1;
        let inner_y = main_area.y + 1;
        let inner_width = main_area.width.saturating_sub(2);
        let inner_height = main_area.height.saturating_sub(2);
        let prefix_rows = (context_count + annotation_height) as u16;
        let prefix_height = prefix_rows.min(inner_height);
        let inline_y = inner_y + prefix_height;
        let inline_height = inner_height.saturating_sub(prefix_height);

        let prefix_lines: Vec<Line> = old_lines
            .iter()
            .take(prefix_rows as usize)
            .cloned()
            .collect();
        let inline_lines: Vec<Line> = old_lines
            .iter()
            .skip(prefix_rows as usize)
            .cloned()
            .collect();

        if prefix_height > 0 {
            let prefix_area = Rect::new(inner_x, inner_y, inner_width, prefix_height);
            let prefix_para = Paragraph::new(prefix_lines)
                .style(Style::default().bg(bg))
                .scroll((0, h_scroll));
            frame.render_widget(prefix_para, prefix_area);
        }
        if inline_height > 0 {
            let inline_area = Rect::new(inner_x, inline_y, inner_width, inline_height);
            let inline_para = Paragraph::new(inline_lines)
                .style(Style::default().bg(bg))
                .scroll((sub_scroll, h_scroll));
            frame.render_widget(inline_para, inline_area);
        }

        let prefix_rows_usize = prefix_rows as usize;
        let prefix_overlays: Vec<(usize, &Annotation, u16)> = annotation_overlays
            .iter()
            .filter(|&&(pos, _)| pos < prefix_rows_usize)
            .map(|&(pos, ann)| (pos, ann, 0u16))
            .collect();
        let inline_overlays: Vec<(usize, &Annotation, u16)> = annotation_overlays
            .iter()
            .filter(|&&(pos, _)| pos >= prefix_rows_usize)
            .map(|&(pos, ann)| (pos, ann, sub_scroll))
            .collect();
        let content_x = main_area.x + 1;
        let content_start_y = main_area.y + 1;
        let content_width = main_area.width.saturating_sub(2);
        render_annotation_overlays(
            frame,
            &prefix_overlays,
            content_x,
            content_start_y,
            content_width,
            main_area,
            bg,
            t,
            false,
            selected_annotation_id,
            inner_y,
        );
        render_annotation_overlays(
            frame,
            &inline_overlays,
            content_x,
            content_start_y,
            content_width,
            main_area,
            bg,
            t,
            false,
            selected_annotation_id,
            inline_y,
        );
    } else {
        let (old_area, new_area) = match diff_fullscreen {
            DiffFullscreen::OldOnly => (Some(main_area), None),
            DiffFullscreen::NewOnly => (None, Some(main_area)),
            DiffFullscreen::None => {
                let content_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(main_area);
                (Some(content_chunks[0]), Some(content_chunks[1]))
            }
        };

        let old_context = compute_context_lines(
            &diff.old_content,
            &diff.filename,
            scroll as usize,
            &settings.context,
            settings.tab_width,
        );
        let new_context = compute_context_lines(
            &diff.new_content,
            &diff.filename,
            scroll as usize,
            &settings.context,
            settings.tab_width,
        );
        let context_count = old_context.len().max(new_context.len());

        let reference_area = old_area.or(new_area).unwrap_or(main_area);
        let visible_height = reference_area.height.saturating_sub(2) as usize;
        let scroll_usize = scroll as usize;

        // Collect file-level annotations
        let file_annotations: Vec<&Annotation> = annotations
            .iter()
            .filter(|a| a.filename == diff.filename && matches!(a.target, AnnotationTarget::File))
            .collect();

        // Collect line-range annotations for this file
        let line_annotations: Vec<&Annotation> = annotations
            .iter()
            .filter(|a| {
                a.filename == diff.filename
                    && matches!(a.target, AnnotationTarget::LineRange { .. })
            })
            .collect();

        let file_overlay_width = if old_area.is_some() && new_area.is_some() {
            old_area
                .unwrap()
                .width
                .saturating_add(new_area.unwrap().width)
                .saturating_sub(2)
        } else {
            reference_area.width.saturating_sub(2)
        };
        let file_ann_height = file_annotation_height(&file_annotations, file_overlay_width);
        let line_annotation_total: usize = line_annotations
            .iter()
            .map(|a| {
                let w = overlay_content_width(a, old_area, new_area, main_area);
                annotation_overlay_height(a, w)
            })
            .sum();
        total_annotation_height = file_ann_height + line_annotation_total;
        content_row_offset = context_count + file_ann_height;
        overlay_heights = compute_overlay_heights(
            &line_annotations,
            side_by_side,
            old_area,
            new_area,
            main_area,
        );

        let base_content_height = visible_height.saturating_sub(context_count);
        let content_height = base_content_height.saturating_sub(file_ann_height);
        let visible_lines: Vec<&DiffLine> = side_by_side
            .iter()
            .skip(scroll_usize)
            .take(content_height + sub_scroll as usize)
            .collect();

        let mut old_lines: Vec<Line> = Vec::new();
        let mut new_lines: Vec<Line> = Vec::new();
        let mut annotation_overlays: Vec<(usize, &Annotation)> = Vec::new();

        if settings.context.enabled && context_count > 0 {
            if old_area.is_some() {
                render_context_lines(
                    &old_context,
                    context_count,
                    &mut old_lines,
                    &diff.filename,
                    old_highlighter,
                    settings.tab_width,
                );
            }
            if new_area.is_some() {
                render_context_lines(
                    &new_context,
                    context_count,
                    &mut new_lines,
                    &diff.filename,
                    new_highlighter,
                    settings.tab_width,
                );
            }
        }

        let hunk_index_for_line =
            |line_idx: usize| -> Option<usize> { hunks.iter().rposition(|&h| h <= line_idx) };
        let is_line_in_viewed_hunk = |line_idx: usize, change_type: ChangeType| -> bool {
            if matches!(change_type, ChangeType::Equal) {
                return false;
            }
            match hunk_index_for_line(line_idx) {
                Some(idx) => viewed_hunks.contains(&idx),
                None => false,
            }
        };

        let is_in_focused_hunk = |line_idx: usize, change_type: ChangeType| -> bool {
            if matches!(change_type, ChangeType::Equal) {
                return false;
            }
            if let Some(hunk_idx) = focused_hunk {
                if let Some(&hunk_start) = hunks.get(hunk_idx) {
                    let hunk_end = hunks.get(hunk_idx + 1).copied().unwrap_or(usize::MAX);
                    return line_idx >= hunk_start && line_idx < hunk_end;
                }
            }
            false
        };

        // Add file-level annotations at the top (after context lines)
        for annotation in &file_annotations {
            let num_lines = annotation_overlay_height(annotation, file_overlay_width);
            let annotation_start = old_lines.len(); // same position in both panels
            for _ in 0..num_lines {
                if old_area.is_some() {
                    old_lines.push(Line::from(vec![Span::raw("")]));
                }
                if new_area.is_some() {
                    new_lines.push(Line::from(vec![Span::raw("")]));
                }
            }
            annotation_overlays.push((annotation_start, annotation));
        }

        let ann_index_ranges = compute_ann_index_ranges(&line_annotations, side_by_side);
        let selected_range =
            selected_ann_range(selected_annotation_id, &line_annotations, side_by_side);

        // Track rendered rows for new-panel border annotation markers (paragraph-relative)
        let mut border_marker_rows: Vec<usize> = Vec::new();
        let mut rendered_row = content_row_offset;

        let focus_style = Style::default().fg(t.ui.border_focused);
        let annotation_indicator_style = Style::default().fg(t.ui.highlight);

        // Old panel always has full borders (Borders::ALL); the new panel drops its left
        // border when shown alongside the old panel (sharing it with the old panel's right).
        let old_row_target_width = old_area
            .map(|a| (a.width as usize).saturating_sub(2))
            .unwrap_or(0)
            + h_scroll as usize;
        let new_row_target_width = new_area
            .map(|a| {
                let borders = if old_area.is_some() { 1 } else { 2 };
                (a.width as usize).saturating_sub(borders)
            })
            .unwrap_or(0)
            + h_scroll as usize;

        for (i, diff_line) in visible_lines.iter().enumerate() {
            let line_idx = scroll_usize + i;
            let in_focused = is_in_focused_hunk(line_idx, diff_line.change_type);
            let hunk_viewed = is_line_in_viewed_hunk(line_idx, diff_line.change_type);
            let mut style = DiffLineStyle::for_change_type(diff_line.change_type, bg, t);
            if hunk_viewed {
                // Drop colored bg from the code area; keep the gutter colored.
                if style.old_bg.is_some() {
                    style.old_bg = Some(bg);
                }
                if style.new_bg.is_some() {
                    style.new_bg = Some(bg);
                }
            }

            let old_selection_range =
                get_selection_range_for_line(line_idx, DiffPanelFocus::Old, selection);
            let new_selection_range =
                get_selection_range_for_line(line_idx, DiffPanelFocus::New, selection);

            let old_in_annotation =
                is_in_ann_range(line_idx, Some(DiffPanelFocus::Old), &ann_index_ranges);
            let new_in_annotation =
                is_in_ann_range(line_idx, Some(DiffPanelFocus::New), &ann_index_ranges);
            let old_in_selected_annotation =
                is_in_selected_ann_range(line_idx, Some(DiffPanelFocus::Old), selected_range);
            let new_in_selected_annotation =
                is_in_selected_ann_range(line_idx, Some(DiffPanelFocus::New), selected_range);

            if new_in_annotation {
                border_marker_rows.push(rendered_row);
            }
            rendered_row += 1;

            let old_line_selected =
                old_selection_range.map_or(false, |(s, e)| s == 0 && e == usize::MAX);
            let new_line_selected =
                new_selection_range.map_or(false, |(s, e)| s == 0 && e == usize::MAX);

            if old_area.is_some() {
                let mut old_spans: Vec<Span> = Vec::new();
                old_spans.push(make_indicator_span(
                    in_focused,
                    old_in_annotation,
                    old_in_selected_annotation,
                    old_line_selected,
                    bg,
                    focus_style,
                    annotation_indicator_style,
                ));
                match &diff_line.old_line {
                    Some((num, _text)) => {
                        let prefix = format!("{:4} ", num);
                        let gutter_bg = style.old_gutter_bg.unwrap_or(Color::Reset);
                        let gutter_bg = if old_line_selected {
                            blend_with_selection(if gutter_bg == Color::Reset {
                                bg
                            } else {
                                gutter_bg
                            })
                        } else {
                            gutter_bg
                        };
                        old_spans.push(Span::styled(
                            prefix,
                            Style::default()
                                .fg(style.old_gutter_fg.unwrap_or(t.ui.line_number))
                                .bg(gutter_bg),
                        ));
                        let matches = search_state.get_matches_for_line(line_idx, MatchPanel::Old);

                        // Use word-level rendering for modified lines if segments are available
                        let content_spans = if matches!(diff_line.change_type, ChangeType::Modified)
                            && !hunk_viewed
                        {
                            if let Some(ref segments) = diff_line.old_segments {
                                let emphasis_ranges = segments_to_emphasis_ranges(segments);
                                apply_word_emphasis_highlight(
                                    _text,
                                    &diff.filename,
                                    style.old_bg,
                                    t.diff.deleted_word_bg,
                                    &emphasis_ranges,
                                    &matches,
                                    Some(old_highlighter),
                                    Some(*num),
                                    settings.tab_width,
                                )
                            } else {
                                apply_search_highlight(
                                    _text,
                                    &diff.filename,
                                    style.old_bg,
                                    &matches,
                                    Some(old_highlighter),
                                    Some(*num),
                                    settings.tab_width,
                                )
                            }
                        } else {
                            apply_search_highlight(
                                _text,
                                &diff.filename,
                                style.old_bg,
                                &matches,
                                Some(old_highlighter),
                                Some(*num),
                                settings.tab_width,
                            )
                        };
                        // Apply selection highlighting
                        let content_spans = apply_selection_to_spans(
                            content_spans,
                            old_selection_range,
                            style.old_bg.unwrap_or(bg),
                        );
                        old_spans.extend(content_spans);
                    }
                    None => {
                        // Fill the whole row (including the line-number gutter) with stripes,
                        // extending past the visible viewport so horizontal scroll stays striped.
                        let current_len: usize =
                            old_spans.iter().map(|s| s.content.chars().count()).sum();
                        let stripe_width = old_row_target_width.saturating_sub(current_len);
                        old_spans.push(Span::styled(
                            generate_stripe_pattern(stripe_width),
                            Style::default().fg(t.diff.empty_placeholder_fg),
                        ));
                    }
                }
                let old_spans =
                    pad_line_bg(old_spans, old_row_target_width, style.old_bg.unwrap_or(bg));
                old_lines.push(Line::from(old_spans));
            }

            if new_area.is_some() {
                let mut new_spans: Vec<Span> = Vec::new();
                if old_area.is_none() {
                    new_spans.push(make_indicator_span(
                        in_focused,
                        new_in_annotation,
                        new_in_selected_annotation,
                        new_line_selected,
                        bg,
                        focus_style,
                        annotation_indicator_style,
                    ));
                }
                match &diff_line.new_line {
                    Some((num, _text)) => {
                        let prefix = format!("{:4} ", num);
                        let gutter_bg = style.new_gutter_bg.unwrap_or(Color::Reset);
                        let gutter_bg = if new_line_selected {
                            blend_with_selection(if gutter_bg == Color::Reset {
                                bg
                            } else {
                                gutter_bg
                            })
                        } else {
                            gutter_bg
                        };
                        new_spans.push(Span::styled(
                            prefix,
                            Style::default()
                                .fg(style.new_gutter_fg.unwrap_or(t.ui.line_number))
                                .bg(gutter_bg),
                        ));
                        let matches = search_state.get_matches_for_line(line_idx, MatchPanel::New);

                        // Use word-level rendering for modified lines if segments are available
                        let content_spans = if matches!(diff_line.change_type, ChangeType::Modified)
                            && !hunk_viewed
                        {
                            if let Some(ref segments) = diff_line.new_segments {
                                let emphasis_ranges = segments_to_emphasis_ranges(segments);
                                apply_word_emphasis_highlight(
                                    _text,
                                    &diff.filename,
                                    style.new_bg,
                                    t.diff.added_word_bg,
                                    &emphasis_ranges,
                                    &matches,
                                    Some(new_highlighter),
                                    Some(*num),
                                    settings.tab_width,
                                )
                            } else {
                                apply_search_highlight(
                                    _text,
                                    &diff.filename,
                                    style.new_bg,
                                    &matches,
                                    Some(new_highlighter),
                                    Some(*num),
                                    settings.tab_width,
                                )
                            }
                        } else {
                            apply_search_highlight(
                                _text,
                                &diff.filename,
                                style.new_bg,
                                &matches,
                                Some(new_highlighter),
                                Some(*num),
                                settings.tab_width,
                            )
                        };
                        // Apply selection highlighting
                        let content_spans = apply_selection_to_spans(
                            content_spans,
                            new_selection_range,
                            style.new_bg.unwrap_or(bg),
                        );
                        new_spans.extend(content_spans);
                    }
                    None => {
                        // Fill the whole row (including the line-number gutter) with stripes,
                        // extending past the visible viewport so horizontal scroll stays striped.
                        let current_len: usize =
                            new_spans.iter().map(|s| s.content.chars().count()).sum();
                        let stripe_width = new_row_target_width.saturating_sub(current_len);
                        new_spans.push(Span::styled(
                            generate_stripe_pattern(stripe_width),
                            Style::default().fg(t.diff.empty_placeholder_fg),
                        ));
                    }
                }
                let new_spans =
                    pad_line_bg(new_spans, new_row_target_width, style.new_bg.unwrap_or(bg));
                new_lines.push(Line::from(new_spans));
            }

            // Check if this line is the end_line for any line-range annotation
            for annotation in &line_annotations {
                if let AnnotationTarget::LineRange {
                    panel, end_line, ..
                } = &annotation.target
                {
                    if diff_line.line_number(*panel) == Some(*end_line) {
                        let annotation_width =
                            overlay_content_width(annotation, old_area, new_area, main_area);
                        let num_lines = annotation_overlay_height(annotation, annotation_width);

                        let line_pos = if old_area.is_some() {
                            old_lines.len()
                        } else {
                            new_lines.len()
                        };

                        // Record gap for mouse coordinate mapping:
                        // `i` is the visible content line index after which this overlay appears
                        overlay_gaps.push((i, num_lines));

                        // Add placeholder lines to both panels to keep them in sync
                        for _ in 0..num_lines {
                            if old_area.is_some() {
                                old_lines.push(Line::from(vec![Span::raw("")]));
                            }
                            if new_area.is_some() {
                                new_lines.push(Line::from(vec![Span::raw("")]));
                            }
                            rendered_row += 1;
                        }

                        annotation_overlays.push((line_pos, annotation));
                    }
                }
            }
        }

        let prefix_rows = (context_count + file_ann_height) as u16;
        let prefix_rows_usize = prefix_rows as usize;

        // Render each panel as a borderless prefix + scrollable inline paragraph,
        // with the surrounding Block drawn separately so the borders stay still.
        if let Some(area) = old_area {
            frame.render_widget(
                Block::default()
                    .title(Line::styled(" [2] Old ", title_style))
                    .borders(Borders::ALL)
                    .border_style(border_style),
                area,
            );
            let inner_x = area.x + 1;
            let inner_y = area.y + 1;
            let inner_width = area.width.saturating_sub(2);
            let inner_height = area.height.saturating_sub(2);
            let prefix_h = prefix_rows.min(inner_height);
            let inline_h = inner_height.saturating_sub(prefix_h);

            let prefix_lines: Vec<Line> =
                old_lines.iter().take(prefix_rows_usize).cloned().collect();
            let inline_lines: Vec<Line> =
                old_lines.iter().skip(prefix_rows_usize).cloned().collect();
            if prefix_h > 0 {
                frame.render_widget(
                    Paragraph::new(prefix_lines)
                        .style(Style::default().bg(bg))
                        .scroll((0, h_scroll)),
                    Rect::new(inner_x, inner_y, inner_width, prefix_h),
                );
            }
            if inline_h > 0 {
                frame.render_widget(
                    Paragraph::new(inline_lines)
                        .style(Style::default().bg(bg))
                        .scroll((sub_scroll, h_scroll)),
                    Rect::new(inner_x, inner_y + prefix_h, inner_width, inline_h),
                );
            }
        }

        if let Some(area) = new_area {
            // When both panels are shown, new panel has no left border to share with old panel
            let new_borders = if old_area.is_some() {
                Borders::TOP | Borders::RIGHT | Borders::BOTTOM
            } else {
                Borders::ALL
            };
            frame.render_widget(
                Block::default()
                    .title(Line::styled(" New ", title_style))
                    .borders(new_borders)
                    .style(Style::default().bg(bg))
                    .border_style(border_style),
                area,
            );
            // Inner content area: new panel may not have a left border (shared with old).
            let inner_x = if old_area.is_some() {
                area.x
            } else {
                area.x + 1
            };
            let inner_y = area.y + 1;
            let inner_width = if old_area.is_some() {
                area.width.saturating_sub(1)
            } else {
                area.width.saturating_sub(2)
            };
            let inner_height = area.height.saturating_sub(2);
            let prefix_h = prefix_rows.min(inner_height);
            let inline_h = inner_height.saturating_sub(prefix_h);

            let prefix_lines: Vec<Line> =
                new_lines.iter().take(prefix_rows_usize).cloned().collect();
            let inline_lines: Vec<Line> =
                new_lines.iter().skip(prefix_rows_usize).cloned().collect();
            if prefix_h > 0 {
                frame.render_widget(
                    Paragraph::new(prefix_lines)
                        .style(Style::default().bg(bg))
                        .scroll((0, h_scroll)),
                    Rect::new(inner_x, inner_y, inner_width, prefix_h),
                );
            }
            if inline_h > 0 {
                frame.render_widget(
                    Paragraph::new(inline_lines)
                        .style(Style::default().bg(bg))
                        .scroll((sub_scroll, h_scroll)),
                    Rect::new(inner_x, inner_y + prefix_h, inner_width, inline_h),
                );
            }
        }

        // Render annotation gutter markers on the shared border for new panel annotations.
        // Markers sit in the scrolled inline area, so shift their screen row by sub_scroll
        // and skip any that have scrolled above it.
        if let (Some(old_a), Some(_)) = (old_area, new_area) {
            let border_x = old_a.x + old_a.width - 1; // Right border of old panel
            let content_start = old_a.y + 1; // After top border of block
            let sub_scroll_usize = sub_scroll as usize;
            let buf = frame.buffer_mut();

            for &para_row in &border_marker_rows {
                if para_row < prefix_rows_usize + sub_scroll_usize {
                    continue; // scrolled off the top of the inline area
                }
                let shifted = para_row - sub_scroll_usize;
                let screen_row = content_start + shifted as u16;
                if screen_row < old_a.y + old_a.height - 1 {
                    if let Some(cell) =
                        buf.cell_mut(ratatui::layout::Position::new(border_x, screen_row))
                    {
                        cell.set_fg(t.ui.highlight);
                        cell.set_char('▎');
                    }
                }
            }
        }

        // Render annotation overlays per-panel for line-range annotations,
        // spanning both panels for file-level annotations.
        let is_side_by_side = old_area.is_some() && new_area.is_some();
        for &(line_pos, annotation) in &annotation_overlays {
            let (overlay_x, overlay_width, overlay_start_y, suppress_gutter) = match &annotation
                .target
            {
                AnnotationTarget::File => {
                    // File-level: span both panels
                    let render_area = old_area.or(new_area).unwrap_or(main_area);
                    let x = render_area.x + 1;
                    let w = if is_side_by_side {
                        old_area
                            .unwrap()
                            .width
                            .saturating_add(new_area.unwrap().width)
                            .saturating_sub(2)
                    } else {
                        render_area.width.saturating_sub(2)
                    };
                    (x, w, render_area.y + 1, false)
                }
                AnnotationTarget::LineRange { panel, .. } => {
                    // Line-range: render in the specific panel
                    let is_new_panel = matches!(panel, DiffPanelFocus::New | DiffPanelFocus::None);
                    let area = match panel {
                        DiffPanelFocus::Old => old_area.unwrap_or(main_area),
                        _ => new_area.unwrap_or(main_area),
                    };
                    // In side-by-side mode, new panel annotations use border markers on the
                    // shared border (via buffer_mut) instead of an inline gutter indicator,
                    // so suppress the gutter in the overlay.
                    let suppress = is_new_panel && is_side_by_side;
                    (
                        area.x + 1,
                        area.width.saturating_sub(2),
                        area.y + 1,
                        suppress,
                    )
                }
            };
            // File-level overlays sit in the pinned prefix; line-range overlays
            // sit in the scrolled inline area and shift up by sub_scroll.
            let in_inline = line_pos >= prefix_rows_usize;
            let shift = if in_inline { sub_scroll } else { 0 };
            // Top-clip prefix overlays at the inner top; clip inline overlays at
            // the inline area's top so a sub_scroll>0 overlay can't draw into the prefix.
            let overlay_min_y = if in_inline {
                overlay_start_y + (prefix_rows as u16).min(main_area.height.saturating_sub(2))
            } else {
                overlay_start_y
            };
            render_annotation_overlays(
                frame,
                &[(line_pos, annotation, shift)],
                overlay_x,
                overlay_start_y,
                overlay_width,
                main_area,
                bg,
                t,
                suppress_gutter,
                selected_annotation_id,
                overlay_min_y,
            );
        }

        // Extend border markers through new-panel annotation overlay rows on the shared border.
        // Markers shift by sub_scroll just like the inline content; rows scrolled above the
        // inline area's top are skipped.
        if let (Some(old_a), Some(_)) = (old_area, new_area) {
            let border_x = old_a.x + old_a.width - 1;
            let content_start = old_a.y + 1;
            let sub_scroll_usize = sub_scroll as usize;
            let buf = frame.buffer_mut();

            for &(line_pos, annotation) in &annotation_overlays {
                if let AnnotationTarget::LineRange { panel, .. } = &annotation.target {
                    if matches!(panel, DiffPanelFocus::New | DiffPanelFocus::None) {
                        let annotation_width =
                            overlay_content_width(annotation, old_area, new_area, main_area);
                        let num_rows = annotation_overlay_height(annotation, annotation_width);
                        for row_offset in 0..num_rows {
                            let absolute = line_pos + row_offset;
                            if absolute < prefix_rows_usize + sub_scroll_usize {
                                continue;
                            }
                            let shifted = absolute - sub_scroll_usize;
                            let screen_row = content_start + shifted as u16;
                            if screen_row < old_a.y + old_a.height - 1 {
                                if let Some(cell) = buf
                                    .cell_mut(ratatui::layout::Position::new(border_x, screen_row))
                                {
                                    cell.set_fg(t.ui.highlight);
                                    cell.set_char('▎');
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Merge the sidebar's right border with the diff panel's left border into
    // a single shared line by overwriting the corner cells with T-junctions.
    if show_sidebar {
        let junction_x = main_area.x;
        let top_y = main_area.y;
        let bottom_y = main_area.y + main_area.height.saturating_sub(1);
        let border_color = t.ui.border_unfocused;
        let buf = frame.buffer_mut();
        if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(junction_x, top_y)) {
            cell.set_char('┬');
            cell.set_fg(border_color);
        }
        if bottom_y > top_y {
            if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(junction_x, bottom_y)) {
                cell.set_char('┴');
                cell.set_fg(border_color);
            }
        }
    }

    render_footer(
        frame,
        footer_area,
        FooterData {
            filename: &diff.filename,
            commit_ref,
            watching,
            current_file,
            viewed_files,
            line_stats_added: line_stats.added,
            line_stats_removed: line_stats.removed,
            hunk_count,
            focused_hunk,
            search_state,
            ref_input,
            area_width: area.width,
            annotation_source_label,
            annotation_source_error,
            annotation_select_active: selected_annotation_id.is_some(),
            loading_message,
        },
    );

    (
        content_row_offset,
        overlay_gaps,
        total_annotation_height,
        overlay_heights,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::diff::annotation_store::{AnnotationId, Author, Comment};
    use std::time::UNIX_EPOCH;

    fn comment(origin: &str, content: &str) -> Comment {
        Comment {
            native_id: None,
            author: Author::for_origin(origin),
            origin: origin.to_string(),
            content: content.to_string(),
            created_at: UNIX_EPOCH,
        }
    }

    fn annotation(root: &str, replies: &[&str]) -> Annotation {
        let mut comments = vec![comment("ui", root)];
        comments.extend(replies.iter().map(|reply| comment("agent", reply)));

        Annotation {
            id: AnnotationId {
                origin: "ui".to_string(),
                native: "1".to_string(),
            },
            filename: "src/main.rs".to_string(),
            target: AnnotationTarget::File,
            comments,
            resolved: false,
        }
    }

    #[test]
    fn wrap_annotation_text_preserves_multiline_breaks() {
        assert_eq!(
            wrap_annotation_text("hello wide world\n\nnext line", 8),
            vec!["hello", "wide", "world", "", "next", "line"]
        );
    }

    #[test]
    fn wrap_annotation_text_splits_long_words() {
        assert_eq!(
            wrap_annotation_text("superlongword", 5),
            vec!["super", "longw", "ord"]
        );
    }

    #[test]
    fn annotation_overlay_height_uses_wrapped_comment_rows() {
        let annotation = annotation("root", &["reply wraps"]);

        assert_eq!(annotation_overlay_height(&annotation, 80), 5);
        assert_eq!(annotation_overlay_height(&annotation, 12), 6);
    }

    #[test]
    fn resolved_annotation_collapses_to_single_row() {
        let mut ann = annotation(
            "a long comment that would normally wrap",
            &["another reply"],
        );
        ann.resolved = true;

        assert_eq!(annotation_overlay_height(&ann, 80), 1);
        assert_eq!(annotation_overlay_height(&ann, 12), 1);
    }
}
