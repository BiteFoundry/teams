//! Interactive HTML & Adaptive Card preview overlay (`H` / `[H: HTML]`).
//!
//! Backed by `bitefoundry_html::HtmlViewSession` (`0 MB` idle on static HTML, on-demand
//! sandboxed QuickJS-NG when `<script>` or `on*` handlers are present, dropped on close)
//! and `bitefoundry_html::NativeHtmlServer` (`o` to serve natively on localhost and open
//! in the default browser).

use std::fmt;

use bitefoundry_html::{ContentKind, HtmlEffect, HtmlTheme, HtmlViewSession};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::{AttachmentInfo, MessageItem, RichBlock},
    parser::card_in,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
        theme::Theme,
    },
};

pub struct HtmlPreviewOverlay {
    pub session: HtmlViewSession,
    pub last_content_area: Rect,
    pub last_served_url: Option<String>,
    ensure_focus_visible: bool,
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("title", &self.session.title())
            .field("raw_mode", &self.session.raw_mode)
            .field("js_active", &self.session.js_active)
            .field("scroll_y", &self.session.scroll_y)
            .field("focused_control", &self.session.focused_control)
            .field("editing_input", &self.session.editing_input)
            .field("last_served_url", &self.last_served_url)
            .finish()
    }
}

impl HtmlPreviewOverlay {
    pub fn new(title: Option<String>, raw_html: String) -> Self {
        Self {
            session: HtmlViewSession::new(title, raw_html),
            last_content_area: Rect::default(),
            last_served_url: None,
            ensure_focus_visible: false,
        }
    }

    /// Builds an interactive `HtmlPreviewOverlay` from a Teams chat/channel message,
    /// combining its HTML body and any Adaptive / O365 Connector Card attachments.
    pub fn from_teams_message(msg: &MessageItem) -> Option<Self> {
        let (title, html) = build_message_html(msg)?;
        Some(Self::new(Some(title), html))
    }

    /// Returns `true` when an attachment's MIME type or filename classifies as HTML.
    pub fn is_html_attachment(att: &AttachmentInfo) -> bool {
        bitefoundry_html::sniff_content_kind(
            Some(att.content_type.as_str()),
            att.name.as_deref(),
            None,
        ) == ContentKind::Html
    }

    /// Returns `true` when an attachment is an Adaptive Card or O365 Connector Card.
    pub fn is_card_attachment(att: &AttachmentInfo) -> bool {
        card_in::is_card_content_type(&att.content_type)
    }

