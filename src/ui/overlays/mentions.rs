//! `@mention` autocomplete popup for the Teams composer, populated from `app.members`.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::MemberInfo,
    ui::{
        hitmap::{HitMap, HitTarget},
        theme::Theme,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCandidate {
    pub user_id: String,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MentionPopup {
    pub active: bool,
    /// Byte index of `@` in the composer buffer.
    pub start: usize,
    /// Query substring after `@`.
    pub query: String,
    pub candidates: Vec<MentionCandidate>,
    pub selected: usize,
}

impl MentionPopup {
    pub fn is_open(&self) -> bool {
        self.active
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.candidates.clear();
        self.selected = 0;
    }

    pub fn next(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    pub fn prev(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
    }

    pub fn current(&self) -> Option<&MentionCandidate> {
        self.candidates.get(self.selected)
    }

    /// Refreshes the popup based on the current cursor position and room members.
    pub fn refresh(&mut self, text: &str, cursor: usize, members: &[MemberInfo]) {
        let Some((start, query)) = token_at(text, cursor) else {
            self.close();
            return;
        };

        let candidates = filter_candidates(members, &query);
        if candidates.is_empty() {
            self.close();
            return;
        }

        let changed = self.candidates != candidates || self.start != start || !self.active;
        self.active = true;
        self.start = start;
        self.query = query;
        self.candidates = candidates;
        if changed || self.selected >= self.candidates.len() {
            self.selected = 0;
        }
    }

    pub fn draw(&self, frame: &mut Frame, composer_area: Rect, theme: &Theme, hitmap: &mut HitMap) {
        if !self.active || self.candidates.is_empty() {
            return;
        }
        let max_rows = 6usize;
        let rows = self.candidates.len().min(max_rows);
        let height = rows as u16 + 2;

        let label_width = self
            .candidates
            .iter()
            .map(|c| {
                UnicodeWidthStr::width(c.display_name.as_str())
                    + c.email.as_deref().map_or(0, |e| e.len() + 3)
                    + 4
            })
            .max()
            .unwrap_or(16);
        let width = (label_width as u16 + 2).clamp(18, 52);

        let frame_area = frame.area();
        let y = composer_area.y.saturating_sub(height);
        let x = composer_area
            .x
            .min(frame_area.width.saturating_sub(width));
        let area = Rect::new(x, y, width.min(frame_area.width), height.min(frame_area.height));
        if area.width == 0 || area.height == 0 {
            return;
        }

        frame.render_widget(Clear, area);
        hitmap.add(area, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(" @Mention ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }

        let visible = (inner.height as usize).min(rows).min(self.candidates.len());
        if visible == 0 {
            return;
        }
        let active_sel = self.selected.min(self.candidates.len().saturating_sub(1));
        let skip = active_sel
            .saturating_sub(visible.saturating_sub(1))
            .min(self.candidates.len().saturating_sub(visible));

        let mut lines = Vec::with_capacity(visible);
        for slot in 0..visible {
            let index = skip + slot;
            let candidate = &self.candidates[index];
            let selected = index == active_sel;
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let marker = if selected { "▸ " } else { "  " };
            let text = match &candidate.email {
                Some(email) if !email.is_empty() => {
                    format!("{marker}{} ({email})", candidate.display_name)
                }
                _ => format!("{marker}{}", candidate.display_name),
            };
            lines.push(Line::from(Span::styled(
                truncate(&text, inner.width as usize),
                style,
            )));
        }
        let list_area = Rect::new(inner.x, inner.y, inner.width, visible as u16);
        frame.render_widget(Paragraph::new(lines), list_area);
        for slot in 0..visible {
            let index = skip + slot;
            hitmap.add_line(
                list_area.x,
                list_area.y + slot as u16,
                list_area.width,
                HitTarget::MentionCandidate(index),
            );
        }
    }
}

/// Detects an active `@query` token ending at `cursor`.
pub fn token_at(text: &str, cursor: usize) -> Option<(usize, String)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let prefix = &text[..cursor];
    let mut start = None;
    for (index, ch) in prefix.char_indices().rev() {
        if ch.is_whitespace() {
            break;
        }
        if ch == '@' {
            start = Some(index);
            break;
        }
    }
    let start = start?;
    let query = &text[start + 1..cursor];
    if query.contains('@') {
        return None;
    }
    Some((start, query.to_string()))
}

/// Filters `MemberInfo` by display name or email prefix/substring (case-insensitive, up to 8).
pub fn filter_candidates(members: &[MemberInfo], query: &str) -> Vec<MentionCandidate> {
    let needle = query.to_lowercase();
    let mut out: Vec<MentionCandidate> = members
        .iter()
        .filter(|m| !m.display_name.is_empty())
        .filter(|m| {
            needle.is_empty()
                || m.display_name.to_lowercase().contains(&needle)
                || m.email
                    .as_deref()
                    .is_some_and(|e| e.to_lowercase().contains(&needle))
        })
        .map(|m| MentionCandidate {
            user_id: m.user_id.clone(),
            display_name: m.display_name.clone(),
            email: m.email.clone(),
        })
        .collect();
    out.sort_by(|a, b| {
        let a_starts = !needle.is_empty() && a.display_name.to_lowercase().starts_with(&needle);
        let b_starts = !needle.is_empty() && b.display_name.to_lowercase().starts_with(&needle);
        b_starts
            .cmp(&a_starts)
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    out.truncate(8);
    out
}

fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_members() -> Vec<MemberInfo> {
        vec![
            MemberInfo {
                user_id: "u1".into(),
                member_id: None,
                display_name: "김철수".into(),
                email: Some("chulsoo@contoso.com".into()),
                roles: vec![],
                presence: None,
            },
            MemberInfo {
                user_id: "u2".into(),
                member_id: None,
                display_name: "이영희".into(),
                email: Some("younghee@contoso.com".into()),
                roles: vec![],
                presence: None,
            },
            MemberInfo {
                user_id: "u3".into(),
                member_id: None,
                display_name: "Alice Kim".into(),
                email: Some("alice@contoso.com".into()),
                roles: vec![],
                presence: None,
            },
        ]
    }

    #[test]
    fn token_recognized_after_at() {
        assert_eq!(token_at("hi @al", 6), Some((3, "al".to_string())));
        assert_eq!(token_at("@", 1), Some((0, "".to_string())));
        assert_eq!(token_at("no mention", 10), None);
        assert_eq!(token_at("a @b @c", 7), Some((5, "c".to_string())));
    }

    #[test]
    fn candidates_filter_by_name_or_email() {
        let members = sample_members();
        assert_eq!(filter_candidates(&members, "").len(), 3);
        let alice = filter_candidates(&members, "alice");
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].user_id, "u3");
        let korean = filter_candidates(&members, "김");
        assert_eq!(korean.len(), 1);
        assert_eq!(korean[0].user_id, "u1");
        assert!(filter_candidates(&members, "zzz").is_empty());
    }

    #[test]
    fn refresh_opens_and_navigates() {
        let mut popup = MentionPopup::default();
        popup.refresh("hello @", 7, &sample_members());
        assert!(popup.active);
        assert_eq!(popup.start, 6);
        assert_eq!(popup.candidates.len(), 3);

        popup.next();
        assert_eq!(popup.selected, 1);
        popup.prev();
        assert_eq!(popup.selected, 0);

        popup.refresh("hello world", 11, &sample_members());
        assert!(!popup.active);
    }

    #[test]
    fn mention_popup_windows_candidates_registers_overlay_body_and_maps_actual_indices() {
        use ratatui::{backend::TestBackend, Terminal};

        let members: Vec<MemberInfo> = (0..8)
            .map(|i| MemberInfo {
                user_id: format!("u{i}"),
                member_id: None,
                display_name: format!("User {i}"),
                email: None,
                roles: vec![],
                presence: None,
            })
            .collect();
        let mut popup = MentionPopup::default();
        popup.refresh("@", 1, &members);
        assert_eq!(popup.candidates.len(), 8);
        popup.selected = 7;

        let theme = Theme::default();
        let mut hitmap = HitMap::default();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let composer_area = Rect::new(2, 20, 60, 3);
        term.draw(|f| {
            popup.draw(f, composer_area, &theme, &mut hitmap);
        })
        .unwrap();

        // Border cell (y = 20 - 8 = 12, x = 2) must register OverlayBody so clicks don't fall through.
        assert_eq!(hitmap.hit(12, 2), Some(HitTarget::OverlayBody));
        // 6 visible rows: candidates 2..=7 mapped to y = 13..=18.
        assert_eq!(hitmap.hit(13, 4), Some(HitTarget::MentionCandidate(2)));
        assert_eq!(hitmap.hit(18, 4), Some(HitTarget::MentionCandidate(7)));
    }
}
