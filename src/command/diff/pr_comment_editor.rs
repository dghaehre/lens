use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use tui_textarea::TextArea;

use super::theme;

/// What the user is composing. Drives the modal title, the footer hint,
/// and (after save) which `gh` subcommand the app layer runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrCommentIntent {
    /// Issue-level PR comment ("Conversation" tab on github.com).
    Comment,
    /// Approving review — `gh pr review --approve` with the body attached
    /// as the review summary.
    Approve,
}

/// Outcome of one keystroke in the editor.
pub enum PrCommentEditorResult {
    Continue,
    /// User confirmed — call site should run the matching `gh` action
    /// with `content()`.
    Save,
    Cancel,
}

/// Multi-line composer for PR-level comments and review approvals. A
/// trimmed-down sibling of [`super::annotation::AnnotationEditor`]: the
/// keystroke handling matches (Enter saves, Ctrl+J/Shift+Enter inserts a
/// newline, Esc cancels) so muscle memory transfers across the two
/// editors.
pub struct PrCommentEditor<'a> {
    textarea: TextArea<'a>,
    pub intent: PrCommentIntent,
    /// Header label — typically the PR slug (`owner/repo#N`). Shown in
    /// the modal title so the user knows what they're commenting on.
    pub pr_slug: String,
}

impl<'a> PrCommentEditor<'a> {
    pub fn new(intent: PrCommentIntent, pr_slug: String) -> Self {
        let mut textarea = TextArea::default();
        let t = theme::get();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_cursor_style(Style::default().bg(t.ui.text_primary).fg(t.ui.bg));
        textarea.set_block(Block::default());
        Self {
            textarea,
            intent,
            pr_slug,
        }
    }

    /// Current draft body as a single string (lines joined with `\n`).
    pub fn content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn handle_input(&mut self, key: KeyEvent) -> PrCommentEditorResult {
        match key.code {
            KeyCode::Esc => PrCommentEditorResult::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                PrCommentEditorResult::Cancel
            }
            // Ctrl+S saves regardless of where the cursor is.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.try_save()
            }
            KeyCode::Enter => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL)
                {
                    self.textarea.insert_char('\n');
                    PrCommentEditorResult::Continue
                } else {
                    self.try_save()
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.textarea.insert_char('\n');
                PrCommentEditorResult::Continue
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.textarea.delete_line_by_head();
                PrCommentEditorResult::Continue
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.textarea.delete_line_by_head();
                PrCommentEditorResult::Continue
            }
            _ => {
                self.textarea.input(key);
                PrCommentEditorResult::Continue
            }
        }
    }

    /// Save unless the draft is empty *and* a non-empty body is required.
    /// Comments require a body (an empty PR comment is meaningless and
    /// rejected by `gh`); approvals don't (you can approve without
    /// comment).
    fn try_save(&self) -> PrCommentEditorResult {
        let body = self.content();
        if body.trim().is_empty() && self.intent == PrCommentIntent::Comment {
            // Empty comments fall back to cancel — user clearly didn't
            // mean to post a blank message.
            PrCommentEditorResult::Cancel
        } else {
            PrCommentEditorResult::Save
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let t = theme::get();
        let area = frame.area();

        let width = 70.min(area.width.saturating_sub(4));
        let height = 12.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let (verb, accent) = match self.intent {
            PrCommentIntent::Comment => ("Comment", t.ui.highlight),
            PrCommentIntent::Approve => ("Approve", t.ui.status_added),
        };
        let title = format!(" {} on {} ", verb, self.pr_slug);

        let block = Block::default()
            .title(title)
            .title_style(Style::default().fg(accent).bold())
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(t.ui.border_focused))
            .style(Style::default().bg(t.ui.bg));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        frame.render_widget(&self.textarea, chunks[0]);

        let muted = Style::default().fg(t.ui.text_muted);
        let sep = Style::default().fg(t.ui.border_unfocused);
        let save_hint = match self.intent {
            PrCommentIntent::Comment => " post  ",
            PrCommentIntent::Approve => " approve  ",
        };
        let footer_text = Line::from(vec![
            Span::styled("enter", muted),
            Span::styled(save_hint, muted),
            Span::styled("│  ", sep),
            Span::styled("esc", muted),
            Span::styled(" cancel  ", muted),
            Span::styled("│  ", sep),
            Span::styled("ctrl+j", muted),
            Span::styled(" newline", muted),
        ]);
        let footer = Paragraph::new(footer_text)
            .style(Style::default().bg(t.ui.bg))
            .alignment(Alignment::Center);
        frame.render_widget(footer, chunks[1]);
    }
}
