//! Presence overlay (`Ctrl+P`): view & set user's Microsoft Teams availability.

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

pub struct PresenceOption {
    pub availability: &'static str,
    pub activity: &'static str,
    pub label: &'static str,
    pub badge: &'static str,
}

pub const PRESENCE_OPTIONS: [PresenceOption; 6] = [
    PresenceOption {
        availability: "Available",
        activity: "Available",
        label: "Available",
        badge: "●",
    },
    PresenceOption {
        availability: "Busy",
        activity: "Busy",
        label: "Busy",
        badge: "●",
    },
    PresenceOption {
        availability: "DoNotDisturb",
        activity: "DoNotDisturb",
        label: "Do Not Disturb",
        badge: "⛔",
    },
    PresenceOption {
        availability: "BeRightBack",
        activity: "BeRightBack",
        label: "Be Right Back",
        badge: "●",
    },
    PresenceOption {
        availability: "Away",
        activity: "Away",
        label: "Away",
        badge: "●",
    },
    PresenceOption {
        availability: "Reset",
        activity: "Reset",
        label: "Reset to Automatic",
        badge: "○",
    },
];

#[derive(Debug, Clone, Default)]
pub struct PresenceOverlay {
    pub selected: usize,
}

impl PresenceOverlay {
    pub fn select(&mut self, index: usize) {
        if index < PRESENCE_OPTIONS.len() {
            self.selected = index;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = (self.selected + PRESENCE_OPTIONS.len() - 1) % PRESENCE_OPTIONS.len();
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1) % PRESENCE_OPTIONS.len();
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down();
                OverlayOutcome::Handled
            }
            Action::Input(ch @ '1'..='6') => {
                let idx = (*ch as usize) - ('1' as usize);
                self.selected = idx;
                let opt = &PRESENCE_OPTIONS[idx];
                OverlayOutcome::CloseAndDispatch(Action::SetPresence {
                    availability: opt.availability.to_string(),
                    activity: opt.activity.to_string(),
                })
            }
            Action::Activate => {
                let opt = &PRESENCE_OPTIONS[self.selected];
                OverlayOutcome::CloseAndDispatch(Action::SetPresence {
                    availability: opt.availability.to_string(),
                    activity: opt.activity.to_string(),
                })
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
        let rect = centered_rect(area, 52, 12);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Presence · Enter Apply · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }

        let current_text = match &app.my_presence {
            Some(p) => format!(" Current: {} ({})", p.availability, p.activity),
            None => " Current: Unknown".to_string(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(current_text, theme.muted_style())),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let start_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        let active_sel = self.selected.min(PRESENCE_OPTIONS.len().saturating_sub(1));
        let skip = active_sel
            .saturating_sub(visible.saturating_sub(1))
            .min(PRESENCE_OPTIONS.len().saturating_sub(visible));
        for slot in 0..visible {
            let index = skip + slot;
            let Some(opt) = PRESENCE_OPTIONS.get(index) else {
                break;
            };
            let y = start_y + slot as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let selected = index == active_sel;
            let (_, dot_color) = theme.presence_indicator(Some(opt.availability));
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(format!(" {}. ", index + 1), theme.muted_style()),
                Span::styled(format!("{} ", opt.badge), Style::default().fg(dot_color)),
                Span::styled(opt.label, style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
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
    fn number_shortcut_dispatches_presence() {
        let mut p = PresenceOverlay::default();
        assert!(matches!(
            p.handle(&Action::Input('2')),
            OverlayOutcome::CloseAndDispatch(Action::SetPresence { availability, .. }) if availability == "Busy"
        ));
    }
}
