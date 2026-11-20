//! Right-click / `m` context menu.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::ConversationKind,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget, MessageScope},
        overlays::{OverlayKind, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    NavRow(usize),
    Message {
        scope: MessageScope,
        msg_index: usize,
    },
    Composer,
    Global,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub action: Action,
}

impl MenuItem {
    fn new(label: impl Into<String>, shortcut: Option<&str>, action: Action) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            action,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub context: ContextTarget,
    pub anchor: (u16, u16),
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ContextMenu {
    pub fn build(target: ContextTarget, anchor: (u16, u16), app: &AppState, ui: &UiState) -> Self {
        let items = match target {
            ContextTarget::NavRow(index) => vec![
                MenuItem::new("Open", Some("Enter"), Action::SelectNavRow(index)),
                MenuItem::new("Refresh", Some("F5"), Action::RefreshAll),
                MenuItem::new(
                    "New Chat",
                    Some("Ctrl+N"),
                    Action::OpenOverlay(OverlayKind::NewChat),
                ),
            ],
            ContextTarget::Message { scope, msg_index } => {
                let msg = app.message_at(scope, msg_index);
                let is_channel_root = scope == MessageScope::Chat
                    && app
                        .active_conversation()
                        .is_some_and(|c| c.kind == ConversationKind::Channel)
                    && msg.is_some_and(|m| m.parent_msg_id.is_none());
                let has_attach = msg.is_some_and(|m| !m.attachments.is_empty());
                let is_me = msg.is_some_and(|m| m.is_me);

                let mut items = Vec::new();
                if is_channel_root {
                    items.push(MenuItem::new(
                        "Open Thread",
                        Some("t"),
                        Action::OpenThread { msg_index },
                    ));
                }
                items.push(MenuItem::new(
                    "Reply / Quote",
                    Some("r"),
                    Action::ReplyTo { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Add Reaction",
                    Some("+"),
                    Action::OpenReactions { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Copy Message",
                    Some("y"),
                    Action::CopyMessage { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Inspect HTML / Card",
                    Some("H"),
                    Action::OpenHtmlPreviewForMessage { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Extract Links (URLs)",
                    Some("u"),
                    Action::OpenLinks,
                ));
                items.push(MenuItem::new(
                    "Toggle Inline Link URLs",
                    Some("h"),
                    Action::ToggleShowHrefs,
                ));
                if has_attach {
                    let attach_index = ui.attach_index.min(
                        msg.map_or(0, |m| m.attachments.len().saturating_sub(1)),
                    );
                    items.push(MenuItem::new(
                        "Download Attachment",
                        Some("d"),
                        Action::DownloadAttachment {
                            scope,
                            msg_index,
                            attach_index,
                        },
                    ));
                }
                if is_me {
                    items.push(MenuItem::new(
                        "Delete Message",
                        Some("Del"),
                        Action::RequestDeleteMessage { scope, msg_index },
                    ));
                }
                items
            }
            ContextTarget::Composer => vec![
                MenuItem::new("Send", Some("Enter"), Action::SendMessage),
                MenuItem::new("Toggle Subject", Some("Ctrl+S"), Action::ToggleSubject),
                MenuItem::new("External Editor", Some("Ctrl+E"), Action::OpenExternalEditor),
                MenuItem::new("Cancel Reply", Some("Esc"), Action::CancelReply),
                MenuItem::new(
                    "Clear Pending Images",
                    Some("Ctrl+Shift+X"),
                    Action::ClearPendingImages,
                ),
                MenuItem::new("Clear Draft", None, Action::ClearComposer),
            ],
            ContextTarget::Global => vec![
                MenuItem::new("Search", Some("/"), Action::OpenSearch),
                MenuItem::new("Browse & Attach File", Some("Ctrl+F"), Action::OpenFilePicker),
                MenuItem::new(
                    "New Chat",
                    Some("Ctrl+N"),
                    Action::OpenOverlay(OverlayKind::NewChat),
                ),
                MenuItem::new(
                    "Change Presence",
                    Some("Ctrl+P"),
                    Action::OpenOverlay(OverlayKind::Presence),
                ),
                MenuItem::new("Refresh", Some("F5"), Action::RefreshAll),
                MenuItem::new(
                    "Settings",
                    Some("Ctrl+,"),
                    Action::OpenOverlay(OverlayKind::Settings),
                ),
                MenuItem::new(
                    "Help",
                    Some("?"),
                    Action::OpenOverlay(OverlayKind::Help),
                ),
                MenuItem::new("Quit", Some("Ctrl+Q"), Action::QuitConfirm),
            ],
        };

        Self {
            context: target,
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    pub fn activate(&self) -> OverlayOutcome {
        match self.items.get(self.selected) {
            Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
            None => OverlayOutcome::Close,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let label_width = self
            .items
            .iter()
            .map(|item| {
                unicode_width::UnicodeWidthStr::width(item.label.as_str())
                    + item
                        .shortcut
                        .as_deref()
                        .map_or(0, |shortcut| shortcut.len() + 2)
            })
            .max()
            .unwrap_or(18);
        let width = (label_width as u16 + 4).clamp(22, 48);
        let height = (self.items.len() as u16 + 2).min(area.height);
        let rect = anchored_rect(area, self.anchor, width, height);

        let title = match self.context {
            ContextTarget::NavRow(_) => " Conversation Menu ",
            ContextTarget::Message { .. } => " Message Menu ",
            ContextTarget::Composer => " Composer Menu ",
            ContextTarget::Global => " Menu ",
        };
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        for (index, item) in self.items.iter().enumerate() {
            if index as u16 >= inner.height {
                break;
            }
            let row = inner.y + index as u16;
            let selected = index == self.selected;
            let label_style = if selected {
                theme.selection().remove_modifier(Modifier::BOLD)
            } else {
                ratatui::style::Style::default()
            };
            let hint_style = if selected {
                theme.selection()
            } else {
                theme.muted_style()
            };
            let hint = item.shortcut.clone().unwrap_or_default();
            let hint_w = unicode_width::UnicodeWidthStr::width(hint.as_str());
            let max_label_w = (inner.width as usize).saturating_sub(hint_w + 2);
            let label = crate::ui::truncate_width(&item.label, max_label_w);
            let used = unicode_width::UnicodeWidthStr::width(label.as_str());
            let pad = (inner.width as usize)
                .saturating_sub(used)
                .saturating_sub(hint_w)
                .saturating_sub(2);
            let line = Line::from(vec![
                Span::styled(format!(" {label}"), label_style),
                Span::styled(" ".repeat(pad), label_style),
                Span::styled(hint, hint_style),
                Span::styled(" ", label_style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, row, inner.width, 1),
            );
            hitmap.add_line(inner.x, row, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }
}

fn anchored_rect(area: Rect, anchor: (u16, u16), width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = anchor.0.min(area.x + area.width.saturating_sub(width));
    let y = anchor.1.min(area.y + area.height.saturating_sub(height));
    Rect::new(x, y, width, height)
}
