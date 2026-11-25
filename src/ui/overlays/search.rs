//! Search overlay (`Ctrl+F`, `/`): queries `storage.search_messages(...)` live
//! and jumps directly to the conversation (opening thread if `parent_msg_id.is_some()`).

use ratatui::{
    layout::Rect,
    style::Style,
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
pub struct SearchOverlay {
    pub query: String,
    pub selected: usize,
}

impl SearchOverlay {
    pub fn reset_selection(&mut self) {
        self.selected = 0;
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::Search(self.query.clone()))
            }
            Action::Paste(text) => {
                self.query.push_str(text);
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::Search(self.query.clone()))
            }
            Action::Backspace => {
                let _ = self.query.pop();
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::Search(self.query.clone()))
            }
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down(app.search_results.len());
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let query = self.query.trim().to_string();
                if query.is_empty() {
                    return OverlayOutcome::Handled;
                }
                let len = app.search_results.len();
                if query == app.search_query.trim() && len > 0 && self.selected < len {
                    return OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(
                        self.selected,
                    ));
                }
                OverlayOutcome::Dispatch(Action::Search(query))
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
            area.width.saturating_sub(8).min(88),
            area.height.saturating_sub(4).min(22),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let title = " Search Messages · Enter Jump · Esc Close ";
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }

        let query_viewport = (inner.width as usize).saturating_sub(7).max(1);
        let win = bitefoundry_html::layout::windowed_single_line_input(
            &self.query,
            self.query.len(),
            query_viewport,
        );
        let mut query_spans = vec![Span::styled("Query: ", theme.title(true))];
        if win.clipped_left {
            query_spans.push(Span::styled("‹", theme.muted_style()));
        }
        query_spans.push(Span::raw(win.before_cursor));
        query_spans.push(Span::styled("▏", theme.muted_style()));
        if !win.after_cursor.is_empty() {
            query_spans.push(Span::raw(win.after_cursor));
        }
        if win.clipped_right {
            query_spans.push(Span::styled("›", theme.muted_style()));
        }
        let query_line = Line::from(query_spans);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let len = app.search_results.len();
        let active_sel = if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        };
        let status_y = inner.y.saturating_add(1);
        let status = if app.searching {
            "Searching…".to_string()
        } else if app.search_query.is_empty() {
            "Type to search cached messages in real time".to_string()
        } else {
            format!(
                "'{}': {} hits · {}/{}",
                app.search_query,
                len,
                if len == 0 { 0 } else { active_sel + 1 },
                len
            )
        };
        frame.render_widget(
            Paragraph::new(Span::styled(status, theme.muted_style())),
            Rect::new(inner.x, status_y, inner.width, 1),
        );

        let list_y = inner.y.saturating_add(3);
        let visible = inner.height.saturating_sub(3) as usize;
        if visible == 0 {
            return;
        }
        let skip = active_sel
            .saturating_sub(visible.saturating_sub(1))
            .min(len.saturating_sub(visible));
        for row in 0..visible {
            let index = skip + row;
            if index >= len {
                break;
            }
            let y = list_y + row as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let selected = index == active_sel;
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let hit = &app.search_results[index];
            let thread_tag = if hit.parent_msg_id.is_some() {
                "[Thread] "
            } else {
                ""
            };
            let text = format!(
                "[{0}] {thread_tag}{1}: {2}",
                hit.conv_title,
                hit.author_name,
                hit.snippet.replace(['\n', '\r'], " ")
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    crate::ui::truncate_width(&text, inner.width as usize),
                    style,
                ))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_moves_within_results() {
        let mut search = SearchOverlay::default();
        search.move_down(3);
        search.move_down(3);
        assert_eq!(search.selected, 2);
        search.move_down(3);
        assert_eq!(search.selected, 2);
        search.move_up();
        assert_eq!(search.selected, 1);
        search.move_down(0);
        assert_eq!(search.selected, 0);
    }

    #[test]
    fn activate_jumps_even_when_query_has_trailing_space() {
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
        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);
        app.search_query = "deploy ".to_string();
        app.search_results = vec![crate::models::SearchHit {
            conv_key: "chat:c1".into(),
            conv_title: "General".into(),
            kind: crate::models::ConversationKind::OneOnOne,
            msg_id: "m1".into(),
            parent_msg_id: None,
            author_name: "Alice".into(),
            created_at: "2026-11-18T00:00:00Z".into(),
            snippet: "deploy now".into(),
        }];
        let mut search = SearchOverlay {
            query: "deploy ".to_string(),
            selected: 0,
        };
        assert_eq!(
            search.handle(&Action::Activate, &app),
            OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(0))
        );
    }

    #[test]
    fn long_query_windows_to_keep_cursor_and_tail_visible() {
        use ratatui::{backend::TestBackend, Terminal};

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
        let ui = UiState::new();
        let search = SearchOverlay {
            query: "한글검색어테스트긴쿼리문자열입니다가나다라마바사아자차카타파하_TAIL_TOKEN".into(),
            selected: 0,
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| search.draw(frame, frame.area(), &app, &ui, &mut hitmap))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("TAIL_TOKEN"), "tail text clipped:\n{text}");
        assert!(text.contains('▏'), "cursor clipped:\n{text}");
        assert!(text.contains('‹'), "left overflow indicator missing:\n{text}");
    }
}
