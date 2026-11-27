//! Right-click / `m` context menu.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget, MessageScope},
    overlays::OverlayOutcome,
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
    pub fn new(label: impl Into<String>, shortcut: Option<&str>, action: Action) -> Self {
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
    pub fn from_items(context: ContextTarget, anchor: (u16, u16), items: Vec<MenuItem>) -> Self {
        Self {
            context,
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

        let visible = inner.height as usize;
        if visible == 0 || self.items.is_empty() {
            return;
        }
        let active_sel = self.selected.min(self.items.len().saturating_sub(1));
        let skip = active_sel
            .saturating_sub(visible.saturating_sub(1))
            .min(self.items.len().saturating_sub(visible));
        for slot in 0..visible {
            let index = skip + slot;
            let Some(item) = self.items.get(index) else {
                break;
            };
            let row = inner.y + slot as u16;
            let selected = index == active_sel;
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
