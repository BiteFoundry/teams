//! Quick reaction picker (`+` or `e` on selected message).

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
    hitmap::{HitMap, HitTarget, MessageScope},
    overlays::{centered_rect, OverlayOutcome},
};

pub struct ReactionOption {
    pub reaction_type: &'static str,
    pub emoji: &'static str,
    pub label: &'static str,
}

pub const REACTION_OPTIONS: [ReactionOption; 6] = [
    ReactionOption {
        reaction_type: "like",
        emoji: "👍",
        label: "Like",
    },
    ReactionOption {
        reaction_type: "heart",
        emoji: "❤️",
        label: "Heart",
    },
    ReactionOption {
        reaction_type: "laugh",
        emoji: "😆",
        label: "Laugh",
    },
    ReactionOption {
        reaction_type: "surprised",
        emoji: "😮",
        label: "Surprised",
    },
    ReactionOption {
        reaction_type: "sad",
        emoji: "😢",
        label: "Sad",
    },
    ReactionOption {
        reaction_type: "angry",
        emoji: "😡",
        label: "Angry",
    },
];

#[derive(Debug, Clone)]
pub struct ReactionsOverlay {
    pub scope: MessageScope,
    pub msg_index: usize,
    pub selected: usize,
}

impl ReactionsOverlay {
    pub fn new(scope: MessageScope, msg_index: usize) -> Self {
        Self {
            scope,
            msg_index,
            selected: 0,
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < REACTION_OPTIONS.len() {
            self.selected = index;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = (self.selected + REACTION_OPTIONS.len() - 1) % REACTION_OPTIONS.len();
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1) % REACTION_OPTIONS.len();
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
                OverlayOutcome::CloseAndDispatch(Action::ToggleReaction {
                    scope: self.scope,
                    msg_index: self.msg_index,
                    reaction_type: REACTION_OPTIONS[idx].reaction_type.to_string(),
                })
            }
            Action::Activate => OverlayOutcome::CloseAndDispatch(Action::ToggleReaction {
                scope: self.scope,
                msg_index: self.msg_index,
                reaction_type: REACTION_OPTIONS[self.selected].reaction_type.to_string(),
            }),
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
        let rect = centered_rect(area, 42, 11);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " React (1-6) · Enter Toggle ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }

        let target_msg = app.message_at(self.scope, self.msg_index);
        let visible = inner.height as usize;
        let active_sel = self.selected.min(REACTION_OPTIONS.len().saturating_sub(1));
        let skip = active_sel
            .saturating_sub(visible.saturating_sub(1))
            .min(REACTION_OPTIONS.len().saturating_sub(visible));
        for slot in 0..visible {
            let index = skip + slot;
            let Some(opt) = REACTION_OPTIONS.get(index) else {
                break;
            };
            let y = inner.y + slot as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let selected = index == active_sel;
            let reacted = target_msg.is_some_and(|m| {
                m.reactions
                    .iter()
                    .any(|r| r.reaction_type == opt.reaction_type && r.me_reacted)
            });
            let mark = if reacted { " ✓" } else { "" };
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(format!(" {}. ", index + 1), theme.muted_style()),
                Span::raw(format!("{} ", opt.emoji)),
                Span::styled(format!("{}{mark}", opt.label), style),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_reaction_dispatches_toggle() {
        let mut r = ReactionsOverlay::new(MessageScope::Chat, 2);
        r.move_down();
        assert!(matches!(
            r.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::ToggleReaction {
                scope: MessageScope::Chat,
                msg_index: 2,
                reaction_type
            }) if reaction_type == "heart"
        ));
    }
}