    pub fn is_editing_input(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn title(&self) -> &str {
        self.session.title().unwrap_or("HTML Preview")
    }

    pub fn raw_html(&self) -> &str {
        self.session.raw_html()
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn html_theme(theme: &Theme) -> HtmlTheme {
        if theme.no_color {
            return HtmlTheme::no_color();
        }
        let (focus_fg, focus_bg) = if theme.ansi16_only {
            (Color::Reset, Color::Reset)
        } else {
            (
                if theme.bg_elevated == Color::Reset {
                    Color::Black
                } else {
                    theme.bg_elevated
                },
                theme.accent,
            )
        };
        HtmlTheme {
            text: theme.fg_primary,
            muted: theme.fg_muted,
            heading: theme.accent_soft,
            accent: theme.accent,
            link: theme.accent,
            code_fg: theme.code_fg,
            code_bg: theme.bg_code,
            quote_border: theme.reply,
            border: theme.border,
            control_fg: theme.fg_primary,
            control_bg: theme.bg_selected,
            focus_fg,
            focus_bg,
            raw_tag: theme.accent,
            raw_attr: theme.code_fg,
        }
    }

    /// Delegates a document-relative `(line_index, col)` click directly to `HtmlViewSession::click_at`.
    pub fn click_at(&mut self, line_index: usize, col: u16) -> Vec<HtmlEffect> {
        self.session.click_at(line_index, col)
    }

    /// Handles a terminal screen click `(column, row)` inside the modal content viewport.
    pub fn handle_screen_click(&mut self, column: u16, row: u16) -> OverlayOutcome {
        let area = self.last_content_area;
        if area.width == 0 || area.height == 0 {
            return OverlayOutcome::Handled;
        }
        let inside = column >= area.x
            && column < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height);
        if !inside {
            return OverlayOutcome::Handled;
        }
        let line_index = (self.session.scroll_y as usize) + (row.saturating_sub(area.y) as usize);
        let col = column.saturating_sub(area.x);
        let effects = self.session.click_at(line_index, col);
        self.effects_to_outcome(effects)
    }

    pub fn toggle_raw_mode(&mut self) -> bool {
        let raw = self.session.toggle_raw_mode();
        self.session.scroll_y = 0;
        raw
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        if self.session.editing_input.is_some() {
            return match action {
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                Action::CloseOverlay | Action::Activate => {
                    let effects = self.session.finish_editing();
                    self.effects_to_outcome(effects)
                }
                Action::Backspace => {
                    self.session.backspace();
                    OverlayOutcome::Handled
                }
                Action::Input(ch) => {
                    self.session.input_char(*ch);
                    OverlayOutcome::Handled
                }
                Action::Paste(text) => {
                    for ch in text.chars() {
                        if !ch.is_control() {
                            self.session.input_char(ch);
                        }
                    }
                    OverlayOutcome::Handled
                }
                Action::Tab => {
                    let _ = self.session.finish_editing();
                    self.session.focus_next_control();
                    self.ensure_focus_visible = true;
                    OverlayOutcome::Handled
                }
                Action::PrevFocus => {
                    let _ = self.session.finish_editing();
                    self.session.focus_prev_control();
                    self.ensure_focus_visible = true;
                    OverlayOutcome::Handled
                }
                Action::MouseScroll { delta, .. } => {
                    self.apply_scroll(*delta);
                    OverlayOutcome::Handled
                }
                _ => OverlayOutcome::Handled,
            };
        }

        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit
            | Action::QuitConfirm
            | Action::ServeHtmlPreview
            | Action::OpenUrl(_)
            | Action::ShowToast(_) => OverlayOutcome::NotHandled,
            Action::Tab => {
                self.session.focus_next_control();
                self.ensure_focus_visible = true;
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.session.focus_prev_control();
                self.ensure_focus_visible = true;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Input(' ') => {
                let effects = self.session.activate_focused();
                self.effects_to_outcome(effects)
            }
            Action::Input('m') | Action::ContextMenuCurrent => {
                self.toggle_raw_mode();
                OverlayOutcome::Handled
            }
            Action::Input('o') | Action::LoadOlder => {
                OverlayOutcome::Dispatch(Action::ServeHtmlPreview)
            }
            Action::MoveUp | Action::ScrollUp | Action::Input('k') => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::ScrollDown | Action::Input('j') => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::ScrollPageUp => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::ScrollPageDown => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::NavFirst | Action::CursorHome | Action::Input('g') => {
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::NavLast | Action::CursorEnd | Action::Input('G') => {
                self.session.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.apply_scroll(*delta);
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    fn apply_scroll(&mut self, delta: i32) {
        if delta < 0 {
            self.session.scroll_y = self.session.scroll_y.saturating_sub((-delta) as u16);
        } else if delta > 0 {
            self.session.scroll_y = self.session.scroll_y.saturating_add(delta as u16);
        }
    }

    fn effects_to_outcome(&mut self, effects: Vec<HtmlEffect>) -> OverlayOutcome {
        let mut open_url: Option<String> = None;
        let mut toast: Option<String> = None;
        for effect in effects {
            match effect {
                HtmlEffect::None | HtmlEffect::DomChanged => {}
                HtmlEffect::OpenUrl(url) => {
                    if open_url.is_none() {
                        open_url = Some(url);
                    }
                }
                HtmlEffect::Toast(msg) => {
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let pairs = fields
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let summary = if pairs.is_empty() {
                        format!("Form submitted -> {action}")
                    } else {
                        format!("Form submitted -> {action} ({pairs})")
                    };
                    self.session.last_status = Some(summary.clone());
                    toast = Some(summary);
                }
            }
        }
        if let Some(url) = open_url {
            OverlayOutcome::Dispatch(Action::OpenUrl(url))
        } else if let Some(msg) = toast {
            OverlayOutcome::Dispatch(Action::ShowToast(msg))
        } else {
            OverlayOutcome::Handled
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let raw_pill = if self.session.raw_mode {
            "[m: Rendered]"
        } else {
            "[m: Raw]"
        };
        let open_pill = "[o: Browser]";
        let close_pill = "[✕ Esc]";

        let pills: [(&str, ButtonId); 3] = [
            (raw_pill, ButtonId::HtmlToggleRaw),
            (open_pill, ButtonId::HtmlOpenBrowser),
            (close_pill, ButtonId::ConfirmCancel),
        ];

        let mut visible_pills: Vec<(&str, ButtonId, u16)> = Vec::new();
        let mut pills_w = 0u16;
        for &(label, id) in pills.iter().rev() {
            let w = label.width() as u16;
            let sep = if visible_pills.is_empty() { 0 } else { 1 };
            if rect.width > pills_w + w + sep + 16 {
                visible_pills.push((label, id, w));
                pills_w += w + sep;
            }
        }
        visible_pills.reverse();

        let title_budget = rect
            .width
            .saturating_sub(2 + if pills_w > 0 { pills_w + 2 } else { 0 })
            as usize;

        let mode_badge = if self.session.raw_mode { "RAW" } else { "HTML" };
        let js_badge = if self.session.js_active { " · JS" } else { "" };
        let title = format!(" 🌐 {} [{}{} ] ", self.title(), mode_badge, js_badge);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if !visible_pills.is_empty() {
            let mut x = rect.right().saturating_sub(pills_w + 1);
            for (idx, (label, id, w)) in visible_pills.into_iter().enumerate() {
                if idx > 0 {
                    x = x.saturating_add(1);
                }
                let pill_style = if id == ButtonId::ConfirmCancel {
                    Style::default().fg(theme.fg).bg(theme.bg_elevated)
                } else {
                    Style::default()
                        .fg(theme.accent)
                        .bg(theme.bg_elevated)
                        .add_modifier(Modifier::BOLD)
                };
                frame.render_widget(
                    Paragraph::new(Span::styled(label, pill_style)),
                    Rect::new(x, rect.y, w, 1),
                );
                hitmap.add_line(x, rect.y, w, HitTarget::Button(id));
                x = x.saturating_add(w);
            }
        }

        if inner.width == 0 || inner.height < 2 {
            self.last_content_area = Rect::default();
            return;
        }

        let status_text = self
            .session
            .last_status
            .clone()
            .or_else(|| self.last_served_url.as_ref().map(|u| format!("Serving: {u}")));
        let has_status = status_text.is_some() && inner.height >= 3;
        let reserved_bottom = if has_status { 2u16 } else { 1u16 };
        let viewport_h = inner.height.saturating_sub(reserved_bottom);
        let content_w = if inner.width > 14 {
            inner.width.saturating_sub(1)
        } else {
            inner.width
        };

        let html_theme = Self::html_theme(theme);
        let focused_idx = self.session.focused_control;
        let (lines, focused_line, total_lines) = {
            let rendered = self.session.render(content_w, &html_theme, ui.show_hrefs);
            let focused_line = focused_idx
                .and_then(|idx| rendered.controls.get(idx))
                .map(|c| c.line);
            (rendered.lines.clone(), focused_line, rendered.lines.len())
        };

        let vh = viewport_h as usize;
        if self.ensure_focus_visible {
            if let Some(fline) = focused_line {
                let sy = self.session.scroll_y as usize;
                if fline < sy {
                    self.session.scroll_y = fline.min(u16::MAX as usize) as u16;
                } else if vh > 0 && fline >= sy.saturating_add(vh) {
                    let target = fline.saturating_sub(vh.saturating_sub(1));
                    self.session.scroll_y = target.min(u16::MAX as usize) as u16;
                }
            }
            self.ensure_focus_visible = false;
        }

        let max_scroll_y = total_lines.saturating_sub(vh).min(u16::MAX as usize) as u16;
        self.session.scroll_y = self.session.scroll_y.min(max_scroll_y);

        let content_area = Rect::new(inner.x, inner.y, content_w, viewport_h);
        self.last_content_area = content_area;

        frame.render_widget(
            Paragraph::new(lines).scroll((self.session.scroll_y, 0)),
            content_area,
        );

        let show_scrollbar = total_lines > vh && inner.width > content_w;
        if show_scrollbar {
            let sb_area = Rect::new(
                inner.right().saturating_sub(1),
                inner.y,
                1,
                viewport_h,
            );
            let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                .position(self.session.scroll_y as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        if let Some(status) = status_text.filter(|_| has_status) {
            let status_area = Rect::new(
                inner.x,
                inner.y.saturating_add(viewport_h),
                inner.width,
                1,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&format!(" ℹ {status}"), status_area.width as usize),
                    Style::default().fg(theme.accent_soft),
                )),
                status_area,
            );
        }

        let footer_area = Rect::new(
            inner.x,
            inner.y.saturating_add(inner.height.saturating_sub(1)),
            inner.width,
            1,
        );
        let footer = if self.session.editing_input.is_some() {
            " Editing input · Type text · Backspace: Delete · Enter/Esc: Finish editing · Tab: Next control "
        } else {
            " Tab/⇧Tab: Focus · Enter/Space/Click: Activate · m: Raw/Rendered · o: Browser · j/k/Wheel: Scroll · Esc/q: Close "
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(footer, footer_area.width as usize),
                theme.muted_style(),
            )),
            footer_area,
        );
    }
}

/// Converts an Adaptive Card or O365 Connector Card attachment into an interactive HTML fragment.
pub fn adaptive_card_to_html(att: &AttachmentInfo) -> Option<(String, String)> {
    let block = card_in::parse_card_attachment(att)?;
    let RichBlock::Card {
        title,
        subtitle,
        facts,
        body_lines,
        actions,
    } = block
    else {
        return None;
    };

    let card_title = title
        .clone()
        .or_else(|| att.name.clone())
        .unwrap_or_else(|| "Adaptive Card".to_string());

    let mut html = String::new();
    html.push_str("<section class=\"adaptive-card\">");
    html.push_str(&format!("<h1>{}</h1>", escape_html(&card_title)));
    if let Some(sub) = subtitle {
        html.push_str(&format!("<p><em>{}</em></p>", escape_html(&sub)));
    }
    if !facts.is_empty() {
        html.push_str("<table><thead><tr><th>Field</th><th>Value</th></tr></thead><tbody>");
        for (k, v) in &facts {
            html.push_str(&format!(
                "<tr><td><strong>{}</strong></td><td>{}</td></tr>",
                escape_html(k),
                escape_html(v)
            ));
        }
        html.push_str("</tbody></table>");
    }
    for line in &body_lines {
        html.push_str(&format!("<p>{}</p>", escape_html(line)));
    }
    if !actions.is_empty() {
        html.push_str("<div class=\"card-actions\">");
        for (label, target) in &actions {
            let trimmed = target.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                html.push_str(&format!(
                    "<a href=\"{}\">{}</a> ",
                    escape_html_attr(trimmed),
                    escape_html(label)
                ));
            } else {
                let js_msg = serde_json::to_string(&format!("{label}: {trimmed}"))
                    .unwrap_or_else(|_| "\"Action triggered\"".to_string());
                html.push_str(&format!(
                    "<button onclick=\"alert({})\">{}</button> ",
                    escape_html_attr(&js_msg),
                    escape_html(label)
                ));
            }
        }
        html.push_str("</div>");
    }
    if let Some(raw_json) = att.content_json.as_deref() {
        let pretty = serde_json::from_str::<serde_json::Value>(raw_json)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| raw_json.to_string());
        html.push_str(&format!(
            "<details><summary>Adaptive Card JSON</summary><pre><code>{}</code></pre></details>",
            escape_html(&pretty)
        ));
    }
    html.push_str("</section>");

    Some((card_title, html))
}

/// Builds `(title, html_document)` for a Teams message (HTML body + any Adaptive Cards).
pub fn build_message_html(msg: &MessageItem) -> Option<(String, String)> {
    let mut card_sections: Vec<(String, String)> = Vec::new();
    for att in &msg.attachments {
        if card_in::is_card_content_type(&att.content_type) {
            if let Some(card) = adaptive_card_to_html(att) {
                card_sections.push(card);
            }
        }
    }

    let raw_body = msg.body_html.trim();
    let body_is_attachment_placeholder = raw_body.is_empty()
        || (raw_body.starts_with("<attachment")
            && raw_body.ends_with("</attachment>")
            && !raw_body.contains("<p")
            && !raw_body.contains("<div"));

    let mut sections = Vec::new();
    if !body_is_attachment_placeholder {
        sections.push(raw_body.to_string());
    } else if card_sections.is_empty() && !msg.body_plain.trim().is_empty() {
        let escaped = escape_html(msg.body_plain.trim()).replace('\n', "<br>");
        sections.push(format!("<p>{escaped}</p>"));
    }

    for (_, card_html) in &card_sections {
        sections.push(card_html.clone());
    }

    if sections.is_empty() {
        return None;
    }

    let title = msg
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| card_sections.first().map(|(t, _)| t.clone()))
        .unwrap_or_else(|| format!("{} — Teams Message", msg.author_name));

