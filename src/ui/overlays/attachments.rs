//! Attachments tray overlay (`Ctrl+T` or `[Attachments]`).

use bitefoundry_html::ContentKind;
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
    pub fn content_kind(&self) -> ContentKind {
        let kind = bitefoundry_html::sniff_content_kind(
            Some(self.content_type.as_str()),
            Some(self.name.as_str()),
            None,
        );
        if matches!(kind, ContentKind::PlainText | ContentKind::Binary)
            && bitefoundry_html::sniff_content_kind(None, Some(self.name.as_str()), None)
                == ContentKind::Markdown
        {
            return ContentKind::Markdown;
        }
        kind
    }

    pub fn is_markdown(&self) -> bool {
        self.content_kind() == ContentKind::Markdown
    }

    pub fn is_previewable_markup(&self) -> bool {
        matches!(
            self.content_kind(),
            ContentKind::Html | ContentKind::Markdown
        )
    }

    pub fn icon(&self) -> &'static str {
        let ct = self.content_type.to_ascii_lowercase();
        if ct.contains("card") {
            "[CARD]"
        } else {
            match self.content_kind() {
                ContentKind::RasterImage => "[IMG]",
                ContentKind::Html => "[HTML]",
                ContentKind::Markdown => "[MD]",
                _ if ct.contains("image") && !ct.contains("svg") => "[IMG]",
                _ => "[FILE]",
            }
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
            // A quoted-reply reference points at another message: there is no file to
            // preview or download, so listing it would only lead to a failed download.
            if att.is_message_reference()
                || att
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
        // A thread opened from Activity or search can have a root outside the loaded channel
        // window; when the window has it, its files are already listed as chat rows.
        if !app.messages.iter().any(|m| m.msg_id == thread.root.msg_id) {
            all.extend(rows_from(
                std::slice::from_ref(&thread.root),
                MessageScope::ThreadRoot,
            ));
        }
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
                    Some(row) if row.is_previewable_markup() => {
                        OverlayOutcome::CloseAndDispatch(Action::PreviewAttachment {
                            scope: row.scope,
                            msg_index: row.msg_index,
                            attach_index: row.attach_index,
                        })
                    }
                    Some(row) => OverlayOutcome::Dispatch(Action::DownloadAttachment {
                        scope: row.scope,
                        msg_index: row.msg_index,
                        attach_index: row.attach_index,
                    }),
                    None => OverlayOutcome::Handled,
                }
            }
            Action::Input('v') | Action::OpenImagePreview => {
                if self.selected == 0 {
                    return OverlayOutcome::Handled;
                }
                match items.get(self.selected - 1) {
                    Some(row) if row.is_previewable_markup() || row.icon() == "[IMG]" => {
                        OverlayOutcome::CloseAndDispatch(Action::PreviewAttachment {
                            scope: row.scope,
                            msg_index: row.msg_index,
                            attach_index: row.attach_index,
                        })
                    }
                    _ => OverlayOutcome::Handled,
                }
            }
            Action::Input('d') | Action::DownloadLatest => {
                if self.selected == 0 {
                    return OverlayOutcome::Handled;
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
                " Attachments · Enter/v Preview · d Download · Esc Close ",
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
                " Enter/v: preview HTML/MD or download · d: download · first row sends file",
                theme.muted_style(),
            )),
            Rect::new(inner.x, hint_y.saturating_add(1), inner.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AttachmentInfo, ThreadView};
    use crate::ui::app::tests::make_test_app;

    fn msg_with(conv_key: &str, msg_id: &str, attachments: Vec<AttachmentInfo>) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u1".to_string()),
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-20T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: "<p>see attached</p>".to_string(),
            body_plain: "see attached".to_string(),
            attachments,
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }
    }

    fn attachment(id: &str, content_type: &str, name: Option<&str>) -> AttachmentInfo {
        AttachmentInfo {
            id: id.to_string(),
            content_type: content_type.to_string(),
            content_url: Some(format!("https://contoso.sharepoint.com/{id}")),
            name: name.map(str::to_string),
            content_json: None,
            thumbnail_url: None,
        }
    }

    // Track 5: a quoted-reply reference is not a file, so the attachments list skips it.
    #[test]
    fn rows_from_skips_message_references_and_cards() {
        let msg = msg_with(
            "chat:c1",
            "m1",
            vec![
                attachment("quoted", "messageReference", None),
                attachment("card", "application/vnd.microsoft.card.adaptive", None),
                attachment("file", "reference", Some("report.pdf")),
            ],
        );
        let rows = rows_from(std::slice::from_ref(&msg), MessageScope::Chat);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "report.pdf");
        assert_eq!(rows[0].attach_index, 2, "the index still points into the message");
    }

    // Track 5: a thread opened outside the loaded channel window lists its root's files once.
    #[tokio::test]
    async fn rows_include_active_thread_root_attachments_once() {
        let (mut app, _ui) = make_test_app();
        let conv = "chan:t1:ch1";
        let root = msg_with(
            conv,
            "root-1",
            vec![attachment("plan", "reference", Some("plan.docx"))],
        );
        let mut reply = msg_with(
            conv,
            "rep-1",
            vec![attachment("sheet", "reference", Some("numbers.xlsx"))],
        );
        reply.parent_msg_id = Some("root-1".to_string());
        app.active_thread = Some(ThreadView {
            conv_key: conv.to_string(),
            root: root.clone(),
            replies: vec![reply],
        });

        // Root outside the channel window: it is listed from the thread.
        app.messages = Vec::new();
        let names: Vec<(String, MessageScope)> =
            rows(&app).into_iter().map(|r| (r.name, r.scope)).collect();
        assert_eq!(
            names,
            vec![
                ("numbers.xlsx".to_string(), MessageScope::ThreadReply),
                ("plan.docx".to_string(), MessageScope::ThreadRoot),
            ]
        );

        // Root inside the window: listed once, as a chat row.
        app.messages = vec![root];
        let names: Vec<(String, MessageScope)> =
            rows(&app).into_iter().map(|r| (r.name, r.scope)).collect();
        assert_eq!(
            names,
            vec![
                ("plan.docx".to_string(), MessageScope::Chat),
                ("numbers.xlsx".to_string(), MessageScope::ThreadReply),
            ]
        );

        app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }
}
