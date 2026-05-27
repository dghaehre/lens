use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    prelude::*,
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};

use crate::command::diff::state::{Annotation, AnnotationId};
use crate::command::diff::theme;

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

#[derive(Clone, Copy)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone)]
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
    Annotations {
        title: String,
        items: Vec<String>,
        annotations: Vec<Annotation>,
        selected: usize,
        export_input: Option<String>,
        /// Error message to display (e.g., for failed export)
        error_message: Option<String>,
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
    AnnotationJump {
        annotation_id: AnnotationId,
    },
    AnnotationEdit {
        annotation_id: AnnotationId,
    },
    AnnotationReply {
        annotation_id: AnnotationId,
    },
    AnnotationDelete {
        annotation_id: AnnotationId,
    },
    AnnotationToggleResolved {
        annotation_id: AnnotationId,
    },
    AnnotationCopyAll,
    AnnotationExport(String),
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

    pub fn annotations(
        title: impl Into<String>,
        items: Vec<String>,
        annotations: Vec<Annotation>,
    ) -> Self {
        Self {
            content: ModalContent::Annotations {
                title: title.into(),
                items,
                annotations,
                selected: 0,
                export_input: None,
                error_message: None,
            },
        }
    }

    pub fn render(&self, frame: &mut Frame) {
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
                let width = 60.min(area.width.saturating_sub(4));
                let total_lines: usize = sections
                    .iter()
                    .map(|s| s.bindings.len() + 2) // +2 for section title and spacing
                    .sum();
                let height = (total_lines as u16 + 4).min(area.height * 80 / 100).max(5);
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
            ModalContent::Annotations {
                items,
                export_input,
                ..
            } => {
                let width = 120.min(area.width.saturating_sub(4));
                let items_count = items.len().min(14) as u16;
                let extra = if export_input.is_some() { 4 } else { 2 };
                let height = (items_count + extra + 2).min(area.height * 85 / 100).max(12);
                (width, height)
            }
        };

        let modal_x = (area.width.saturating_sub(modal_width)) / 2;
        let modal_y = (area.height.saturating_sub(modal_height)) / 2;
        let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

        frame.render_widget(Clear, modal_area);

        match &self.content {
            ModalContent::Info { title, message } => {
                self.render_info(frame, modal_area, title, message);
            }
            ModalContent::Select {
                title,
                items,
                selected,
            } => {
                self.render_select(frame, modal_area, title, items, *selected);
            }
            ModalContent::KeyBindings {
                title,
                sections,
                scroll,
                content_height,
            } => {
                self.render_keybindings(
                    frame,
                    modal_area,
                    title,
                    sections,
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
                self.render_file_picker(
                    frame,
                    modal_area,
                    title,
                    items,
                    filtered_indices,
                    query,
                    *selected,
                );
            }
            ModalContent::Annotations {
                title,
                items,
                annotations,
                selected,
                export_input,
                error_message,
            } => {
                self.render_annotations(
                    frame,
                    modal_area,
                    title,
                    items,
                    annotations,
                    *selected,
                    export_input.as_deref(),
                    error_message.as_deref(),
                );
            }
        }
    }

    fn render_info(&self, frame: &mut Frame, area: Rect, title: &str, message: &str) {
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
        &self,
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
        &self,
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

        // Reserve space for scrollbar on the right
        let content_area = Rect::new(
            inner.x,
            inner.y,
            inner.width.saturating_sub(1),
            inner.height,
        );

        let para = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(para, content_area);

        // Render scrollbar if content exceeds visible area
        let visible_height = inner.height;
        if content_height > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("█");

            let mut scrollbar_state =
                ScrollbarState::new(content_height.saturating_sub(visible_height) as usize)
                    .position(scroll as usize);

            frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_file_picker(
        &self,
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
    fn render_annotations(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: &[String],
        annotations: &[Annotation],
        selected: usize,
        export_input: Option<&str>,
        error_message: Option<&str>,
    ) {
        let t = theme::get();

        let title_text = format!(" {} ({}) ", title, items.len());

        let block = Block::default()
            .title(title_text)
            .title_style(Style::default().fg(t.ui.text_secondary))
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

        // Render the annotation list (left pane).
        render_annotation_list(frame, list_area, items, selected, t);

        // Render the thread preview (right pane).
        if let Some(area) = preview_area {
            let preview = annotations.get(selected);
            render_annotation_preview(frame, area, preview, t);
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
        }

        // Compact footer
        let footer_text = if export_input.is_some() {
            Line::from(vec![
                Span::styled("enter", Style::default().fg(t.ui.text_muted)),
                Span::styled(" confirm  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("esc", Style::default().fg(t.ui.text_muted)),
                Span::styled(" cancel", Style::default().fg(t.ui.text_muted)),
            ])
        } else {
            Line::from(vec![
                Span::styled("enter", Style::default().fg(t.ui.text_muted)),
                Span::styled(" jump  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("e", Style::default().fg(t.ui.text_muted)),
                Span::styled(" edit  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("r", Style::default().fg(t.ui.text_muted)),
                Span::styled(" reply  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("x", Style::default().fg(t.ui.text_muted)),
                Span::styled(" resolve  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("d", Style::default().fg(t.ui.text_muted)),
                Span::styled(" del  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("y", Style::default().fg(t.ui.text_muted)),
                Span::styled(" copy  ", Style::default().fg(t.ui.text_muted)),
                Span::styled("│  ", Style::default().fg(t.ui.border_unfocused)),
                Span::styled("o", Style::default().fg(t.ui.text_muted)),
                Span::styled(" export", Style::default().fg(t.ui.text_muted)),
            ])
        };
        let footer = Paragraph::new(footer_text).alignment(ratatui::prelude::Alignment::Center);
        frame.render_widget(footer, footer_area);
    }

    /// Handle mouse scroll for the modal.
    /// Returns true if the scroll was handled.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, terminal_height: u16) -> bool {
        if let ModalContent::KeyBindings {
            scroll,
            content_height,
            ..
        } = &mut self.content
        {
            let visible_height =
                calculate_keybindings_visible_height(terminal_height, *content_height);
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
        } else {
            false
        }
    }

    /// Handle keyboard input for the modal.
    /// Returns Some(ModalResult) if the modal should close.
    pub fn handle_input(&mut self, key: KeyEvent, terminal_height: u16) -> Option<ModalResult> {
        // FilePicker and Annotations handle their own dismiss logic
        if !matches!(
            self.content,
            ModalContent::FilePicker { .. } | ModalContent::Annotations { .. }
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
            ModalContent::Annotations {
                items,
                annotations,
                selected,
                export_input,
                error_message,
                ..
            } => {
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
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationJump {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('e') => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationEdit {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('r') => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationReply {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('d') => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationDelete {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('x') => {
                            annotations
                                .get(*selected)
                                .map(|ann| ModalResult::AnnotationToggleResolved {
                                    annotation_id: ann.id.clone(),
                                })
                        }
                        KeyCode::Char('y') => Some(ModalResult::AnnotationCopyAll),
                        KeyCode::Char('o') => {
                            *export_input = Some(String::from("annotations.txt"));
                            None
                        }
                        _ => None,
                    }
                }
            }
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
fn calculate_keybindings_visible_height(terminal_height: u16, content_height: u16) -> u16 {
    // Modal height calculation from render: (total_lines + 4).min(height * 80 / 100).max(5)
    let modal_height = (content_height + 4).min(terminal_height * 80 / 100).max(5);
    // Subtract 2 for top/bottom borders
    modal_height.saturating_sub(2)
}

/// Render the left-hand annotation picker list. Each entry shows
/// `location` on the left and the time on the right; the body preview lives
/// in the right-hand pane so we no longer crowd it into the row.
fn render_annotation_list(
    frame: &mut Frame,
    area: Rect,
    items: &[String],
    selected: usize,
    t: &theme::Theme,
) {
    let visible_count = area.height as usize;
    let scroll_offset = if selected >= visible_count {
        selected - visible_count + 1
    } else {
        0
    };

    let list_items: Vec<ListItem> = items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(i, item)| {
            let is_selected = i == selected;
            // Format produced by `build_annotation_list`: "location | preview | HH:MM".
            // We only show location + time in the list — the preview moves to the
            // right pane.
            let parts: Vec<&str> = item.splitn(3, " | ").collect();
            let location = parts.first().copied().unwrap_or("");
            let time = parts.get(2).copied().unwrap_or("");

            let available = area.width as usize;
            let time_w = time.chars().count() + 2; // " HH:MM "
            let location_max = available.saturating_sub(time_w + 2).max(8);
            let truncated_location = if location.chars().count() > location_max && location_max > 3
            {
                let truncate_at = location_max - 1;
                let truncated: String = location.chars().take(truncate_at).collect();
                format!("{}…", truncated)
            } else {
                location.to_string()
            };

            let used = truncated_location.chars().count() + 2 + time_w;
            let padding = available.saturating_sub(used);

            let spans = if is_selected {
                let sel = Style::default().fg(t.ui.selection_fg).bg(t.ui.selection_bg);
                vec![
                    Span::styled(format!(" {} ", truncated_location), sel),
                    Span::styled(format!("{:>width$}", "", width = padding), sel),
                    Span::styled(format!(" {} ", time), sel),
                ]
            } else {
                vec![
                    Span::styled(
                        format!(" {} ", truncated_location),
                        Style::default().fg(t.ui.text_secondary),
                    ),
                    Span::styled(format!("{:>width$}", "", width = padding), Style::default()),
                    Span::styled(format!(" {} ", time), Style::default().fg(t.ui.text_muted)),
                ]
            };
            ListItem::new(Line::from(spans))
        })
        .collect();

    frame.render_widget(List::new(list_items), area);
}

/// Render the right-hand preview pane for the currently-selected annotation:
/// thread anchor (file + line range), then every comment in the thread with
/// a short header and wrapped body.
fn render_annotation_preview(
    frame: &mut Frame,
    area: Rect,
    annotation: Option<&Annotation>,
    t: &theme::Theme,
) {
    use crate::command::diff::state::AnnotationTarget;

    let Some(ann) = annotation else {
        let empty = Paragraph::new(Line::from(Span::styled(
            "no annotations",
            Style::default().fg(t.ui.text_muted).italic(),
        )))
        .alignment(ratatui::prelude::Alignment::Center);
        frame.render_widget(empty, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // Anchor header: filename + line range, plus a "resolved" badge.
    let location = match &ann.target {
        AnnotationTarget::File => format!("{} [file]", ann.filename),
        AnnotationTarget::LineRange { .. } => {
            format!("{}:{}", ann.filename, ann.line_range_display())
        }
    };
    let mut header_spans: Vec<Span> = vec![Span::styled(
        location,
        Style::default().fg(t.ui.text_primary).bold(),
    )];
    if ann.resolved {
        header_spans.push(Span::styled("  ", Style::default()));
        header_spans.push(Span::styled(
            "✓ resolved",
            Style::default().fg(t.ui.status_added).bold(),
        ));
    }
    lines.push(Line::from(header_spans));
    lines.push(Line::from(""));

    // Render every comment in the thread.
    let wrap_width = (area.width.saturating_sub(2) as usize).max(10);
    for (i, comment) in ann.comments.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "↳ reply",
                Style::default().fg(t.ui.text_muted),
            )));
        }
        let header = format!(
            "{}  {}",
            comment.author.login,
            format_comment_time(comment.created_at),
        );
        lines.push(Line::from(Span::styled(
            header,
            Style::default().fg(t.ui.text_secondary),
        )));
        for body_line in comment.content.lines() {
            for wrapped in wrap_preview_line(body_line, wrap_width) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(t.ui.text_primary),
                )));
            }
            if body_line.is_empty() {
                lines.push(Line::from(""));
            }
        }
    }

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
