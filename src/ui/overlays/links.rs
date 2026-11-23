//! Link Extractor modal overlay (`u` in Chat / Thread).
//! Extracts HTML `<a href>`, Markdown `[label](url)`, Adaptive Card actions, and bare `http(s)://` URLs
//! from the focused message or thread (`Enter` open in browser, `y` copy URL, `Esc` close).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use scraper::{Html, Selector};

use crate::{
    models::{MessageItem, RichBlock},
    parser,
    ui::{
        action::{is_openable_url, Action},
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct LinksOverlay {
    pub links: Vec<ExtractedLink>,
    pub selected: usize,
    pub source_label: String,
}

impl LinksOverlay {
    pub fn new(links: Vec<ExtractedLink>, source_label: impl Into<String>) -> Self {
        Self {
            links,
            selected: 0,
            source_label: source_label.into(),
        }
    }

    pub fn current(&self) -> Option<&ExtractedLink> {
        self.links.get(self.selected)
    }

    pub fn select(&mut self, index: usize) {
        if index < self.links.len() {
            self.selected = index;
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                if !self.links.is_empty() {
                    let len = self.links.len();
                    self.selected = (self.selected + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if !self.links.is_empty() {
                    self.selected = (self.selected + 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::Activate => match self.current() {
                Some(link) => OverlayOutcome::CloseAndDispatch(Action::OpenUrl(link.url.clone())),
                None => OverlayOutcome::Close,
            },
            Action::Input('y') | Action::CopyFocused | Action::CopySelectedMessage => {
                match self.current() {
                    Some(link) => OverlayOutcome::Dispatch(Action::CopyText(link.url.clone())),
                    None => OverlayOutcome::Handled,
                }
            }
            Action::Input(ch) if ch.is_ascii_digit() && *ch != '0' => {
                let idx = (*ch as u8 - b'1') as usize;
                if idx < self.links.len() {
                    self.selected = idx;
                }
                OverlayOutcome::Handled
            }
            Action::CloseOverlay
            | Action::OpenLinks
            | Action::JumpNextUnread
            | Action::Input('q')
            | Action::Input('u') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(86),
            area.height.saturating_sub(4).min(20),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = if self.source_label.is_empty() {
            format!(
                " Links ({}) · Enter Open · y Copy · Esc Close ",
                self.links.len()
            )
        } else {
            format!(
                " Links ({}) · {} · Enter Open · y Copy · Esc Close ",
                self.links.len(),
                self.source_label
            )
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if self.links.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(" No links found", theme.muted_style())),
                inner,
            );
            return;
        }

        let has_footer = inner.height >= 3;
        let list_h = if has_footer {
            inner.height.saturating_sub(2) as usize
        } else {
            inner.height as usize
        };
        let active_sel = self.selected.min(self.links.len().saturating_sub(1));
        let skip = active_sel.saturating_sub(list_h.saturating_sub(1));

        for (row, (idx, link)) in self
            .links
            .iter()
            .enumerate()
            .skip(skip)
            .take(list_h)
            .enumerate()
        {
            let y = inner.y + row as u16;
            let is_selected = idx == active_sel;
            let row_style = if is_selected {
                theme.selection()
            } else {
                Style::default()
            };
            let prefix = format!(" {}. ", idx + 1);
            let display = if !link.label.is_empty() && !link.label.eq_ignore_ascii_case(&link.url) {
                format!("{prefix}{} — {}", link.label, link.url)
            } else {
                format!("{prefix}{}", link.url)
            };
            let text = crate::ui::truncate_width(&display, inner.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(text, row_style))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }

        if has_footer {
            if let Some(cur) = self.links.get(active_sel) {
                let footer_y = inner.y + inner.height - 1;
                let preview = format!(" URL: {}", cur.url);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        crate::ui::truncate_width(&preview, inner.width as usize),
                        theme.accent_style().add_modifier(Modifier::UNDERLINED),
                    ))),
                    Rect::new(inner.x, footer_y, inner.width, 1),
                );
            }
        }
    }
}

/// Extracts deduplicated, openable URLs from HTML (`<a href>`), Markdown (`[label](url)`), and bare `http(s)://` text.
pub fn extract_links_from_text(html: &str, plain: &str) -> Vec<ExtractedLink> {
    let mut out: Vec<ExtractedLink> = Vec::new();

    if !html.trim().is_empty() {
        let doc = Html::parse_fragment(html);
        if let Ok(sel) = Selector::parse("a[href]") {
            for el in doc.select(&sel) {
                if let Some(href) = el.value().attr("href") {
                    let label = collapse_ws(&el.text().collect::<Vec<_>>().join(" "));
                    push_unique_link(&mut out, &label, href);
                }
            }
        }

        let html_text = doc.root_element().text().collect::<Vec<_>>().join(" ");
        extract_markdown_links(&html_text, &mut out);
        extract_bare_urls(&html_text, &mut out);
    }

    if !plain.trim().is_empty() {
        extract_markdown_links(plain, &mut out);
        extract_bare_urls(plain, &mut out);
    }

    out
}

/// Extracts links from a single `MessageItem` (HTML, plain text, Markdown, and Adaptive Card actions).
pub fn extract_links_from_message(msg: &MessageItem) -> Vec<ExtractedLink> {
    let mut out = extract_links_from_text(&msg.body_html, &msg.body_plain);
    let eff_att = msg.effective_attachments();
    let blocks = parser::parse_message_blocks_arc(&msg.body_html, &eff_att, &msg.mentions);
    for block in blocks.iter() {
        if let RichBlock::Card { actions, .. } = block {
            for (label, url) in actions {
                push_unique_link(&mut out, label, url);
            }
        }
    }
    out
}

/// Extracts deduplicated links across multiple messages (e.g. an open thread).
pub fn extract_links_from_messages(msgs: &[&MessageItem]) -> Vec<ExtractedLink> {
    let mut out: Vec<ExtractedLink> = Vec::new();
    for msg in msgs {
        for link in extract_links_from_message(msg) {
            push_unique_link(&mut out, &link.label, &link.url);
        }
    }
    out
}

fn push_unique_link(out: &mut Vec<ExtractedLink>, raw_label: &str, raw_url: &str) {
    let url = raw_url.trim();
    if !is_openable_url(url) {
        return;
    }
    let label = collapse_ws(raw_label);
    if let Some(existing) = out.iter_mut().find(|l| l.url == url) {
        if (existing.label.is_empty() || existing.label.eq_ignore_ascii_case(&existing.url))
            && !label.is_empty()
            && !label.eq_ignore_ascii_case(url)
        {
            existing.label = label;
        }
        return;
    }
    out.push(ExtractedLink {
        label,
        url: url.to_string(),
    });
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_markdown_links(text: &str, out: &mut Vec<ExtractedLink>) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let label_start = i + 1;
            let mut j = label_start;
            while j < bytes.len() && bytes[j] != b']' && bytes[j] != b'\n' {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b']' && bytes[j + 1] == b'(' {
                let label = &text[label_start..j];
                let url_start = j + 2;
                let mut k = url_start;
                let mut paren_depth = 0usize;
                while k < bytes.len() && bytes[k] != b'\n' {
                    match bytes[k] {
                        b'(' => paren_depth += 1,
                        b')' => {
                            if paren_depth == 0 {
                                break;
                            }
                            paren_depth -= 1;
                        }
                        _ => {}
                    }
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b')' {
                    let url = text[url_start..k].trim();
                    if is_openable_url(url) {
                        push_unique_link(out, label, url);
                        i = k + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
}

fn extract_bare_urls(text: &str, out: &mut Vec<ExtractedLink>) {
    let mut offset = 0usize;
    while offset < text.len() {
        let rest = &text[offset..];
        let next_https = rest.find("https://");
        let next_http = rest.find("http://");
        let Some(rel) = (match (next_https, next_http) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }) else {
            break;
        };

        let start = offset + rel;
        let tail = &text[start..];
        let end_rel = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`'))
            .unwrap_or(tail.len());
        let candidate = trim_trailing_url_punct(&tail[..end_rel]);
        if is_openable_url(candidate) {
            push_unique_link(out, "", candidate);
        }
        offset = start + end_rel.max(1);
    }
}

fn trim_trailing_url_punct(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().next_back() else {
            break;
        };
        match last {
            '.' | ',' | ';' | ':' | '!' | '?' | ']' | '}' => {
                s = &s[..s.len() - last.len_utf8()];
            }
            ')' => {
                let opens = s.chars().filter(|&c| c == '(').count();
                let closes = s.chars().filter(|&c| c == ')').count();
                if closes > opens {
                    s = &s[..s.len() - 1];
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_html_markdown_and_bare_urls_deduplicated() {
        let html = r#"<p>See <a href="https://example.com/docs">Docs</a> and <a href="javascript:alert(1)">bad</a>.</p>"#;
        let plain = "Check [Release Notes](https://example.com/release) and https://example.com/docs plus https://en.wikipedia.org/wiki/Rust_(programming_language).";

        let links = extract_links_from_text(html, plain);
        assert_eq!(links.len(), 3);
        assert_eq!(
            links[0],
            ExtractedLink {
                label: "Docs".to_string(),
                url: "https://example.com/docs".to_string(),
            }
        );
        assert_eq!(
            links[1],
            ExtractedLink {
                label: "Release Notes".to_string(),
                url: "https://example.com/release".to_string(),
            }
        );
        assert_eq!(
            links[2],
            ExtractedLink {
                label: String::new(),
                url: "https://en.wikipedia.org/wiki/Rust_(programming_language)".to_string(),
            }
        );
    }

    #[test]
    fn overlay_navigation_open_copy_and_close() {
        let links = vec![
            ExtractedLink {
                label: "One".into(),
                url: "https://one.example.com".into(),
            },
            ExtractedLink {
                label: "Two".into(),
                url: "https://two.example.com".into(),
            },
        ];
        let mut overlay = LinksOverlay::new(links, "Alice");
        assert_eq!(overlay.selected, 0);

        assert_eq!(overlay.handle(&Action::MoveDown), OverlayOutcome::Handled);
        assert_eq!(overlay.selected, 1);

        assert_eq!(
            overlay.handle(&Action::Input('y')),
            OverlayOutcome::Dispatch(Action::CopyText("https://two.example.com".into()))
        );

        assert_eq!(
            overlay.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenUrl("https://two.example.com".into()))
        );

        assert_eq!(overlay.handle(&Action::CloseOverlay), OverlayOutcome::Close);
        assert_eq!(overlay.handle(&Action::Input('q')), OverlayOutcome::Close);
        assert_eq!(overlay.handle(&Action::QuitConfirm), OverlayOutcome::NotHandled);
    }
}
