//! Local file path + caption input overlay (`Ctrl+O` or `[Send File]`).

use std::path::{Path, PathBuf};

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

pub fn expand_path_with_home(input: &str, home: &Path) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

pub fn expand_path(input: &str) -> PathBuf {
    expand_path_with_home(input, &home_dir())
}

fn home_dir() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Clone, Default)]
pub struct PathInputState {
    pub path: String,
    pub caption: String,
    pub field: usize,
    pub error: Option<String>,
}

impl PathInputState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_field(&mut self, field: usize) {
        self.field = field % 2;
        self.error = None;
    }

    pub fn next_field(&mut self) {
        self.set_field(self.field + 1);
    }

    pub fn prev_field(&mut self) {
        self.set_field(self.field + 1);
    }

    fn active_mut(&mut self) -> &mut String {
        if self.field == 0 {
            &mut self.path
        } else {
            &mut self.caption
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.active_mut().push(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.active_mut().push_str(text);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.active_mut().pop();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Tab => {
                self.next_field();
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::PrevFocus => {
                self.prev_field();
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let path = self.path.trim().to_string();
                if path.is_empty() {
                    self.field = 0;
                    self.error = Some("Enter a file path".to_string());
                    return OverlayOutcome::Handled;
                }
                let expanded = expand_path(&path);
                if !expanded.exists() {
                    self.field = 0;
                    self.error = Some(format!("File not found: {}", expanded.display()));
                    return OverlayOutcome::Handled;
                }
                if !expanded.is_file() {
                    self.field = 0;
                    self.error = Some(format!("Not a file: {}", expanded.display()));
                    return OverlayOutcome::Handled;
                }
                let caption = self.caption.trim().to_string();
                OverlayOutcome::CloseAndDispatch(Action::SendAttachmentPath {
                    path: expanded.to_string_lossy().into_owned(),
                    caption,
                })
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(78),
            area.height.saturating_sub(4).min(11),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Send File · Tab Field · Enter Send · Esc Cancel ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let path_style = if self.field == 0 {
            theme.accent_style()
        } else {
            theme.muted_style()
        };
        let caption_style = if self.field == 1 {
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
                " Local file path (~ expands to home directory)",
                theme.muted_style(),
            )),
            render_input_line(&self.path, path_style),
            Line::from(""),
            Line::from(Span::styled(
                " Message / caption (optional · Tab to switch)",
                theme.muted_style(),
            )),
            render_input_line(&self.caption, caption_style),
        ];
        match &self.error {
            Some(error) => lines.push(Line::from(Span::styled(
                format!(" Error: {error}"),
                Style::default().fg(theme.error),
            ))),
            None => lines.push(Line::from(Span::styled(
                " Uploads to OneDrive/SharePoint and sends to active chat/channel/thread",
                theme.muted_style(),
            ))),
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_leading_tilde() {
        let home = PathBuf::from("/home/user");
        assert_eq!(expand_path_with_home("~", &home), home);
        assert_eq!(expand_path_with_home("~/a/b.png", &home), home.join("a/b.png"));
        assert_eq!(expand_path_with_home(r"~\a\b.png", &home), home.join(r"a\b.png"));
        assert_eq!(expand_path_with_home(" x/~/y ", &home), PathBuf::from("x/~/y"));
    }

    #[test]
    fn empty_or_missing_path_shows_error() {
        let mut state = PathInputState::new();
        assert!(matches!(state.handle(&Action::Activate), OverlayOutcome::Handled));
        assert!(state.error.is_some());
    }
}