    let combined = if sections.len() == 1 {
        sections.remove(0)
    } else {
        sections.join("\n<hr>\n")
    };

    Some((title, combined))
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_html_attr(input: &str) -> String {
    escape_html(input)
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn rendered_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn button_onclick_mutates_dom_via_keyboard_and_click_at_and_raw_toggle() {
        let html = r#"<!DOCTYPE html>
<html>
<head><title>Interactive Counter</title></head>
<body>
  <h1 id="heading">Clicks: 0</h1>
  <button id="inc" onclick="window.__c=(window.__c||0)+1; document.getElementById('heading').textContent='Clicks: '+window.__c;">Increment</button>
  <details><summary>More Info</summary><p>Secret Panel</p></details>
</body>
</html>"#;

        let mut overlay = HtmlPreviewOverlay::new(None, html.to_string());
        assert!(overlay.session.js_active);
        assert_eq!(overlay.title(), "Interactive Counter");

        let theme = HtmlPreviewOverlay::html_theme(&Theme::midnight_dark());
        let r0 = overlay.session.render(80, &theme, false);
        assert!(rendered_text(&r0.lines).contains("Clicks: 0"));
        assert!(!rendered_text(&r0.lines).contains("Secret Panel"));
        let btn = r0.controls[0].clone();
        let details = r0.controls[1].clone();

        // 1. Activate button via mouse click_at(line, col)
        let click_effs = overlay.click_at(btn.line, btn.col_start + 1);
        assert!(click_effs.contains(&HtmlEffect::DomChanged));
        let r1 = overlay.session.render(80, &theme, false);
        assert!(rendered_text(&r1.lines).contains("Clicks: 1"));

        // 2. Focus and activate via Tab + Space / Enter
        assert_eq!(overlay.handle(&Action::Input(' ')), OverlayOutcome::Handled);
        let r2 = overlay.session.render(80, &theme, false);
        assert!(rendered_text(&r2.lines).contains("Clicks: 2"));

        // 3. Tab to <details> and open with Enter
        assert_eq!(overlay.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(overlay.handle(&Action::Activate), OverlayOutcome::Handled);
        let r3 = overlay.session.render(80, &theme, false);
        assert!(rendered_text(&r3.lines).contains("Secret Panel"));

        // Click details summary again via click_at to collapse
        let _ = overlay.click_at(details.line, details.col_start + 1);
        let r4 = overlay.session.render(80, &theme, false);
        assert!(!rendered_text(&r4.lines).contains("Secret Panel"));

        // 4. Toggle Raw HTML via `m` (both Input('m') and ContextMenuCurrent keymap action)
        assert_eq!(overlay.handle(&Action::Input('m')), OverlayOutcome::Handled);
        assert!(overlay.session.raw_mode);
        let raw_r = overlay.session.render(80, &theme, false);
        assert!(rendered_text(&raw_r.lines).contains("<button id=\"inc\""));

        assert_eq!(
            overlay.handle(&Action::ContextMenuCurrent),
            OverlayOutcome::Handled
        );
        assert!(!overlay.session.raw_mode);

        // 5. `o` (both Input('o') and LoadOlder keymap action) dispatches ServeHtmlPreview
        assert_eq!(
            overlay.handle(&Action::Input('o')),
            OverlayOutcome::Dispatch(Action::ServeHtmlPreview)
        );
        assert_eq!(
            overlay.handle(&Action::LoadOlder),
            OverlayOutcome::Dispatch(Action::ServeHtmlPreview)
        );

        // 6. Scroll & close
        assert_eq!(overlay.handle(&Action::Input('j')), OverlayOutcome::Handled);
        assert_eq!(overlay.session.scroll_y, 2);
        assert_eq!(overlay.handle(&Action::Input('k')), OverlayOutcome::Handled);
        assert_eq!(overlay.session.scroll_y, 0);
        assert_eq!(overlay.handle(&Action::Input('q')), OverlayOutcome::Close);
        assert_eq!(overlay.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[test]
    fn static_html_keeps_js_inactive_and_supports_text_input_editing_and_screen_click() {
        let html = r#"<p>Search:</p><input type="text" name="q" value="ab" /><a href="https://example.com/docs">Docs</a>"#;
        let mut overlay = HtmlPreviewOverlay::new(Some("Static Doc".into()), html.into());
        assert!(!overlay.session.js_active);

        let ui = UiState::new();
        let area = Rect::new(0, 0, 100, 28);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw(frame, area, &ui, &mut hitmap);
            })
            .unwrap();

        assert!(overlay.last_content_area.width > 0);
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::HtmlToggleRaw)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::HtmlOpenBrowser)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ConfirmCancel)).is_some());

        // Focus text input with Tab, activate to edit, type chars, finish with Enter
        overlay.handle(&Action::Tab);
        overlay.handle(&Action::Activate);
        assert!(overlay.is_editing_input());
        overlay.handle(&Action::Input('c'));
        overlay.handle(&Action::Backspace);
        overlay.handle(&Action::Input('z'));
        // Esc exits input editing rather than closing the modal
        assert_eq!(overlay.handle(&Action::CloseOverlay), OverlayOutcome::Handled);
        assert!(!overlay.is_editing_input());

        // Click the link via screen coordinates
        let theme = HtmlPreviewOverlay::html_theme(&ui.theme);
        let link_ctrl = overlay
            .session
            .render(overlay.last_content_area.width, &theme, false)
            .controls[1]
            .clone();
        let screen_x = overlay.last_content_area.x + link_ctrl.col_start + 1;
        let screen_y = overlay.last_content_area.y + (link_ctrl.line as u16);
        assert_eq!(
            overlay.handle_screen_click(screen_x, screen_y),
            OverlayOutcome::Dispatch(Action::OpenUrl("https://example.com/docs".to_string()))
        );
    }

    #[test]
    fn adaptive_card_message_builds_interactive_html() {
        let card_json = r#"{
            "type": "AdaptiveCard",
            "body": [
                { "type": "TextBlock", "text": "Deploy Approval", "size": "Large", "weight": "Bolder" },
                { "type": "FactSet", "facts": [{ "title": "Env", "value": "Prod" }] }
            ],
            "actions": [
                { "type": "Action.OpenUrl", "title": "View Pipeline", "url": "https://ci.example.com/123" }
            ]
        }"#;
        let msg = MessageItem {
            msg_key: "chat:c1|m-card".into(),
            conv_key: "chat:c1".into(),
            msg_id: "m-card".into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "DeployBot".into(),
            is_me: false,
            created_at: "2026-11-20T00:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: "<attachment id=\"card-1\"></attachment>".into(),
            body_plain: String::new(),
            attachments: vec![AttachmentInfo {
                id: "card-1".into(),
                content_type: card_in::ADAPTIVE_CARD_CONTENT_TYPE.into(),
                content_url: None,
                name: None,
                content_json: Some(card_json.into()),
                thumbnail_url: None,
            }],
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        };

        let mut overlay = HtmlPreviewOverlay::from_teams_message(&msg).unwrap();
        assert_eq!(overlay.title(), "Deploy Approval");
        assert!(overlay.raw_html().contains("https://ci.example.com/123"));
        assert!(overlay.raw_html().contains("Prod"));

        let theme = HtmlPreviewOverlay::html_theme(&Theme::midnight_dark());
        let rendered = overlay.session.render(80, &theme, false);
        let text = rendered_text(&rendered.lines);
        assert!(text.contains("Deploy Approval"));
        assert!(text.contains("Prod"));
        assert!(text.contains("View Pipeline"));
    }
}
