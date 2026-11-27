//! Interactive HTML & Adaptive Card preview overlay (`H` / `[H: HTML]`).
//!
//! Backed by `bitefoundry_html::HtmlViewSession` (`0 MB` idle on static HTML, on-demand
//! sandboxed QuickJS-NG when `<script>` or `on*` handlers are present, dropped on close)
//! and `bitefoundry_html::NativeHtmlServer` (`o` to serve natively on localhost and open
//! in the default browser).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use bitefoundry_html::pulldown_cmark::{Event as MdEvent, Options as MdOptions, Parser as MdParser};
use bitefoundry_html::{ContentKind, HtmlEffect, HtmlTheme, HtmlViewSession, SourceKind};
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
    img_preview::{self, FitBox, RenderedTerminalImage, TerminalImageProtocol},
    models::{AttachmentInfo, MessageItem, RichBlock},
    parser::card_in,
    ui::{
        action::Action,
        app::{PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
        theme::Theme,
    },
};

struct CachedHtmlGraphic {
    source_bytes: Arc<[u8]>,
    cell_px: (u16, u16),
    rendered: RenderedTerminalImage,
}

pub struct HtmlPreviewOverlay {
    pub session: HtmlViewSession,
    pub last_content_area: Rect,
    pub last_served_url: Option<String>,
    ensure_focus_visible: bool,
    graphic_cache: HashMap<(usize, u16, TerminalImageProtocol), CachedHtmlGraphic>,
}

