//! Attachments tray overlay (`Ctrl+T` or `[Attachments]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::MessageItem,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget, MessageScope},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    pub scope: MessageScope,
    pub msg_index: usize,
    pub attach_index: usize,
    pub name: String,
    pub content_type: String,
    pub author_name: String,
}

impl AttachmentRow {
    pub fn icon(&self) -> &'static str {
        let ct = self.content_type.to_ascii_lowercase();
        if ct.contains("image") && !ct.contains("svg") {
            "[IMG]"
        } else if ct.contains("html")
            || self.name.to_ascii_lowercase().ends_with(".html")
            || self.name.to_ascii_lowercase().ends_with(".htm")
        {
            "[HTML]"
        } else if ct.contains("card") {
            "[CARD]"
        } else {
            "[FILE]"
        }
    }

    pub fn label(&self) -> String {
        format!("{} {} · {}", self.icon(), self.name, self.author_name)
    }
}

pub fn rows_from(messages: &[MessageItem], scope: MessageScope) -> Vec<AttachmentRow> {
    let mut rows = Vec::new();
    for (msg_index, msg) in messages.iter().enumerate().rev() {
        if msg.deleted {
            continue;
        }
        for (attach_index, att) in msg.effective_attachments().iter().enumerate() {
            if att
                .content_type
                .to_ascii_lowercase()
                .contains("card.adaptive")
            {
                continue;
            }
            let name = att
                .name
                .clone()
                .unwrap_or_else(|| format!("Attachment #{}", attach_index + 1));
            rows.push(AttachmentRow {
                scope,
                msg_index,
                attach_index,
                name,
                content_type: att.content_type.clone(),
                author_name: msg.author_name.clone(),
            });
        }
    }
    rows
}

pub fn rows(app: &AppState) -> Vec<AttachmentRow> {
    let mut all = rows_from(&app.messages, MessageScope::Chat);
    if let Some(thread) = &app.active_thread {
        all.extend(rows_from(&thread.replies, MessageScope::ThreadReply));
    }
    all
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentsState {
    pub selected: usize,
}

impl AttachmentsState {
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let items = rows(app);
        match action {
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down(items.len() + 1);
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if self.selected == 0 {
                    return OverlayOutcome::CloseAndDispatch(Action::OpenSendFile);
                }
                match items.get(self.selected - 1) {
                    Some(row) => OverlayOutcome::Dispatch(Action::DownloadAttachment {
                        scope: row.scope,
                        msg_index: row.msg_index,
                        attach_index: row.attach_index,
                    }),
                    None => OverlayOutcome::Handled,
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(80),
            area.height.saturating_sub(4).min(22),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Attachments · Enter Download · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }

        let items = rows(app);
        let total = items.len() + 1;
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "{} attachments · {}/{}",
                    items.len(),
                    (self.selected + 1).min(total),
                    total
                ),
                theme.muted_style(),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let hint_y = inner.y + inner.height.saturating_sub(2);
        let list_y = inner.y.saturating_add(2);
        let visible = hint_y.saturating_sub(list_y) as usize;
        if visible == 0 {
            return;
        }

        let skip = self
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(total.saturating_sub(visible));
        for slot in 0..visible {
            let index = skip + slot;
            if index >= total {
                break;
            }
            let y = list_y + slot as u16;
            let selected = index == self.selected;
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let label = if index == 0 {
                " + Send File (Ctrl+O)".to_string()
            } else {
                format!(" {}", items[index - 1].label())
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    crate::ui::truncate_width(&label, inner.width as usize),
                    style,
                ))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }

        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(
                    &format!(" Downloads: {}", app.download_dir.display()),
                    inner.width as usize,
                ),
                theme.muted_style(),
            )),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Enter: download · first row sends file",
                theme.muted_style(),
            )),
            Rect::new(inner.x, hint_y.saturating_add(1), inner.width, 1),
        );
    }
}
