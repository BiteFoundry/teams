//! New Chat overlay (`Ctrl+N` or `[+ New Chat]`):
//! Enter comma-separated user emails/IDs or search users via `graph.search_users`,
//! optionally specify a group topic, and call `graph.create_chat(...)`.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Default)]
pub struct NewChatState {
    pub peers_input: String,
    pub topic: String,
    pub selected_peers: Vec<String>,
    /// `0` = peers/search input, `1` = optional topic, `2` = search result / submit list
    pub field: usize,
    pub selected_result: usize,
    pub error: Option<String>,
}

impl NewChatState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select(&mut self, index: usize) {
        self.field = 2;
        self.selected_result = index;
    }

    pub fn parsed_peers(&self) -> Vec<String> {
        let mut peers = self.selected_peers.clone();
        for part in self.peers_input.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() && !peers.iter().any(|p| p.eq_ignore_ascii_case(trimmed)) {
                peers.push(trimmed.to_string());
            }
        }
        peers
    }

    fn looks_like_direct_ids(&self) -> bool {
        let trimmed = self.peers_input.trim();
        !trimmed.is_empty() && (trimmed.contains('@') || trimmed.contains(','))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let result_count = app.user_search_results.len();
        match action {
            Action::Input(ch) => {
                self.error = None;
                match self.field {
                    0 => {
                        self.peers_input.push(*ch);
                        if !self.peers_input.contains(',') && self.peers_input.trim().len() >= 2 {
                            return OverlayOutcome::Dispatch(Action::SearchUsers(
                                self.peers_input.trim().to_string(),
                            ));
                        }
                    }
                    1 => self.topic.push(*ch),
                    _ => {
                        self.field = 0;
                        self.peers_input.push(*ch);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.error = None;
                match self.field {
                    1 => self.topic.push_str(text),
                    _ => {
                        self.field = 0;
                        self.peers_input.push_str(text);
                        if !self.peers_input.contains(',') && self.peers_input.trim().len() >= 2 {
                            return OverlayOutcome::Dispatch(Action::SearchUsers(
                                self.peers_input.trim().to_string(),
                            ));
                        }
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.error = None;
                match self.field {
                    0 => {
                        if self.peers_input.is_empty() {
                            self.selected_peers.pop();
                        } else {
                            self.peers_input.pop();
                        }
                    }
                    1 => {
                        self.topic.pop();
                    }
                    _ => {}
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Tab => {
                if self.field == 0 {
                    self.field = 1;
                } else if self.field == 1 {
                    if result_count > 0 {
                        self.field = 2;
                        self.selected_result = 0;
                    } else {
                        self.field = 0;
                    }
                } else if self.selected_result + 1 < result_count {
                    self.selected_result += 1;
                } else {
                    self.field = 0;
                }
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::PrevFocus => {
                if self.field == 2 {
                    if self.selected_result > 0 {
                        self.selected_result -= 1;
                    } else {
                        self.field = 1;
                    }
                } else if self.field == 1 {
                    self.field = 0;
                } else if result_count > 0 {
                    self.field = 2;
                    self.selected_result = result_count - 1;
                } else {
                    self.field = 1;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                // Selecting a user from search results appends them to `selected_peers`.
                if self.field == 2 {
                    if let Some(user) = app.user_search_results.get(self.selected_result) {
                        let identifier = user
                            .user_principal_name
                            .clone()
                            .or_else(|| user.mail.clone())
                            .unwrap_or_else(|| user.id.clone());
                        if !self
                            .selected_peers
                            .iter()
                            .any(|p| p.eq_ignore_ascii_case(&identifier))
                        {
                            self.selected_peers.push(identifier);
                        }
                        self.peers_input.clear();
                        self.field = 0;
                        return OverlayOutcome::Handled;
                    }
                }

                // If typing a name without `@`/`,` and no peers chosen yet, run user search.
                if self.field == 0
                    && !self.peers_input.trim().is_empty()
                    && !self.looks_like_direct_ids()
                    && self.selected_peers.is_empty()
                {
                    if let Some(user) = app.user_search_results.first() {
                        let id = user
                            .user_principal_name
                            .clone()
                            .or_else(|| user.mail.clone())
                            .unwrap_or_else(|| user.id.clone());
                        self.selected_peers.push(id);
                        self.peers_input.clear();
                    } else {
                        return OverlayOutcome::Dispatch(Action::SearchUsers(
                            self.peers_input.trim().to_string(),
                        ));
                    }
                }

                let peers = self.parsed_peers();
                if peers.is_empty() {
                    self.error = Some("Enter an email or user ID".to_string());
                    return OverlayOutcome::Handled;
                }
                let topic = {
                    let t = self.topic.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                };
                OverlayOutcome::CloseAndDispatch(Action::CreateChat { peers, topic })
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
            area.height.saturating_sub(4).min(20),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " New Chat · Tab Field · Enter Add/Create · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let peer_style = if self.field == 0 {
            theme.accent_style()
        } else {
            theme.muted_style()
        };
        let topic_style = if self.field == 1 {
            theme.accent_style()
        } else {
            theme.muted_style()
        };

        let input_viewport = (inner.width as usize).saturating_sub(3).max(1);
        let render_input_line = |text: &str, style: Style| {
            let win = bitefoundry_html::layout::windowed_single_line_input(
                text,
                text.len(),
                input_viewport,
            );
            let mut spans = vec![Span::styled(" > ", style)];
            if win.clipped_left {
                spans.push(Span::styled("‹", theme.muted_style()));
            }
            spans.push(Span::raw(win.before_cursor));
            spans.push(Span::styled("▏", style));
            if !win.after_cursor.is_empty() {
                spans.push(Span::raw(win.after_cursor));
            }
            if win.clipped_right {
                spans.push(Span::styled("›", theme.muted_style()));
            }
            Line::from(spans)
        };

        let mut lines = vec![
            Line::from(Span::styled(
                " Recipients (comma-separated email/ID or search name):",
                theme.muted_style(),
            )),
            render_input_line(&self.peers_input, peer_style),
            Line::from(Span::styled(
                if self.selected_peers.is_empty() {
                    " Selected: none".to_string()
                } else {
                    format!(" Selected: {}", self.selected_peers.join(", "))
                },
                theme.accent_style(),
            )),
            Line::from(Span::styled(
                " Group topic (optional for 2+ recipients):",
                theme.muted_style(),
            )),
            render_input_line(&self.topic, topic_style),
        ];

        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!(" Error: {err}"),
                Style::default().fg(theme.error),
            )));
        } else if app.user_searching {
            lines.push(Line::from(Span::styled(
                " Searching users…",
                theme.muted_style(),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                " Search results (click or ↓ then Enter to add):",
                theme.muted_style(),
            )));
        }

        let header_rows = lines.len() as u16;
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(inner.x, inner.y, inner.width, header_rows.min(inner.height)),
        );

        let list_y = inner.y.saturating_add(header_rows);
        let visible = inner.height.saturating_sub(header_rows) as usize;
        if visible == 0 {
            return;
        }
        let active_sel = self
            .selected_result
            .min(app.user_search_results.len().saturating_sub(1));
        let skip = if self.field == 2 {
            active_sel.saturating_sub(visible.saturating_sub(1))
        } else {
            0
        };
        for (row, (idx, user)) in app
            .user_search_results
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
            let y = list_y + row as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let selected = self.field == 2 && self.selected_result == idx;
            let style = if selected {
                theme.selection()
            } else {
                ratatui::style::Style::default()
            };
            let email = user
                .mail
                .as_deref()
                .or(user.user_principal_name.as_deref())
                .unwrap_or(&user.id);
            let label = format!("  {} ({email})", user.display_name);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&label, inner.width as usize),
                    style.add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                )),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_separated_emails_and_dedups() {
        let mut s = NewChatState::new();
        s.selected_peers.push("alice@contoso.com".into());
        s.peers_input = "alice@contoso.com, bob@contoso.com , ".into();
        assert_eq!(
            s.parsed_peers(),
            vec!["alice@contoso.com", "bob@contoso.com"]
        );
    }

    #[test]
    fn backspace_pops_selected_peers_only_when_field0_empty_and_paste_searches() {
        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = crate::sync::SyncController::from_channels(
            tokio::sync::watch::channel(crate::sync::FocusTarget::default()).0,
            event_tx,
            false,
        );
        let (ui_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);

        let mut s = NewChatState::new();
        s.selected_peers.push("alice@contoso.com".into());
        s.field = 2;
        s.handle(&Action::Backspace, &app);
        assert_eq!(s.selected_peers.len(), 1);

        s.field = 0;
        assert_eq!(
            s.handle(&Action::Paste("bob".into()), &app),
            OverlayOutcome::Dispatch(Action::SearchUsers("bob".into()))
        );
        s.handle(&Action::Backspace, &app);
        assert_eq!(s.peers_input, "bo");
        assert_eq!(s.selected_peers.len(), 1);

        s.peers_input.clear();
        s.handle(&Action::Backspace, &app);
        assert!(s.selected_peers.is_empty());
    }
}