impl Drop for HtmlPreviewOverlay {
    fn drop(&mut self) {
        self.graphic_cache.clear();
    }
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
            .field("cached_graphics", &self.graphic_cache.len())
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
            graphic_cache: HashMap::new(),
        }
    }

    pub fn new_markdown(title: Option<String>, raw_md: String) -> Self {
        Self {
            session: HtmlViewSession::new_markdown(title, raw_md),
            last_content_area: Rect::default(),
            last_served_url: None,
            ensure_focus_visible: false,
            graphic_cache: HashMap::new(),
        }
    }

    pub fn from_content(title: Option<String>, kind: ContentKind, raw: String) -> Self {
        Self {
            session: HtmlViewSession::from_content(title, kind, raw),
            last_content_area: Rect::default(),
            last_served_url: None,
            ensure_focus_visible: false,
            graphic_cache: HashMap::new(),
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

    /// Returns `true` when an attachment's MIME type or filename classifies as Markdown.
    pub fn is_markdown_attachment(att: &AttachmentInfo) -> bool {
        let kind = bitefoundry_html::sniff_content_kind(
            Some(att.content_type.as_str()),
            att.name.as_deref(),
            None,
        );
        if kind == ContentKind::Markdown {
            return true;
        }
        matches!(kind, ContentKind::PlainText | ContentKind::Binary)
            && bitefoundry_html::sniff_content_kind(None, att.name.as_deref(), None)
                == ContentKind::Markdown
    }

    /// Returns `true` when an attachment can be rendered in `HtmlPreviewOverlay` (HTML or Markdown).
    pub fn is_previewable_markup_attachment(att: &AttachmentInfo) -> bool {
        Self::is_html_attachment(att) || Self::is_markdown_attachment(att)
    }

    /// Returns `true` when an attachment is an Adaptive Card or O365 Connector Card.
    pub fn is_card_attachment(att: &AttachmentInfo) -> bool {
        card_in::is_card_content_type(&att.content_type)
    }

    pub fn is_editing_input(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn title(&self) -> &str {
        self.session.title().unwrap_or(match self.session.source_kind() {
            SourceKind::Markdown => "Markdown Preview",
            SourceKind::Html => "HTML Preview",
        })
    }

    pub fn raw_html(&self) -> &str {
        self.session.raw_html()
    }

    /// Returns standalone HTML ready to serve in an external browser via `NativeHtmlServer`.
    pub fn serve_html(&self) -> String {
        self.session.serve_html()
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
            Action::Input('r') | Action::Input('m') | Action::ContextMenuCurrent => {
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

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ui: &mut UiState, hitmap: &mut HitMap) {
        let mut pending = std::mem::take(&mut ui.pending_pixel_graphics);
        self.draw_inner(frame, area, ui, Some(&mut pending), hitmap);
        ui.pending_pixel_graphics = pending;
    }

    pub(crate) fn draw_readonly(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        self.draw_inner(frame, area, ui, None, hitmap);
    }

    fn draw_inner(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ui: &UiState,
        mut pending_out: Option<&mut Vec<PendingPixelGraphic>>,
        hitmap: &mut HitMap,
    ) {
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

        let mode_badge = if self.session.raw_mode {
            "RAW"
        } else {
            match self.session.source_kind() {
                SourceKind::Markdown => "MD",
                SourceKind::Html => "HTML",
            }
        };
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
        let (mut lines, graphics, focused_line, total_lines) = {
            let rendered = self.session.render(content_w, &html_theme, ui.show_hrefs);
            let focused_line = focused_idx
                .and_then(|idx| rendered.controls.get(idx))
                .map(|c| c.line);
            (
                rendered.lines.clone(),
                rendered.graphics.clone(),
                focused_line,
                rendered.lines.len(),
            )
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

        let vis_top = self.session.scroll_y as usize;
        let vis_bot = vis_top.saturating_add(vh);
        let bg_rgb = img_preview::color_to_rgb(theme.bg_elevated, (18, 20, 26));
        let mut pixel_skip_rects: Vec<(String, Rect, TerminalImageProtocol)> = Vec::new();

        for g in &graphics {
            let g_start = g.line_start;
            let g_end = g.line_start.saturating_add(g.line_count);
            if g.line_count == 0 || g_end <= vis_top || g_start >= vis_bot {
                continue;
            }
            let cache_k = (g.node_id.0, content_w, ui.graphics_caps.protocol);
            let needs_decode = self.graphic_cache.get(&cache_k).is_none_or(|c| {
                c.cell_px != ui.graphics_caps.cell_px || !Arc::ptr_eq(&c.source_bytes, &g.image_bytes)
            });
            if needs_decode {
                let fit = FitBox::fixed(content_w, g.line_count.clamp(1, u16::MAX as usize) as u16);
                if let Ok(rendered_img) = img_preview::decode_and_render_for_protocol(
                    &g.image_bytes,
                    fit,
                    bg_rgb,
                    ui.graphics_caps,
                ) {
                    self.graphic_cache.insert(
                        cache_k,
                        CachedHtmlGraphic {
                            source_bytes: Arc::clone(&g.image_bytes),
                            cell_px: ui.graphics_caps.cell_px,
                            rendered: rendered_img,
                        },
                    );
                }
            }
            let Some(cached) = self.graphic_cache.get(&cache_k) else {
                continue;
            };
            let rendered_img = &cached.rendered;

            if !rendered_img.protocol.is_pixel() {
                for offset in 0..g.line_count {
                    let line_idx = g_start + offset;
                    if line_idx < lines.len() {
                        lines[line_idx] = rendered_img
                            .lines
                            .get(offset)
                            .cloned()
                            .unwrap_or_default();
                    }
                }
            } else {
                let slice_start = g_start.max(vis_top);
                let slice_end = g_end.min(vis_bot);
                let vis_rows = slice_end.saturating_sub(slice_start) as u16;
                let crop_y = slice_start.saturating_sub(g_start) as u16;
                let screen_y = content_area
                    .y
                    .saturating_add(slice_start.saturating_sub(vis_top) as u16);
                if let Some((payload, draw_cols, draw_rows)) =
                    rendered_img.viewport_pixel_payload(0, crop_y, content_w, vis_rows)
                {
                    let draw_w = draw_cols.min(content_w);
                    let draw_h = draw_rows.min(vis_rows);
                    if draw_w > 0 && draw_h > 0 {
                        let draw_rect = Rect::new(content_area.x, screen_y, draw_w, draw_h);
                        let pixel_key = format!(
                            "html:n{}:w{}:r{}:{}:c{},{}",
                            g.node_id.0,
                            content_w,
                            rendered_img.cols,
                            rendered_img.rows,
                            crop_y,
                            vis_rows
                        );
                        if let Some(out) = pending_out.as_deref_mut() {
                            out.push(PendingPixelGraphic {
                                cache_key: pixel_key.clone(),
                                rect: draw_rect,
                                protocol: rendered_img.protocol,
                                payload: Some(payload),
                            });
                        }
                        pixel_skip_rects.push((pixel_key, draw_rect, rendered_img.protocol));
                    }
                }
            }
        }

        frame.render_widget(
            Paragraph::new(lines).scroll((self.session.scroll_y, 0)),
            content_area,
        );

        if !pixel_skip_rects.is_empty() {
            let buf = frame.buffer_mut();
            for (key, draw_rect, proto) in pixel_skip_rects {
                if ui.active_pixel_graphics.contains(&(key, draw_rect, proto)) {
                    for y in draw_rect.top()..draw_rect.bottom() {
                        for x in draw_rect.left()..draw_rect.right() {
                            if let Some(cell) = buf.cell_mut((x, y)) {
                                cell.set_skip(true);
                            }
                        }
                    }
                }
            }
        }

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
            " Tab/⇧Tab: Focus · Enter/Space/Click: Activate · r/m: Raw/Rendered · o: Browser · j/k/Wheel: Scroll · Esc/q: Close "
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
    html.push_str(&format!("<h1>{}</h1>", card_markdown_to_html_inline(&card_title)));
    if let Some(sub) = subtitle {
        html.push_str(&format!(
            "<p><em>{}</em></p>",
            card_markdown_to_html_inline(&sub)
        ));
    }
    if !facts.is_empty() {
        html.push_str("<table><thead><tr><th>Field</th><th>Value</th></tr></thead><tbody>");
        for (k, v) in &facts {
            html.push_str(&format!(
                "<tr><td><strong>{}</strong></td><td>{}</td></tr>",
                card_markdown_to_html_inline(k),
                card_markdown_to_html_inline(v)
            ));
        }
        html.push_str("</tbody></table>");
    }
    if !body_lines.is_empty() {
        let joined_body = body_lines.join("\n");
        html.push_str(&card_markdown_to_html_blocks(&joined_body));
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

fn card_markdown_to_html_blocks(input: &str) -> String {
    let mut opts = MdOptions::empty();
    opts.insert(MdOptions::ENABLE_STRIKETHROUGH);
    opts.insert(MdOptions::ENABLE_TASKLISTS);
    opts.insert(MdOptions::ENABLE_TABLES);

    let events = MdParser::new_ext(input, opts).map(|ev| match ev {
        MdEvent::SoftBreak => MdEvent::HardBreak,
        MdEvent::Html(raw) | MdEvent::InlineHtml(raw) => MdEvent::Text(raw),
        other => other,
    });
    let mut out = String::new();
    bitefoundry_html::pulldown_cmark::html::push_html(&mut out, events);
    out
}

fn card_markdown_to_html_inline(input: &str) -> String {
    let rendered = card_markdown_to_html_blocks(input);
    let trimmed = rendered.trim();
    if let Some(inner) = trimmed.strip_prefix("<p>").and_then(|s| s.strip_suffix("</p>")) {
        if !inner.contains("</p>") {
            return inner.to_string();
        }
    }
    trimmed.to_string()
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

    #[cfg(feature = "html-js")]
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

        let mut ui = UiState::new();
        let area = Rect::new(0, 0, 100, 28);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw(frame, area, &mut ui, &mut hitmap);
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

    #[cfg(all(feature = "svg-graphics", feature = "html-js"))]
    #[test]
    fn inline_svg_renders_halfblocks_in_slot_and_preserves_button_click_coordinates() {
        let html = r##"<!DOCTYPE html>
<html>
<body>
  <p>Chart Report</p>
  <svg width="80" height="40"><rect width="80" height="40" fill="#e11d48"/></svg>
  <button id="act" onclick="alert('clicked-below-svg')">Approve</button>
</body>
</html>"##;

        let mut overlay = HtmlPreviewOverlay::new(Some("SVG Report".into()), html.into());
        let mut ui = UiState::new();
        ui.graphics_caps = img_preview::TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (8, 16),
            cell_px_exact: true,
        };

        let area = Rect::new(0, 0, 100, 32);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw(frame, area, &mut ui, &mut hitmap);
            })
            .unwrap();

        let content_area = overlay.last_content_area;
        assert!(content_area.width > 0 && content_area.height > 0);
        let html_theme = HtmlPreviewOverlay::html_theme(&ui.theme);
        let rendered = overlay
            .session
            .render(content_area.width, &html_theme, ui.show_hrefs);
        assert_eq!(rendered.graphics.len(), 1);
        let graphic = &rendered.graphics[0];
        assert!(graphic.line_count >= 1);

        // Reserved graphic slot must contain Halfblock '▀' glyphs in the rendered TestBackend buffer.
        let buf = terminal.backend().buffer();
        let mut halfblock_cells = 0usize;
        for doc_line in graphic.line_start..graphic.line_start + graphic.line_count {
            let screen_y = content_area.y + (doc_line as u16);
            if screen_y < content_area.bottom() {
                for x in content_area.left()..content_area.right() {
                    if buf[(x, screen_y)].symbol() == "▀" {
                        halfblock_cells += 1;
                    }
                }
            }
        }
        assert!(
            halfblock_cells > 0,
            "expected Halfblock '▀' glyphs in reserved SVG slot lines {}..{}",
            graphic.line_start,
            graphic.line_start + graphic.line_count
        );

        // Button located below the SVG must still hit accurately via click_at(line, col) & screen click.
        let btn = rendered
            .controls
            .iter()
            .find(|c| c.label.contains("Approve"))
            .expect("expected Approve button control")
            .clone();
        assert!(
            btn.line >= graphic.line_start + graphic.line_count,
            "button line {} must be below SVG slot end {}",
            btn.line,
            graphic.line_start + graphic.line_count
        );
        let effects = overlay.click_at(btn.line, btn.col_start + 1);
        assert!(
            effects.contains(&HtmlEffect::Toast("clicked-below-svg".to_string())),
            "expected button click toast, got {effects:?}"
        );

        // Also verify pixel protocol (Sixel) pushes PendingPixelGraphic and marks skip on active frame.
        ui.graphics_caps.protocol = TerminalImageProtocol::Sixel;
        ui.pending_pixel_graphics.clear();
        ui.active_pixel_graphics.clear();
        terminal
            .draw(|frame| {
                overlay.draw(frame, area, &mut ui, &mut hitmap);
            })
            .unwrap();
        assert_eq!(ui.pending_pixel_graphics.len(), 1);
        let pg = ui.pending_pixel_graphics[0].clone();
        assert_eq!(pg.protocol, TerminalImageProtocol::Sixel);
        assert!(pg.payload.as_deref().is_some_and(|p| p.starts_with("\x1bP")));

        ui.active_pixel_graphics = vec![(pg.cache_key.clone(), pg.rect, pg.protocol)];
        let mut skipped = 0usize;
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                overlay.draw(frame, area, &mut ui, &mut hitmap);
                let b = frame.buffer_mut();
                for y in pg.rect.top()..pg.rect.bottom() {
                    for x in pg.rect.left()..pg.rect.right() {
                        if b[(x, y)].skip {
                            skipped += 1;
                        }
                    }
                }
            })
            .unwrap();
        assert_eq!(skipped, (pg.rect.width as usize) * (pg.rect.height as usize));
    }

    #[tokio::test]
    async fn markdown_attachment_preview_icon_dispatch_raw_toggle_and_serve_html() {
        use crate::ui::hitmap::MessageScope;
        use crate::ui::overlays::attachments::{AttachmentRow, AttachmentsState};
        use crate::ui::overlays::Overlay;

        // 1. Both text/markdown and application/octet-stream fallback for README.md show [MD]
        let md_mime_att = AttachmentInfo {
            id: "md-1".into(),
            content_type: "text/markdown".into(),
            content_url: Some("https://contoso.sharepoint.com/README.md".into()),
            name: Some("README.md".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let md_octet_att = AttachmentInfo {
            id: "md-2".into(),
            content_type: "application/octet-stream".into(),
            content_url: Some("https://contoso.sharepoint.com/README.md".into()),
            name: Some("README.md".into()),
            content_json: None,
            thumbnail_url: None,
        };
        assert!(HtmlPreviewOverlay::is_markdown_attachment(&md_mime_att));
        assert!(HtmlPreviewOverlay::is_previewable_markup_attachment(&md_mime_att));
        assert!(HtmlPreviewOverlay::is_markdown_attachment(&md_octet_att));
        assert!(HtmlPreviewOverlay::is_previewable_markup_attachment(&md_octet_att));

        let row_mime = AttachmentRow {
            scope: MessageScope::Chat,
            msg_index: 0,
            attach_index: 0,
            name: "README.md".into(),
            content_type: "text/markdown".into(),
            author_name: "Alice".into(),
        };
        let row_octet = AttachmentRow {
            scope: MessageScope::Chat,
            msg_index: 0,
            attach_index: 1,
            name: "README.md".into(),
            content_type: "application/octet-stream".into(),
            author_name: "Alice".into(),
        };
        assert_eq!(row_mime.icon(), "[MD]");
        assert_eq!(row_octet.icon(), "[MD]");

        // 2. AttachmentsState dispatches PreviewAttachment on Enter and v for .md rows
        let (mut app, mut ui) = crate::ui::app::tests::make_test_app();
        app.active_conv_key = Some("chat:c1".into());
        app.messages.push(MessageItem {
            msg_key: "chat:c1|m-md".into(),
            conv_key: "chat:c1".into(),
            msg_id: "m-md".into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".into(),
            is_me: false,
            created_at: "2026-11-25T00:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: vec![md_mime_att, md_octet_att],
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        });

        let mut tray = AttachmentsState { selected: 1 };
        assert_eq!(
            tray.handle(&Action::Activate, &app),
            OverlayOutcome::CloseAndDispatch(Action::PreviewAttachment {
                scope: MessageScope::Chat,
                msg_index: 0,
                attach_index: 0,
            })
        );
        tray.selected = 2;
        assert_eq!(
            tray.handle(&Action::Input('v'), &app),
            OverlayOutcome::CloseAndDispatch(Action::PreviewAttachment {
                scope: MessageScope::Chat,
                msg_index: 0,
                attach_index: 1,
            })
        );
        assert_eq!(
            tray.handle(&Action::OpenImagePreview, &app),
            OverlayOutcome::CloseAndDispatch(Action::PreviewAttachment {
                scope: MessageScope::Chat,
                msg_index: 0,
                attach_index: 1,
            })
        );

        // 3. Loading sniffed Markdown bytes opens Overlay::HtmlPreview with SourceKind::Markdown
        let raw_md = "# Title\n\n**bold** | [link](https://example.com)";
        let sniffed_kind = bitefoundry_html::sniff_content_kind(
            Some("application/octet-stream"),
            Some("README.md"),
            Some(raw_md.as_bytes()),
        );
        assert_eq!(sniffed_kind, ContentKind::Markdown);

        app.html_fetch_seq = 1;
        app.handle_ui_msg(
            &mut ui,
            crate::ui::app::UiMsg::HtmlAttachmentLoaded {
                req_seq: 1,
                conv_key: Some("chat:c1".into()),
                title: "README.md".into(),
                html: raw_md.into(),
                kind: sniffed_kind,
            },
        );
        let Some(Overlay::HtmlPreview(overlay)) = ui.overlays.last_mut() else {
            panic!("expected Overlay::HtmlPreview to be opened for .md attachment");
        };
        assert_eq!(overlay.session.source_kind(), SourceKind::Markdown);
        assert!(!overlay.session.js_active);
        assert_eq!(overlay.title(), "README.md");

        let theme = HtmlPreviewOverlay::html_theme(&Theme::midnight_dark());
        let rendered = overlay.session.render(80, &theme, false);
        let text = rendered_text(&rendered.lines);
        assert!(text.contains("Title"), "rendered markdown missing title:\n{text}");
        assert!(text.contains("bold"), "rendered markdown missing bold text:\n{text}");
        assert!(text.contains("link"), "rendered markdown missing link text:\n{text}");
        assert!(!text.contains("**"), "raw ** leaked in rendered markdown:\n{text}");

        // 4. `r` toggles raw mode showing `# Title`
        assert_eq!(overlay.handle(&Action::Input('r')), OverlayOutcome::Handled);
        assert!(overlay.session.raw_mode);
        let raw_rendered = overlay.session.render(80, &theme, false);
        let raw_text = rendered_text(&raw_rendered.lines);
        assert!(
            raw_text.contains("# Title"),
            "raw mode must show `# Title`, got:\n{raw_text}"
        );

        // 5. `serve_html()` produces valid standalone HTML document
        let served = overlay.serve_html();
        assert!(
            served.contains("<!DOCTYPE html>") || served.contains("<html"),
            "serve_html() must return valid HTML document:\n{served}"
        );
        assert!(served.contains("<strong>bold</strong>"), "{served}");
        assert!(served.contains("https://example.com"), "{served}");
        assert!(!served.contains("**bold**"), "{served}");

        app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }
}
