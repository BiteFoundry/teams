//! Center main conversation view & shared rich message renderer.
//!
//! Pure line builders (`ChatRender`, `build_lines_with_hyperlinks`, `render_message_item`)
//! are decoupled from `GraphClient` / `Storage` so they can be unit tested directly.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    config::DensityMode,
    img_preview::{self, RenderedTerminalImage},
    models::{
        AttachmentInfo, ConversationKind, ConversationSummary, MentionInfo, MessageItem,
        PresenceStatus, RichBlock, RichSpan,
    },
    parser,
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget, InlineActionKind, MessageScope},
        theme::Theme,
    },
};

/// One rendered visual line plus optional full-line and inline click targets.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct RenderedLine {
    pub line: Line<'static>,
    pub target: Option<HitTarget>,
    /// `(col_offset, width, target)` registered after `target` so inline chips win hit-tests.
    pub inline_targets: Vec<(u16, u16, HitTarget)>,
}

impl RenderedLine {
    pub fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            target: None,
            inline_targets: Vec::new(),
        }
    }

    pub fn clickable(line: Line<'static>, target: HitTarget) -> Self {
        Self {
            line,
            target: Some(target),
            inline_targets: Vec::new(),
        }
    }

    pub fn with_inline(
        line: Line<'static>,
        target: Option<HitTarget>,
        inline_targets: Vec<(u16, u16, HitTarget)>,
    ) -> Self {
        Self {
            line,
            target,
            inline_targets,
        }
    }
}

/// Pure view struct for testing and rendering without requiring `AppState`.
pub struct ChatRender<'a> {
    pub messages: &'a [MessageItem],
    pub is_channel: bool,
    pub selected_index: Option<usize>,
}

impl<'a> ChatRender<'a> {
    pub fn from_app(app: &'a AppState, ui: &'a UiState) -> Self {
        let is_channel = app
            .active_conversation()
            .is_some_and(|c| c.kind == ConversationKind::Channel)
            || app.active_channel().is_some();
        Self {
            messages: &app.messages,
            is_channel,
            selected_index: ui.chat_scroll.selected_idx,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CachedStream {
    pub lines: Arc<[RenderedLine]>,
    pub ranges: Arc<[(usize, usize)]>,
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if ui.theme.bg_canvas != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(ui.theme.bg_canvas)),
            area,
        );
    }

    draw_header(frame, area, app, ui, hitmap);

    let message_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.chat_scroll.area = message_area;

    if message_area.height == 0 {
        return;
    }

    if app.active_conv_key.is_none() || app.messages.is_empty() {
        CHAT_LINES_CACHE.take();
    }

    if app.active_conv_key.is_none() {
        ui.chat_scroll.message_ranges = Arc::from([]);
        ui.chat_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new("Select a conversation from the sidebar (or press Ctrl+K)")
                .style(ui.theme.muted_style())
                .wrap(Wrap { trim: false }),
            message_area,
        );
        return;
    }

    if app.loading_messages && app.messages.is_empty() {
        ui.chat_scroll.message_ranges = Arc::from([]);
        ui.chat_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new("Loading messages…")
                .style(ui.theme.muted_style())
                .wrap(Wrap { trim: false }),
            message_area,
        );
        return;
    }

    if app.messages.is_empty() {
        ui.chat_scroll.message_ranges = Arc::from([]);
        let mut empty_lines = Vec::new();
        if let Some(banner) = build_history_banner_line(
            app.loading_older,
            app.oldest_reached,
            app.history_error.as_deref(),
            false,
            &ui.theme,
            message_area.width as usize,
        ) {
            empty_lines.push(banner);
        }
        let empty_wrapped = bitefoundry_html::layout::wrap_styled_spans(
            &[Span::styled(
                "No messages yet — write the first message below.",
                ui.theme.muted_style(),
            )],
            (message_area.width as usize).max(12),
            bitefoundry_html::layout::WrapPrefix::empty(),
        );
        for line in empty_wrapped {
            empty_lines.push(RenderedLine::plain(line));
        }
        let height = message_area.height as usize;
        ui.chat_scroll.total_lines = empty_lines.len();
        ui.chat_scroll.offset = 0;
        ui.pending_prepend_anchor = None;
        ui.pending_append_anchor = None;
        render_scrolled_lines(frame, message_area, &empty_lines, 0, height, hitmap);
        return;
    }

    let cached = build_stream_cached(app, ui, message_area.width as usize);
    ui.chat_scroll.message_ranges = Arc::clone(&cached.ranges);
    if cached.lines.is_empty() {
        ui.chat_scroll.total_lines = 0;
        return;
    }

    let height = message_area.height as usize;
    apply_prepend_anchor(ui, cached.lines.len(), height);
    apply_append_anchor(ui, cached.lines.len(), height);
    ui.chat_scroll.total_lines = cached.lines.len();
    ui.chat_scroll.clamp_to_viewport(height);
    if std::mem::take(&mut ui.pending_reveal_selection) {
        if let Some(idx) = ui.chat_scroll.selected_idx {
            ui.chat_scroll.ensure_visible(idx, height);
        }
    }
    let offset = ui.chat_scroll.offset(height);
    ui.chat_scroll.offset = offset;

    if ui.inline_images && !cached.inline_images.is_empty() {
        let width = (message_area.width as usize).max(12);
        let thumb_cols = width.saturating_sub(4).clamp(24, 72) as u16;
        let thumb_rows = 16u16;
        let cell_px = ui.graphics_caps.cell_px;
        let inline_bg = AppState::inline_image_bg(&ui.theme);
        let vis_start = offset.saturating_sub(20);
        let vis_end = offset.saturating_add(height).saturating_add(20);
        for (msg_idx, msg_id, att) in cached.inline_images.iter() {
            if let Some(&(r_start, r_end)) = cached.ranges.get(*msg_idx) {
                if r_end > vis_start && r_start < vis_end {
                    app.request_teams_image(
                        msg_id.clone(),
                        att.clone(),
                        thumb_cols,
                        thumb_rows,
                        cell_px,
                        inline_bg,
                    );
                }
            }
        }
    }

    let is_channel = app
        .active_conversation()
        .is_some_and(|c| c.kind == ConversationKind::Channel)
        || app.active_channel().is_some();
    let selection = ui.chat_scroll.selected_idx.and_then(|sel_idx| {
        let range = *cached.ranges.get(sel_idx)?;
        let msg = app.messages.get(sel_idx)?;
        Some(VisibleSelection {
            range,
            scope: MessageScope::Chat,
            msg_index: sel_idx,
            show_thread_action: is_channel && msg.parent_msg_id.is_none(),
            theme: &ui.theme,
        })
    });

    render_scrolled_lines_with_selection(
        frame,
        message_area,
        &cached.lines,
        offset,
        height,
        selection,
        hitmap,
    );
}

pub(crate) fn apply_prepend_anchor(ui: &mut UiState, new_total_lines: usize, height: usize) {
    if let Some((old_total_lines, old_offset)) = ui.pending_prepend_anchor.take() {
        if !ui.chat_scroll.follow_tail && old_total_lines > 0 && new_total_lines > old_total_lines
        {
            let delta = new_total_lines - old_total_lines;
            let new_max_off = new_total_lines.saturating_sub(height);
            let anchored_offset = old_offset.saturating_add(delta).min(new_max_off);
            ui.chat_scroll.scroll_up = new_max_off
                .saturating_sub(anchored_offset)
                .min(u16::MAX as usize) as u16;
        }
    }
}

pub(crate) fn apply_append_anchor(ui: &mut UiState, new_total_lines: usize, height: usize) {
    if let Some(old_offset) = ui.pending_append_anchor.take() {
        if !ui.chat_scroll.follow_tail
            && ui.chat_scroll.scroll_up > 0
            && new_total_lines > height
        {
            let new_max_off = new_total_lines.saturating_sub(height);
            let anchored_offset = old_offset.min(new_max_off);
            ui.chat_scroll.scroll_up = new_max_off
                .saturating_sub(anchored_offset)
                .min(u16::MAX as usize) as u16;
        }
    }
}

pub(crate) fn build_history_banner_line(
    loading_older: bool,
    oldest_reached: bool,
    history_error: Option<&str>,
    has_messages: bool,
    theme: &Theme,
    width: usize,
) -> Option<RenderedLine> {
    let width = width.max(12);
    if loading_older {
        return Some(RenderedLine::plain(Line::from(Span::styled(
            truncate("  ⟳ Loading older messages...", width),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ))));
    }
    if let Some(err) = history_error {
        let text = format!("  ⚠ {err}  [o / click to retry]");
        return Some(RenderedLine::clickable(
            Line::from(Span::styled(
                truncate(&text, width),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )),
            HitTarget::LoadOlderMessages,
        ));
    }
    if oldest_reached {
        if has_messages {
            let label = "── Beginning of conversation ──";
            let label_w = UnicodeWidthStr::width(label);
            let left_pad = width.saturating_sub(label_w) / 2;
            let centered = format!("{}{}", " ".repeat(left_pad), label);
            return Some(RenderedLine::plain(Line::from(Span::styled(
                truncate(&centered, width),
                theme.muted_style(),
            ))));
        }
        return None;
    }
    let pill = "  [ ↑ Scroll up or press o / H to load older messages ]";
    Some(RenderedLine::clickable(
        Line::from(Span::styled(truncate(pill, width), theme.muted_style())),
        HitTarget::LoadOlderMessages,
    ))
}

pub(crate) struct VisibleSelection<'a> {
    pub range: (usize, usize),
    pub scope: MessageScope,
    pub msg_index: usize,
    pub show_thread_action: bool,
    pub theme: &'a Theme,
}

pub fn render_scrolled_lines(
    frame: &mut Frame,
    area: Rect,
    lines: &[RenderedLine],
    offset: usize,
    height: usize,
    hitmap: &mut HitMap,
) {
    render_scrolled_lines_with_selection(frame, area, lines, offset, height, None, hitmap);
}

pub(crate) fn render_scrolled_lines_with_selection(
    frame: &mut Frame,
    area: Rect,
    lines: &[RenderedLine],
    offset: usize,
    height: usize,
    selection: Option<VisibleSelection<'_>>,
    hitmap: &mut HitMap,
) {
    let end = offset.saturating_add(height).min(lines.len());
    let slice = lines.get(offset..end).unwrap_or(&[]);
    let width = area.width as usize;

    let mut visible: Vec<Line> = Vec::with_capacity(slice.len());
    for (row, rendered) in slice.iter().enumerate() {
        let global_idx = offset + row;
        let y = area.y + row as u16;
        if let Some(sel) = selection
            .as_ref()
            .filter(|sel| global_idx >= sel.range.0 && global_idx < sel.range.1)
        {
            let mut decorated = rendered.clone();
            apply_selection_to_line(
                &mut decorated,
                global_idx == sel.range.0,
                sel.scope,
                sel.msg_index,
                width,
                sel.theme,
                sel.show_thread_action,
            );
            visible.push(decorated.line);
            if let Some(target) = decorated.target {
                hitmap.add_line(area.x, y, area.width, target);
            }
            for &(col_off, w, target) in &decorated.inline_targets {
                let x = area.x.saturating_add(col_off);
                if x < area.x + area.width {
                    let clamped = w.min((area.x + area.width).saturating_sub(x));
                    hitmap.add_line(x, y, clamped, target);
                }
            }
        } else {
            visible.push(rendered.line.clone());
            if let Some(target) = rendered.target {
                hitmap.add_line(area.x, y, area.width, target);
            }
            for &(col_off, w, target) in &rendered.inline_targets {
                let x = area.x.saturating_add(col_off);
                if x < area.x + area.width {
                    let clamped = w.min((area.x + area.width).saturating_sub(x));
                    hitmap.add_line(x, y, clamped, target);
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(visible), area);
}

pub fn pane_title(follow_tail: bool) -> &'static str {
    if follow_tail {
        " Chat "
    } else {
        " Chat · New ↓ "
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let header_rect = Rect::new(area.x, area.y, area.width, 1);
    let header_bg = if theme.no_color {
        Color::Reset
    } else {
        theme.bg_header
    };
    let header_style = Style::default().bg(header_bg);

    let buttons: [(&str, ButtonId); 8] = [
        ("[Search]", ButtonId::Search),
        ("[File]", ButtonId::SendFile),
        ("[Attach]", ButtonId::Attachments),
        ("[Img]", ButtonId::ImagePreview),
        ("[H: HTML]", ButtonId::HtmlPreview),
        ("[Info]", ButtonId::RoomInfo),
        ("[Refresh]", ButtonId::Refresh),
        ("[Settings]", ButtonId::Settings),
    ];

    let min_title_w = 16u16;
    let mut visible_buttons = Vec::new();
    let mut used_btn_w = 0u16;
    for &(label, id) in &buttons {
        let w = UnicodeWidthStr::width(label) as u16 + 1;
        if area.width >= min_title_w + used_btn_w + w {
            visible_buttons.push((label, id, w));
            used_btn_w += w;
        }
    }

    let (prefix_kind, title_text, presence_opt) = header_parts(app.active_conversation(), app, theme);
    let title_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(if ui.focus == Focus::Chat {
                theme.accent
            } else {
                theme.fg_primary
            })
            .bg(header_bg)
            .add_modifier(Modifier::BOLD)
    };
    let muted_hdr = if theme.no_color {
        theme.muted_style()
    } else {
        Style::default().fg(theme.fg_muted).bg(header_bg)
    };

    let mut spans = vec![Span::styled(" ", header_style)];
    let mut prefix_w = 1u16;

    if let Some((dot, color)) = presence_opt {
        let badge = format!("{dot} ");
        let bw = UnicodeWidthStr::width(badge.as_str()) as u16;
        prefix_w += bw;
        spans.push(Span::styled(
            badge,
            if theme.no_color {
                Style::default()
            } else {
                Style::default().fg(color).bg(header_bg)
            },
        ));
    } else if let Some(kind_prefix) = prefix_kind {
        let kw = UnicodeWidthStr::width(kind_prefix) as u16;
        prefix_w += kw;
        spans.push(Span::styled(
            kind_prefix,
            if theme.no_color {
                Style::default()
            } else {
                Style::default().fg(theme.accent).bg(header_bg)
            },
        ));
    }

    let member_label = if app.active_conv_key.is_some() && !app.members.is_empty() {
        Some(format!(" · {} members", app.members.len()))
    } else {
        None
    };

    let title_budget = area
        .width
        .saturating_sub(used_btn_w + prefix_w + 1)
        .max(1) as usize;
    let truncated_title = truncate(&title_text, title_budget);
    prefix_w += UnicodeWidthStr::width(truncated_title.as_str()) as u16;
    spans.push(Span::styled(truncated_title, title_style));

    if let Some(m_label) = member_label {
        let m_w = UnicodeWidthStr::width(m_label.as_str()) as u16;
        if prefix_w + m_w + used_btn_w < area.width {
            prefix_w += m_w;
            spans.push(Span::styled(m_label, muted_hdr));
        }
    }

    if !visible_buttons.is_empty() {
        let pad = area
            .width
            .saturating_sub(prefix_w)
            .saturating_sub(used_btn_w);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad as usize), header_style));
        }
        let mut x = area.x + prefix_w + pad;
        for (label, id, w) in visible_buttons {
            spans.push(Span::styled(
                label.to_string(),
                if theme.no_color {
                    Style::default()
                } else {
                    Style::default().fg(theme.accent).bg(header_bg)
                },
            ));
            spans.push(Span::styled(" ", header_style));
            hitmap.add_line(x, area.y, w.saturating_sub(1), HitTarget::Button(id));
            x += w;
        }
    } else if prefix_w < area.width {
        let pad = area.width.saturating_sub(prefix_w);
        spans.push(Span::styled(" ".repeat(pad as usize), header_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(header_style), header_rect);
}

fn header_parts<'a>(
    conv: Option<&'a ConversationSummary>,
    app: &'a AppState,
    theme: &Theme,
) -> (
    Option<&'static str>,
    String,
    Option<(&'static str, Color)>,
) {
    if let Some(conv) = conv {
        match conv.kind {
            ConversationKind::Channel => {
                let title = match conv.team_name.as_deref().filter(|s| !s.is_empty()) {
                    Some(team) => format!("{team} › {}", conv.title),
                    None => conv.title.clone(),
                };
                (Some("# "), title, None)
            }
            ConversationKind::Group => (Some("[Group] "), conv.title.clone(), None),
            ConversationKind::Meeting => (Some("[Meeting] "), conv.title.clone(), None),
            ConversationKind::OneOnOne => {
                let avail = conv
                    .peer_user_id
                    .as_deref()
                    .and_then(|id| app.presences.get(id))
                    .map(|p| p.availability.as_str());
                (None, conv.title.clone(), Some(theme.presence_indicator(avail)))
            }
        }
    } else if let Some((team, ch)) = app.active_channel() {
        (
            Some("# "),
            format!("{} › {}", team.display_name, ch.display_name),
            None,
        )
    } else {
        (None, "Select a conversation".to_string(), None)
    }
}

pub(crate) struct DualHasher {
    h1: DefaultHasher,
    h2: DefaultHasher,
}

impl DualHasher {
    pub(crate) fn new(domain: u64) -> Self {
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        domain.hash(&mut h1);
        (domain ^ 0x9e37_79b9_7f4a_7c15).hash(&mut h2);
        Self { h1, h2 }
    }

    pub(crate) fn write_hash<T: Hash>(&mut self, val: &T) {
        val.hash(&mut self.h1);
        val.hash(&mut self.h2);
    }

    pub(crate) fn write_str(&mut self, s: &str) {
        s.len().hash(&mut self.h1);
        s.as_bytes().hash(&mut self.h1);
        s.len().hash(&mut self.h2);
        s.as_bytes().hash(&mut self.h2);
    }

    pub(crate) fn finish(&self) -> (u64, u64) {
        (self.h1.finish(), self.h2.finish())
    }
}

pub(crate) struct LineSliceCache {
    entries: Vec<((u64, u64), CachedStream)>,
    max_entries: usize,
}

impl LineSliceCache {
    pub(crate) fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries: max_entries.max(1),
        }
    }

    pub(crate) fn get(&mut self, key: (u64, u64)) -> Option<CachedStream> {
        let pos = self.entries.iter().position(|(k, _)| *k == key)?;
        let (_, val) = self.entries.remove(pos);
        self.entries.push((key, val.clone()));
        Some(val)
    }

    pub(crate) fn insert(&mut self, key: (u64, u64), val: CachedStream) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push((key, val));
    }
}

pub(crate) fn hash_theme_sig(h: &mut DualHasher, theme: &Theme) {
    h.write_hash(&theme.no_color);
    h.write_hash(&theme.ansi16_only);
    h.write_hash(&theme.fg_primary);
    h.write_hash(&theme.fg_secondary);
    h.write_hash(&theme.fg_muted);
    h.write_hash(&theme.fg_subtle);
    h.write_hash(&theme.bg_selected);
    h.write_hash(&theme.bg_code);
    h.write_hash(&theme.bg_card);
    h.write_hash(&theme.bg_elevated);
    h.write_hash(&theme.bg_mention_me);
    h.write_hash(&theme.fg_mention_me);
    h.write_hash(&theme.bg_reaction_me);
    h.write_hash(&theme.accent);
    h.write_hash(&theme.accent_soft);
    h.write_hash(&theme.own);
    h.write_hash(&theme.date_separator);
    h.write_hash(&theme.unread);
    h.write_hash(&theme.unread_divider);
    h.write_hash(&theme.warning);
    h.write_hash(&theme.error);
    h.write_hash(&theme.success);
    h.write_hash(&theme.code_fg);
    h.write_hash(&theme.author_palette);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChatLinesSignature {
    prefix: (u64, u64),
    full: (u64, u64),
}

#[allow(clippy::too_many_arguments)]
fn compute_chat_lines_sig(
    conv_key: Option<&str>,
    view: &ChatRender,
    messages_gen: u64,
    messages_prefix_gen: u64,
    presences_gen: u64,
    density: DensityMode,
    unread_anchor_ts: Option<&str>,
    my_user_id: &str,
    my_display_name: Option<&str>,
    width: usize,
    hyperlinks: bool,
    show_hrefs: bool,
    inline_images: bool,
    image_cache_gen: u64,
    cell_px: (u16, u16),
    loading_older: bool,
    oldest_reached: bool,
    history_error: Option<&str>,
    theme: &Theme,
) -> ChatLinesSignature {
    let mut h = DualHasher::new(0x4348_4154_5f4c_494e);
    match conv_key {
        Some(k) => {
            h.write_hash(&1u8);
            h.write_str(k);
        }
        None => h.write_hash(&0u8),
    }
    h.write_hash(&view.is_channel);
    h.write_hash(&density);
    match unread_anchor_ts {
        Some(ts) => {
            h.write_hash(&1u8);
            h.write_str(ts);
        }
        None => h.write_hash(&0u8),
    }
    h.write_str(my_user_id);
    if let Some(dn) = my_display_name {
        h.write_hash(&1u8);
        h.write_str(dn);
    } else {
        h.write_hash(&0u8);
    }
    h.write_hash(&Local::now().date_naive());
    h.write_hash(&width);
    h.write_hash(&hyperlinks);
    h.write_hash(&show_hrefs);
    h.write_hash(&inline_images);
    h.write_hash(&image_cache_gen);
    h.write_hash(&cell_px);
    h.write_hash(&loading_older);
    h.write_hash(&oldest_reached);
    match history_error {
        Some(err) => {
            h.write_hash(&1u8);
            h.write_str(err);
        }
        None => h.write_hash(&0u8),
    }
    h.write_hash(&presences_gen);
    hash_theme_sig(&mut h, theme);
    h.write_hash(&view.messages.len());
    h.write_hash(&messages_prefix_gen);
    let Some(tail) = view.messages.last() else {
        let empty = h.finish();
        return ChatLinesSignature {
            prefix: empty,
            full: empty,
        };
    };
    // These tail fields fix its position and contribution to the unread divider.
    h.write_str(&tail.msg_id);
    h.write_str(&tail.created_at);
    h.write_hash(&tail.is_me);
    let prefix = h.finish();
    h.write_hash(&messages_gen);
    ChatLinesSignature {
        prefix,
        full: h.finish(),
    }
}

#[derive(Clone)]
struct ChatStream {
    signature: ChatLinesSignature,
    lines: Arc<Vec<RenderedLine>>,
    ranges: Arc<[(usize, usize)]>,
    inline_images: Arc<[(usize, String, AttachmentInfo)]>,
}

fn collect_inline_image_attachments(
    messages: &[MessageItem],
    start_index: usize,
    out: &mut Vec<(usize, String, AttachmentInfo)>,
) {
    for (msg_idx, msg) in messages.iter().enumerate().skip(start_index) {
        if msg.deleted {
            continue;
        }
        let eff = msg.effective_attachments();
        for att in eff.iter() {
            if img_preview::is_image_mime_or_name(&att.content_type, att.name.as_deref()) {
                out.push((msg_idx, msg.msg_id.clone(), att.clone()));
            }
        }
    }
}

thread_local! {
    static CHAT_LINES_CACHE: RefCell<Option<ChatStream>> = const { RefCell::new(None) };
    #[cfg(test)]
    static RENDERED_MESSAGE_BODIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn build_lines(app: &AppState, ui: &UiState, width: usize) -> Vec<RenderedLine> {
    build_lines_with_hyperlinks(
        &ChatRender::from_app(app, ui),
        &ui.theme,
        width,
        hyperlinks_enabled(),
    )
}

pub fn build_lines_with_images(
    app: &mut AppState,
    ui: &UiState,
    width: usize,
) -> Vec<RenderedLine> {
    build_stream_cached(app, ui, width).lines.to_vec()
}

fn build_stream_cached(
    app: &mut AppState,
    ui: &UiState,
    width: usize,
) -> ChatStream {
    let tail_message_updated = std::mem::take(&mut app.tail_message_updated);
    let hyperlinks = hyperlinks_enabled();
    let width = width.max(12);
    let mut view = ChatRender::from_app(app, ui);
    view.selected_index = None;
    let active_unread_ts = match (&ui.unread_anchor, app.active_conv_key.as_deref()) {
        (Some((conv, ts)), Some(active)) if conv == active => Some(ts.as_str()),
        _ => None,
    };

    let my_display_name = app.my_profile.as_ref().map(|p| p.display_name.as_str());
    let signature = compute_chat_lines_sig(
        app.active_conv_key.as_deref(),
        &view,
        app.messages_gen,
        app.messages_prefix_gen,
        app.presences_gen,
        ui.density,
        active_unread_ts,
        &app.my_user_id,
        my_display_name,
        width,
        hyperlinks,
        ui.show_hrefs,
        ui.inline_images,
        app.image_cache_gen,
        ui.graphics_caps.cell_px,
        app.loading_older,
        app.oldest_reached,
        app.history_error.as_deref(),
        &ui.theme,
    );
    let previous = CHAT_LINES_CACHE.take();
    if let Some(cached) = previous.as_ref().filter(|cached| cached.signature == signature) {
        let result = cached.clone();
        CHAT_LINES_CACHE.set(previous);
        return result;
    }

    let thumb_cols = width.saturating_sub(4).clamp(24, 72) as u16;
    let thumb_rows = 16u16;

    let mut out = Vec::new();
    let mut ranges = Vec::with_capacity(view.messages.len());
    let mut inline_images: Vec<(usize, String, AttachmentInfo)> = Vec::new();
    if let Some(cached) = previous.filter(|cached| {
        tail_message_updated && cached.signature.prefix == signature.prefix && cached.ranges.len() > 1
    }) {
        if let Ok(lines) = Arc::try_unwrap(cached.lines) {
            let prefix_len = cached.ranges.len() - 1;
            ranges.extend_from_slice(&cached.ranges[..prefix_len]);
            inline_images.extend(
                cached
                    .inline_images
                    .iter()
                    .filter(|(idx, _, _)| *idx < prefix_len)
                    .cloned(),
            );
            out = lines;
            // The date divider and spacing belong to the following message.
            out.truncate(ranges[ranges.len() - 1].1);
        }
    }
    let start_index = ranges.len();
    collect_inline_image_attachments(view.messages, start_index, &mut inline_images);
    if ranges.is_empty() && app.active_conv_key.is_some() {
        if let Some(banner) = build_history_banner_line(
            app.loading_older,
            app.oldest_reached,
            app.history_error.as_deref(),
            !view.messages.is_empty(),
            &ui.theme,
            width,
        ) {
            out.push(banner);
        }
    }

    let thumbs = if ui.inline_images {
        Some(&app.image_cache)
    } else {
        None
    };

    let ranges = build_chat_body_lines(
        &mut out,
        &view,
        ui.density,
        active_unread_ts,
        &app.my_user_id,
        my_display_name,
        Some(&app.presences),
        app.my_presence.as_ref(),
        &ui.theme,
        width,
        thumbs,
        (thumb_cols, thumb_rows),
        ui.graphics_caps.cell_px,
        ui.show_hrefs,
        ranges,
    );

    let cached = ChatStream {
        signature,
        lines: Arc::new(out),
        ranges: Arc::from(ranges),
        inline_images: Arc::from(inline_images),
    };
    CHAT_LINES_CACHE.set(Some(cached.clone()));
    cached
}

pub fn build_lines_with_hyperlinks(
    view: &ChatRender,
    theme: &Theme,
    width: usize,
    _hyperlinks: bool,
) -> Vec<RenderedLine> {
    let width = width.max(12);
    let mut out = Vec::new();
    let _ = build_chat_body_lines(
        &mut out,
        view,
        DensityMode::Cozy,
        None,
        "",
        None,
        None,
        None,
        theme,
        width,
        None,
        (0, 0),
        img_preview::TerminalGraphicsCaps::default().cell_px,
        false,
        Vec::new(),
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn build_chat_body_lines(
    out: &mut Vec<RenderedLine>,
    view: &ChatRender,
    density: DensityMode,
    unread_anchor_ts: Option<&str>,
    my_user_id: &str,
    my_display_name: Option<&str>,
    presences: Option<&HashMap<String, PresenceStatus>>,
    my_presence: Option<&PresenceStatus>,
    theme: &Theme,
    width: usize,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
    cell_px: (u16, u16),
    show_hrefs: bool,
    mut ranges: Vec<(usize, usize)>,
) -> Vec<(usize, usize)> {
    let start_index = ranges.len();
    let first_unread = find_first_unread(view.messages, unread_anchor_ts);
    let mut last_date_key = start_index
        .checked_sub(1)
        .map(|idx| format_date_key(&view.messages[idx].created_at));
    ranges.reserve(view.messages.len() - start_index);

    for (msg_index, msg) in view.messages.iter().enumerate().skip(start_index) {
        let mut just_emitted_divider = false;
        let date_key = format_date_key(&msg.created_at);
        if last_date_key.as_deref() != Some(date_key.as_str()) {
            if density == DensityMode::Cozy && msg_index > 0 {
                out.push(RenderedLine::plain(Line::raw("")));
            }
            let date_label = format_date(&msg.created_at);
            out.push(RenderedLine::plain(Line::from(Span::styled(
                format!("── {date_label} ──"),
                Style::default()
                    .fg(theme.date_separator)
                    .add_modifier(Modifier::BOLD),
            ))));
            last_date_key = Some(date_key);
            just_emitted_divider = true;
        }

        let is_first_after_unread = if let Some((unread_idx, unread_count)) = first_unread {
            if msg_index == unread_idx {
                let rule_label = if unread_count == 1 {
                    "─── 1 unread message ───".to_string()
                } else {
                    format!("─── {unread_count} unread messages ───")
                };
                let rule_w = UnicodeWidthStr::width(rule_label.as_str());
                let left_pad = width.saturating_sub(rule_w) / 2;
                let centered = format!("{}{}", " ".repeat(left_pad), rule_label);
                out.push(RenderedLine::plain(Line::from(Span::styled(
                    truncate(&centered, width),
                    Style::default()
                        .fg(theme.unread_divider)
                        .add_modifier(Modifier::BOLD),
                ))));
                just_emitted_divider = true;
                true
            } else {
                false
            }
        } else {
            false
        };

        let is_grouped = msg_index > 0
            && messages_groupable(
                &view.messages[msg_index - 1],
                msg,
                is_first_after_unread,
            );

        if density == DensityMode::Cozy && !is_grouped && !just_emitted_divider && msg_index > 0 {
            out.push(RenderedLine::plain(Line::raw("")));
        }

        let start_line = out.len();
        let selected = view.selected_index == Some(msg_index);
        let ctx = MessageRenderCtx {
            scope: MessageScope::Chat,
            is_channel: view.is_channel,
            density,
            is_grouped,
            selected,
            theme,
            width,
            my_user_id,
            my_display_name,
            presences,
            my_presence,
            image_cache,
            thumb_dims,
            cell_px,
            show_hrefs,
        };
        render_message_item_with_ctx(out, msg, msg_index, &ctx);
        let end_line = out.len();
        ranges.push((start_line, end_line));
    }

    ranges
}

fn find_first_unread(
    messages: &[MessageItem],
    unread_anchor_ts: Option<&str>,
) -> Option<(usize, usize)> {
    let anchor_ts = unread_anchor_ts?;
    let anchor_dt = DateTime::parse_from_rfc3339(anchor_ts).ok();
    let mut first_idx = None;
    let mut count = 0usize;
    for (idx, m) in messages.iter().enumerate() {
        if m.is_me {
            continue;
        }
        let newer = match (&anchor_dt, DateTime::parse_from_rfc3339(&m.created_at)) {
            (Some(a), Ok(b)) => b > *a,
            _ => m.created_at.as_str() > anchor_ts,
        };
        if newer {
            if first_idx.is_none() {
                first_idx = Some(idx);
            }
            count += 1;
        }
    }
    first_idx.map(|idx| (idx, count))
}

fn same_sender(prev: &MessageItem, curr: &MessageItem) -> bool {
    match (
        prev.author_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        curr.author_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => prev.author_name == curr.author_name,
    }
}

pub(crate) fn messages_groupable(
    prev: &MessageItem,
    curr: &MessageItem,
    is_first_after_unread_divider: bool,
) -> bool {
    if is_first_after_unread_divider || prev.deleted || curr.deleted {
        return false;
    }
    if format_date_key(&prev.created_at) != format_date_key(&curr.created_at) {
        return false;
    }
    if !same_sender(prev, curr) {
        return false;
    }
    if !curr.subject.as_deref().unwrap_or("").trim().is_empty() {
        return false;
    }
    if curr.reply_to_id.is_some() || curr.reply_to_preview.is_some() {
        return false;
    }
    match (
        DateTime::parse_from_rfc3339(&prev.created_at),
        DateTime::parse_from_rfc3339(&curr.created_at),
    ) {
        (Ok(p), Ok(c)) => {
            let delta = c.timestamp() - p.timestamp();
            (0..=300).contains(&delta)
        }
        _ => false,
    }
}

pub(crate) struct MessageRenderCtx<'a> {
    pub scope: MessageScope,
    pub is_channel: bool,
    pub density: DensityMode,
    pub is_grouped: bool,
    pub selected: bool,
    pub theme: &'a Theme,
    pub width: usize,
    pub my_user_id: &'a str,
    pub my_display_name: Option<&'a str>,
    pub presences: Option<&'a HashMap<String, PresenceStatus>>,
    pub my_presence: Option<&'a PresenceStatus>,
    pub image_cache: Option<&'a HashMap<String, RenderedTerminalImage>>,
    pub thumb_dims: (u16, u16),
    pub cell_px: (u16, u16),
    pub show_hrefs: bool,
}

/// Renders a single `MessageItem` into `RenderedLine`s. Shared by `chat_view` and `thread_view`.
#[allow(clippy::too_many_arguments)]
pub fn render_message_item(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    scope: MessageScope,
    is_channel: bool,
    selected: bool,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) {
    render_message_item_with_thumbs(
        out,
        msg,
        msg_index,
        scope,
        is_channel,
        selected,
        theme,
        width,
        hyperlinks,
        None,
        (0, 0),
    );
}

#[allow(clippy::too_many_arguments)]
pub fn render_message_item_with_thumbs(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    scope: MessageScope,
    is_channel: bool,
    selected: bool,
    theme: &Theme,
    width: usize,
    _hyperlinks: bool,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
) {
    let ctx = MessageRenderCtx {
        scope,
        is_channel,
        density: DensityMode::Cozy,
        is_grouped: false,
        selected,
        theme,
        width,
        my_user_id: "",
        my_display_name: None,
        presences: None,
        my_presence: None,
        image_cache,
        thumb_dims,
        cell_px: img_preview::TerminalGraphicsCaps::default().cell_px,
        show_hrefs: false,
    };
    render_message_item_with_ctx(out, msg, msg_index, &ctx);
}

pub(crate) fn render_message_item_with_ctx(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    ctx: &MessageRenderCtx<'_>,
) {
    let start_idx = out.len();
    match ctx.density {
        DensityMode::Cozy => render_message_cozy(out, msg, msg_index, ctx),
        DensityMode::Compact => render_message_compact(out, msg, msg_index, ctx),
    }

    if ctx.selected && out.len() > start_idx {
        let show_thread_action =
            ctx.is_channel && ctx.scope == MessageScope::Chat && msg.parent_msg_id.is_none();
        apply_selection_to_lines(
            &mut out[start_idx..],
            ctx.scope,
            msg_index,
            ctx.width,
            ctx.theme,
            show_thread_action,
        );
    }
}

fn author_name_style(msg: &MessageItem, theme: &Theme) -> Style {
    if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else if msg.is_me {
        Style::default().fg(theme.own).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.author(&msg.author_name))
            .add_modifier(Modifier::BOLD)
    }
}

fn resolve_author_presence<'a>(
    msg: &MessageItem,
    ctx: &'a MessageRenderCtx<'a>,
) -> Option<&'a str> {
    if msg.is_me {
        if let Some(p) = ctx.my_presence {
            return Some(p.availability.as_str());
        }
    }
    msg.author_id
        .as_deref()
        .and_then(|id| ctx.presences.and_then(|m| m.get(id)))
        .map(|p| p.availability.as_str())
}

fn render_message_cozy(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    ctx: &MessageRenderCtx<'_>,
) {
    let theme = ctx.theme;
    let width = ctx.width;
    let target = HitTarget::Message {
        scope: ctx.scope,
        msg_index,
    };

    // Group start header: <2-col gutter><presence/dot> <Author>  <HH:MM> [badges]
    if !ctx.is_grouped {
        let time = format_time(&msg.created_at);
        let author_style = author_name_style(msg, theme);
        let (dot, dot_color) = match resolve_author_presence(msg, ctx) {
            Some(avail) => theme.presence_indicator(Some(avail)),
            None => {
                let c = if msg.is_me {
                    theme.own
                } else {
                    theme.author(&msg.author_name)
                };
                ("●", c)
            }
        };
        let dot_prefix = format!("{dot} ");
        let dot_style = if theme.no_color {
            Style::default()
        } else {
            Style::default().fg(dot_color)
        };

        let mut suffix_spans = vec![Span::styled(format!("  {time}"), theme.muted_style())];
        if msg.importance.eq_ignore_ascii_case("urgent") {
            suffix_spans.push(Span::styled(
                " [URGENT]",
                Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
            ));
        } else if msg.importance.eq_ignore_ascii_case("high") {
            suffix_spans.push(Span::styled(
                " [IMPORTANT]",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if msg.last_modified_at.is_some() && !msg.deleted {
            suffix_spans.push(Span::styled(" (edited)", theme.muted_style()));
        }

        let prefix_w = 2 + UnicodeWidthStr::width(dot_prefix.as_str());
        let suffix_w: usize = suffix_spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let author_budget = width.saturating_sub(prefix_w + suffix_w);
        let author_display =
            if UnicodeWidthStr::width(msg.author_name.as_str()) <= author_budget {
                msg.author_name.clone()
            } else if author_budget >= 2 {
                format!("{}…", truncate(&msg.author_name, author_budget - 1))
            } else {
                truncate(&msg.author_name, author_budget)
            };

        let mut header_spans = vec![
            Span::raw("  "),
            Span::styled(dot_prefix, dot_style),
            Span::styled(author_display, author_style),
        ];
        header_spans.extend(suffix_spans);
        out.push(RenderedLine::clickable(Line::from(header_spans), target));
    }

    // Channel root post subject renders BELOW the author header (indented 4 cols, wrapped losslessly)
    if let Some(subject) = msg.subject.as_deref().filter(|s| !s.trim().is_empty()) {
        let first_prefix = [
            Span::raw("  "),
            Span::styled("  ◆ ", theme.subject_style()),
        ];
        let cont_prefix = [Span::raw("  "), Span::raw("    ")];
        let wrapped = bitefoundry_html::layout::wrap_styled_spans(
            &[Span::styled(subject.to_string(), theme.subject_style())],
            width,
            bitefoundry_html::layout::WrapPrefix {
                first: &first_prefix,
                cont: &cont_prefix,
            },
        );
        for line in wrapped {
            out.push(RenderedLine::clickable(line, target));
        }
    }

    // Quoted reply banner (indented 4 cols)
    if msg.reply_to_author.is_some() || msg.reply_to_preview.is_some() {
        let author = msg.reply_to_author.as_deref().unwrap_or("Reply");
        let preview = msg
            .reply_to_preview
            .as_deref()
            .unwrap_or("")
            .replace(['\n', '\r'], " ");
        let prefix = format!("▎ {author}: \"");
        let prefix_w = UnicodeWidthStr::width(prefix.as_str());
        let avail = width.saturating_sub(4 + prefix_w + 1);
        let raw_banner = format!("  {prefix}{}\"", truncate(&preview, avail));
        let banner = truncate(&raw_banner, width.saturating_sub(2));
        out.push(RenderedLine::clickable(
            Line::from(vec![
                Span::raw("  "),
                Span::styled(banner, theme.quote_style()),
            ]),
            target,
        ));
    }

    if msg.deleted {
        out.push(RenderedLine::clickable(
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "  ⊘ Message deleted",
                    theme.muted_style().add_modifier(Modifier::ITALIC),
                ),
            ]),
            target,
        ));
        return;
    }

    let indent = "    ";
    let body_ctx = BodyBlockCtx {
        indent,
        indent_col: 4,
        theme,
        width,
        my_user_id: ctx.my_user_id,
        my_display_name: ctx.my_display_name,
        mentions: &msg.mentions,
        scope: ctx.scope,
        msg_index,
        target,
        image_cache: ctx.image_cache,
        thumb_dims: ctx.thumb_dims,
        cell_px: ctx.cell_px,
        show_hrefs: ctx.show_hrefs,
    };
    render_message_body_and_attachments(out, msg, &body_ctx);

    // Reaction badges row(s) (`    [👍 2] [❤️ 1]`) — flow-wrapped across lines
    if !msg.reactions.is_empty() {
        let mut spans = vec![Span::raw("    ")];
        let mut inline_targets = Vec::new();
        let mut col = 4u16;
        for (r_idx, r) in msg.reactions.iter().enumerate() {
            let emoji = reaction_emoji(&r.reaction_type);
            let badge = format!("[{emoji} {}]", r.count);
            let badge_w = UnicodeWidthStr::width(badge.as_str()) as u16;
            let gap = if col > 4 { 1u16 } else { 0u16 };
            if col > 4 && (col as usize) + (gap as usize) + (badge_w as usize) > width {
                out.push(RenderedLine::with_inline(
                    Line::from(std::mem::replace(&mut spans, vec![Span::raw("    ")])),
                    Some(target),
                    std::mem::take(&mut inline_targets),
                ));
                col = 4;
            }
            if col > 4 {
                spans.push(Span::raw(" "));
                col += 1;
            }
            let max_badge_w = width.saturating_sub(col as usize);
            let fitted = if (badge_w as usize) > max_badge_w && max_badge_w > 0 {
                truncate(&badge, max_badge_w)
            } else {
                badge
            };
            let fitted_w = UnicodeWidthStr::width(fitted.as_str()) as u16;
            spans.push(Span::styled(fitted, theme.reaction_style(r.me_reacted)));
            inline_targets.push((
                col,
                fitted_w,
                HitTarget::Reaction {
                    scope: ctx.scope,
                    msg_index,
                    reaction_index: r_idx,
                },
            ));
            col += fitted_w;
        }
        if col > 4 {
            out.push(RenderedLine::with_inline(
                Line::from(spans),
                Some(target),
                inline_targets,
            ));
        }
    }

    // Quiet Thread Pill: render ONLY when reply_count > 0
    if ctx.is_channel
        && ctx.scope == MessageScope::Chat
        && msg.parent_msg_id.is_none()
        && msg.reply_count > 0
    {
        let label = if msg.reply_count == 1 {
            "    ↳ 1 reply · Enter to open".to_string()
        } else {
            format!("    ↳ {} replies · Enter to open", msg.reply_count)
        };
        let trunc = truncate(&label, width);
        let label_w = UnicodeWidthStr::width(trunc.trim_start()) as u16;
        out.push(RenderedLine::with_inline(
            Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    trunc.strip_prefix("    ").unwrap_or(&trunc).to_string(),
                    theme.accent_style(),
                ),
            ]),
            Some(target),
            vec![(4, label_w, HitTarget::OpenThread { msg_index })],
        ));
    }
}

fn format_compact_author(name: &str, width: usize) -> String {
    let name_w = UnicodeWidthStr::width(name);
    if name_w <= width {
        format!("{}{name}", " ".repeat(width - name_w))
    } else if width >= 2 {
        let trunc = truncate(name, width - 1);
        let trunc_w = UnicodeWidthStr::width(trunc.as_str()) + 1;
        format!("{}{trunc}…", " ".repeat(width.saturating_sub(trunc_w)))
    } else {
        truncate(name, width)
    }
}

fn render_message_compact(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    ctx: &MessageRenderCtx<'_>,
) {
    let theme = ctx.theme;
    let width = ctx.width;
    let content_w = width.saturating_sub(22).max(6);
    let target = HitTarget::Message {
        scope: ctx.scope,
        msg_index,
    };

    let mut content_lines: Vec<RenderedLine> = Vec::new();
    let mut last_is_appendable = false;

    if msg.deleted {
        content_lines.push(RenderedLine::plain(Line::from(Span::styled(
            truncate("⊘ Message deleted", content_w),
            theme.muted_style().add_modifier(Modifier::ITALIC),
        ))));
    } else {
        if msg.importance.eq_ignore_ascii_case("urgent") {
            content_lines.push(RenderedLine::plain(Line::from(Span::styled(
                "[URGENT]",
                Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
            ))));
        } else if msg.importance.eq_ignore_ascii_case("high") {
            content_lines.push(RenderedLine::plain(Line::from(Span::styled(
                "[IMPORTANT]",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ))));
        }

        if let Some(subject) = msg.subject.as_deref().filter(|s| !s.trim().is_empty()) {
            let first_prefix = [Span::styled("◆ ", theme.subject_style())];
            let cont_prefix = [Span::raw("  ")];
            let wrapped = bitefoundry_html::layout::wrap_styled_spans(
                &[Span::styled(subject.to_string(), theme.subject_style())],
                content_w,
                bitefoundry_html::layout::WrapPrefix {
                    first: &first_prefix,
                    cont: &cont_prefix,
                },
            );
            for line in wrapped {
                content_lines.push(RenderedLine::plain(line));
            }
            last_is_appendable = true;
        }

        if msg.reply_to_author.is_some() || msg.reply_to_preview.is_some() {
            let author = msg.reply_to_author.as_deref().unwrap_or("Reply");
            let preview = msg
                .reply_to_preview
                .as_deref()
                .unwrap_or("")
                .replace(['\n', '\r'], " ");
            let prefix = format!("▎ {author}: \"");
            let prefix_w = UnicodeWidthStr::width(prefix.as_str());
            let avail = content_w.saturating_sub(prefix_w + 1);
            content_lines.push(RenderedLine::plain(Line::from(Span::styled(
                format!("{prefix}{}\"", truncate(&preview, avail)),
                theme.quote_style(),
            ))));
            last_is_appendable = true;
        }

        let before_body = content_lines.len();
        let body_ctx = BodyBlockCtx {
            indent: "",
            indent_col: 0,
            theme,
            width: content_w,
            my_user_id: ctx.my_user_id,
            my_display_name: ctx.my_display_name,
            mentions: &msg.mentions,
            scope: ctx.scope,
            msg_index,
            target,
            image_cache: ctx.image_cache,
            thumb_dims: ctx.thumb_dims,
            cell_px: ctx.cell_px,
            show_hrefs: ctx.show_hrefs,
        };
        let ended_with_plain_text = render_message_body_and_attachments(&mut content_lines, msg, &body_ctx);
        if content_lines.len() > before_body {
            last_is_appendable = ended_with_plain_text;
        }

        if msg.last_modified_at.is_some() {
            let edited_span = Span::styled(" (edited)", theme.muted_style());
            let edited_w = UnicodeWidthStr::width(" (edited)");
            if last_is_appendable {
                if let Some(last) = content_lines.last_mut() {
                    let lw = line_display_width(&last.line);
                    if lw + edited_w <= content_w {
                        last.line.spans.push(edited_span);
                    } else {
                        content_lines.push(RenderedLine::plain(Line::from(Span::styled(
                            "(edited)",
                            theme.muted_style(),
                        ))));
                    }
                }
            } else {
                content_lines.push(RenderedLine::plain(Line::from(Span::styled(
                    "(edited)",
                    theme.muted_style(),
                ))));
            }
        }

        // Compact suffix: `[👍2] [❤️1] · ↳ 4` (flow-wrapped so `↳ N` never disappears)
        type SuffixRow = (Vec<Span<'static>>, Vec<(u16, u16, HitTarget)>, u16);
        let mut suffix_rows: Vec<SuffixRow> = Vec::new();
        let mut cur_spans: Vec<Span<'static>> = Vec::new();
        let mut cur_targets: Vec<(u16, u16, HitTarget)> = Vec::new();
        let mut scol = 0u16;

        for (r_idx, r) in msg.reactions.iter().enumerate() {
            let emoji = reaction_emoji(&r.reaction_type);
            let badge = format!("[{emoji}{}]", r.count);
            let bw = UnicodeWidthStr::width(badge.as_str()) as u16;
            let gap = if scol > 0 { 1u16 } else { 0u16 };
            if scol > 0 && (scol as usize) + (gap as usize) + (bw as usize) > content_w {
                suffix_rows.push((
                    std::mem::take(&mut cur_spans),
                    std::mem::take(&mut cur_targets),
                    scol,
                ));
                scol = 0;
            }
            if scol > 0 {
                cur_spans.push(Span::raw(" "));
                scol += 1;
            }
            let avail = content_w.saturating_sub(scol as usize);
            let fitted = if (bw as usize) > avail && avail > 0 {
                truncate(&badge, avail)
            } else {
                badge
            };
            let fitted_w = UnicodeWidthStr::width(fitted.as_str()) as u16;
            cur_spans.push(Span::styled(fitted, theme.reaction_style(r.me_reacted)));
            cur_targets.push((
                scol,
                fitted_w,
                HitTarget::Reaction {
                    scope: ctx.scope,
                    msg_index,
                    reaction_index: r_idx,
                },
            ));
            scol += fitted_w;
        }

        if ctx.is_channel
            && ctx.scope == MessageScope::Chat
            && msg.parent_msg_id.is_none()
            && msg.reply_count > 0
        {
            let pill = format!("↳ {}", msg.reply_count);
            let pw = UnicodeWidthStr::width(pill.as_str()) as u16;
            let sep_w = if scol == 0 { 0u16 } else { 3u16 };
            if scol > 0 && (scol as usize) + (sep_w as usize) + (pw as usize) > content_w {
                suffix_rows.push((
                    std::mem::take(&mut cur_spans),
                    std::mem::take(&mut cur_targets),
                    scol,
                ));
                scol = 0;
            }
            if scol > 0 {
                cur_spans.push(Span::styled(" · ", theme.muted_style()));
                scol += 3;
            }
            let avail = content_w.saturating_sub(scol as usize);
            let fitted_pill = if (pw as usize) > avail && avail > 0 {
                truncate(&pill, avail)
            } else {
                pill
            };
            let fitted_pw = UnicodeWidthStr::width(fitted_pill.as_str()) as u16;
            cur_spans.push(Span::styled(fitted_pill, theme.accent_style()));
            cur_targets.push((scol, fitted_pw, HitTarget::OpenThread { msg_index }));
            scol += fitted_pw;
        }

        if scol > 0 {
            suffix_rows.push((cur_spans, cur_targets, scol));
        }

        if suffix_rows.len() == 1 && last_is_appendable {
            let mut appended = false;
            if let Some(last) = content_lines.last_mut() {
                let lw = line_display_width(&last.line);
                let row_w = suffix_rows[0].2 as usize;
                if lw + 2 + row_w <= content_w {
                    let (spans, targets, _) = suffix_rows.remove(0);
                    last.line.spans.push(Span::raw("  "));
                    last.line.spans.extend(spans);
                    let base_col = (lw + 2) as u16;
                    for (c, w, t) in targets {
                        last.inline_targets.push((base_col + c, w, t));
                    }
                    appended = true;
                }
            }
            if !appended {
                for (spans, targets, _) in suffix_rows {
                    content_lines.push(RenderedLine::with_inline(
                        Line::from(spans),
                        Some(target),
                        targets,
                    ));
                }
            }
        } else {
            for (spans, targets, _) in suffix_rows {
                content_lines.push(RenderedLine::with_inline(
                    Line::from(spans),
                    Some(target),
                    targets,
                ));
            }
        }
    }

    if content_lines.is_empty() {
        content_lines.push(RenderedLine::plain(Line::raw("")));
    }

    let time = format_time(&msg.created_at);
    let author_col = format_compact_author(&msg.author_name, 11);
    let author_style = author_name_style(msg, theme);
    let rail_style = theme.subtle_style();

    for (ci, mut cline) in content_lines.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(5 + cline.line.spans.len());
        spans.push(Span::raw("  "));
        if ci == 0 {
            spans.push(Span::styled(format!("{time} "), theme.muted_style()));
            if ctx.is_grouped {
                spans.push(Span::raw("            "));
            } else {
                spans.push(Span::styled(format!("{author_col} "), author_style));
            }
            spans.push(Span::styled("│ ", rail_style));
        } else {
            spans.push(Span::styled("                  │ ", rail_style));
        }
        spans.extend(cline.line.spans);
        cline.line = Line::from(spans);
        cline.target = Some(target);
        for (col, _, _) in &mut cline.inline_targets {
            *col = col.saturating_add(22);
        }
        out.push(cline);
    }
}

struct BodyBlockCtx<'a> {
    indent: &'a str,
    indent_col: u16,
    theme: &'a Theme,
    width: usize,
    my_user_id: &'a str,
    my_display_name: Option<&'a str>,
    mentions: &'a [MentionInfo],
    scope: MessageScope,
    msg_index: usize,
    target: HitTarget,
    image_cache: Option<&'a HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
    cell_px: (u16, u16),
    show_hrefs: bool,
}

/// Renders parsed `RichBlock`s + attachments + thumbnails.
/// Returns `true` if the last emitted line is plain body text suitable for inline suffixing.
fn render_message_body_and_attachments(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    bctx: &BodyBlockCtx<'_>,
) -> bool {
    #[cfg(test)]
    RENDERED_MESSAGE_BODIES.set(RENDERED_MESSAGE_BODIES.get() + 1);

    let eff_attachments = msg.effective_attachments();
    let attachments = eff_attachments.as_ref();
    let parsed_blocks =
        parser::parse_message_blocks_arc(&msg.body_html, attachments, &msg.mentions);
    let fallback_blocks: Vec<RichBlock>;
    let blocks: &[RichBlock] = if parsed_blocks.is_empty() && !msg.body_plain.trim().is_empty() {
        fallback_blocks = parser::html_in::parse_plain_markdown_blocks(&msg.body_plain);
        &fallback_blocks
    } else {
        &parsed_blocks
    };

    let mut emitted_attach_indices: Vec<usize> = Vec::new();
    let mut emitted_thumb_ids: Vec<String> = Vec::new();
    let mut ended_with_text = false;

    for block in blocks {
        match block {
            RichBlock::AttachmentChip { id, name } => {
                ended_with_text = false;
                let attach_idx = resolve_attachment_index(attachments, Some(id.as_str()), None)
                    .unwrap_or(0);
                emitted_attach_indices.push(attach_idx);
                let display_name = name
                    .as_deref()
                    .or_else(|| attachments.get(attach_idx).and_then(|a| a.name.as_deref()))
                    .unwrap_or("attachment");
                let is_img = attachments.get(attach_idx).is_some_and(|a| {
                    img_preview::is_image_mime_or_name(&a.content_type, a.name.as_deref())
                });
                let is_html = !is_img
                    && attachments.get(attach_idx).is_some_and(
                        crate::ui::overlays::html_preview::HtmlPreviewOverlay::is_html_attachment,
                    );
                let is_md = !is_img
                    && !is_html
                    && attachments.get(attach_idx).is_some_and(
                        crate::ui::overlays::html_preview::HtmlPreviewOverlay::is_markdown_attachment,
                    );
                let tag = if is_img {
                    "IMG "
                } else if is_html {
                    "HTML "
                } else if is_md {
                    "MD "
                } else {
                    "FILE "
                };
                let chip = format!("[{tag} {display_name}]");
                let max_chip_w = bctx.width.saturating_sub(bctx.indent_col as usize);
                let trunc_chip = truncate(&chip, max_chip_w);
                let chip_w = UnicodeWidthStr::width(trunc_chip.as_str()) as u16;
                let mut spans = Vec::new();
                if !bctx.indent.is_empty() {
                    spans.push(Span::raw(bctx.indent.to_string()));
                }
                spans.push(Span::styled(
                    trunc_chip,
                    bctx.theme.accent_style().add_modifier(Modifier::BOLD),
                ));
                out.push(RenderedLine::with_inline(
                    Line::from(spans),
                    Some(bctx.target),
                    vec![(
                        bctx.indent_col,
                        chip_w,
                        HitTarget::Attachment {
                            scope: bctx.scope,
                            msg_index: bctx.msg_index,
                            attach_index: attach_idx,
                        },
                    )],
                ));
                if let (Some(cache), Some(att)) = (bctx.image_cache, attachments.get(attach_idx)) {
                    if is_img && !emitted_thumb_ids.contains(&att.id) {
                        emitted_thumb_ids.push(att.id.clone());
                        emit_inline_thumb(
                            out,
                            &msg.msg_id,
                            att,
                            cache,
                            bctx.thumb_dims,
                            bctx.cell_px,
                            bctx.indent,
                            bctx.width,
                            bctx.theme,
                            bctx.scope,
                            bctx.msg_index,
                            attach_idx,
                        );
                    }
                }
            }
            RichBlock::InlineImageChip { src, index } => {
                ended_with_text = false;
                let attach_idx = resolve_attachment_index(attachments, None, Some(src.as_str()))
                    .unwrap_or(*index);
                emitted_attach_indices.push(attach_idx);
                let chip = format!("[IMG #{}]", index + 1);
                let max_chip_w = bctx.width.saturating_sub(bctx.indent_col as usize);
                let trunc_chip = truncate(&chip, max_chip_w);
                let chip_w = UnicodeWidthStr::width(trunc_chip.as_str()) as u16;
                let mut spans = Vec::new();
                if !bctx.indent.is_empty() {
                    spans.push(Span::raw(bctx.indent.to_string()));
                }
                spans.push(Span::styled(trunc_chip, bctx.theme.accent_style()));
                out.push(RenderedLine::with_inline(
                    Line::from(spans),
                    Some(bctx.target),
                    vec![(
                        bctx.indent_col,
                        chip_w,
                        HitTarget::Attachment {
                            scope: bctx.scope,
                            msg_index: bctx.msg_index,
                            attach_index: attach_idx,
                        },
                    )],
                ));
                if let (Some(cache), Some(att)) = (bctx.image_cache, attachments.get(attach_idx)) {
                    if !emitted_thumb_ids.contains(&att.id) {
                        emitted_thumb_ids.push(att.id.clone());
                        emit_inline_thumb(
                            out,
                            &msg.msg_id,
                            att,
                            cache,
                            bctx.thumb_dims,
                            bctx.cell_px,
                            bctx.indent,
                            bctx.width,
                            bctx.theme,
                            bctx.scope,
                            bctx.msg_index,
                            attach_idx,
                        );
                    }
                }
            }
            other => {
                ended_with_text = matches!(
                    other,
                    RichBlock::Paragraph(_)
                        | RichBlock::Heading { .. }
                        | RichBlock::Quote(_)
                        | RichBlock::BulletList(_)
                );
                render_rich_blocks_inner(
                    out,
                    std::slice::from_ref(other),
                    bctx.indent,
                    Some(bctx.target),
                    bctx.theme,
                    bctx.width,
                    bctx.my_user_id,
                    bctx.my_display_name,
                    bctx.mentions,
                    bctx.show_hrefs,
                );
            }
        }
    }

    // Emit any remaining file/reference attachments not already emitted by parser blocks
    for (idx, att) in attachments.iter().enumerate() {
        if emitted_attach_indices.contains(&idx) {
            continue;
        }
        if att
            .content_type
            .to_ascii_lowercase()
            .contains("card.adaptive")
        {
            continue;
        }
        ended_with_text = false;
        let name = att.name.as_deref().unwrap_or(&att.id);
        let is_img = img_preview::is_image_mime_or_name(&att.content_type, att.name.as_deref());
        let is_html = !is_img
            && crate::ui::overlays::html_preview::HtmlPreviewOverlay::is_html_attachment(att);
        let is_md = !is_img
            && !is_html
            && crate::ui::overlays::html_preview::HtmlPreviewOverlay::is_markdown_attachment(att);
        let tag = if is_img {
            "IMG "
        } else if is_html {
            "HTML "
        } else if is_md {
            "MD "
        } else {
            "FILE "
        };
        let chip = format!("[{tag} {name}]");
        let max_chip_w = bctx.width.saturating_sub(bctx.indent_col as usize);
        let trunc_chip = truncate(&chip, max_chip_w);
        let chip_w = UnicodeWidthStr::width(trunc_chip.as_str()) as u16;
        let mut spans = Vec::new();
        if !bctx.indent.is_empty() {
            spans.push(Span::raw(bctx.indent.to_string()));
        }
        spans.push(Span::styled(
            trunc_chip,
            bctx.theme.accent_style().add_modifier(Modifier::BOLD),
        ));
        out.push(RenderedLine::with_inline(
            Line::from(spans),
            Some(bctx.target),
            vec![(
                bctx.indent_col,
                chip_w,
                HitTarget::Attachment {
                    scope: bctx.scope,
                    msg_index: bctx.msg_index,
                    attach_index: idx,
                },
            )],
        ));
        if let Some(cache) = bctx.image_cache {
            if is_img && !emitted_thumb_ids.contains(&att.id) {
                emitted_thumb_ids.push(att.id.clone());
                emit_inline_thumb(
                    out,
                    &msg.msg_id,
                    att,
                    cache,
                    bctx.thumb_dims,
                    bctx.cell_px,
                    bctx.indent,
                    bctx.width,
                    bctx.theme,
                    bctx.scope,
                    bctx.msg_index,
                    idx,
                );
            }
        }
    }

    ended_with_text
}

#[allow(clippy::too_many_arguments)]
fn emit_inline_thumb(
    out: &mut Vec<RenderedLine>,
    msg_id: &str,
    att: &AttachmentInfo,
    cache: &HashMap<String, RenderedTerminalImage>,
    thumb_dims: (u16, u16),
    cell_px: (u16, u16),
    indent: &str,
    width: usize,
    theme: &Theme,
    scope: MessageScope,
    msg_index: usize,
    attach_idx: usize,
) {
    let bg_rgb = AppState::inline_image_bg(theme);
    let key = AppState::image_cache_key(msg_id, &att.id, thumb_dims.0, thumb_dims.1, cell_px, bg_rgb);
    if let Some(rendered) = cache.get(&key) {
        let target = HitTarget::Attachment {
            scope,
            msg_index,
            attach_index: attach_idx,
        };
        for thumb_line in &rendered.lines {
            let mut full = Vec::new();
            if !indent.is_empty() {
                full.push(Span::raw(indent.to_string()));
            }
            full.extend(thumb_line.spans.clone());
            out.push(RenderedLine::clickable(Line::from(full), target));
        }
        let meta = format!(
            "↳ {}x{} ({}) · click or Alt+v for HD preview",
            rendered.width_px, rendered.height_px, rendered.format_label
        );
        let max_w = width.saturating_sub(UnicodeWidthStr::width(indent));
        let mut full = Vec::new();
        if !indent.is_empty() {
            full.push(Span::raw(indent.to_string()));
        }
        full.push(Span::styled(truncate(&meta, max_w), theme.muted_style()));
        out.push(RenderedLine::clickable(Line::from(full), target));
    }
}

fn line_display_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

pub(crate) fn apply_selection_to_line(
    rendered: &mut RenderedLine,
    is_first_line: bool,
    scope: MessageScope,
    msg_index: usize,
    width: usize,
    theme: &Theme,
    show_thread_action: bool,
) {
    let sel_bg = if theme.no_color || theme.bg_selected == Color::Reset {
        None
    } else {
        Some(theme.bg_selected)
    };
    let gutter_style = match sel_bg {
        Some(bg) => Style::default()
            .fg(theme.accent)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::BOLD),
    };

    // Replace leading 2-col gutter with `▎ `
    let mut new_spans: Vec<Span<'static>> = Vec::with_capacity(rendered.line.spans.len() + 2);
    let mut replaced_gutter = false;
    for (si, span) in rendered.line.spans.drain(..).enumerate() {
        if si == 0 && !replaced_gutter {
            if let Some(rest) = span.content.strip_prefix("  ") {
                new_spans.push(Span::styled("▎ ", gutter_style));
                if !rest.is_empty() {
                    new_spans.push(Span::styled(rest.to_string(), span.style));
                }
                replaced_gutter = true;
                continue;
            }
        }
        new_spans.push(span);
    }
    if !replaced_gutter {
        new_spans.insert(0, Span::styled("▎ ", gutter_style));
    }

    // Non-destructive bg_selected tint on all non-gutter spans without clobbering `.fg`
    if let Some(bg) = sel_bg {
        for span in new_spans.iter_mut().skip(1) {
            if span.style.bg.is_none() || span.style.bg == Some(Color::Reset) {
                span.style = span.style.bg(bg);
            }
        }
    }

    rendered.line = Line::from(new_spans);
    let cur_w = line_display_width(&rendered.line);

    let pad_style = match sel_bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    };

    let mut added_chips = false;
    if is_first_line {
        let chips: &[(&str, InlineActionKind)] = if show_thread_action {
            &[
                ("r reply", InlineActionKind::Reply),
                ("t thread", InlineActionKind::Thread),
                ("+ react", InlineActionKind::React),
                ("[H: HTML]", InlineActionKind::HtmlPreview),
            ]
        } else {
            &[
                ("r reply", InlineActionKind::Reply),
                ("+ react", InlineActionKind::React),
                ("[H: HTML]", InlineActionKind::HtmlPreview),
            ]
        };
        let sep = " · ";
        let sep_w = UnicodeWidthStr::width(sep);
        let chips_w: usize = chips
            .iter()
            .map(|(l, _)| UnicodeWidthStr::width(*l))
            .sum::<usize>()
            + sep_w * chips.len().saturating_sub(1);

        if cur_w + 2 + chips_w <= width {
            let trail = usize::from(cur_w + 2 + chips_w < width);
            let pad = width.saturating_sub(cur_w + chips_w + trail);
            if pad > 0 {
                rendered
                    .line
                    .spans
                    .push(Span::styled(" ".repeat(pad), pad_style));
            }
            let chip_style = match sel_bg {
                Some(bg) => Style::default().fg(theme.accent).bg(bg),
                None => Style::default().add_modifier(Modifier::BOLD),
            };
            let sep_style = match sel_bg {
                Some(bg) => Style::default().fg(theme.fg_muted).bg(bg),
                None => theme.muted_style(),
            };
            let mut col = (cur_w + pad) as u16;
            for (ci, &(label, action)) in chips.iter().enumerate() {
                if ci > 0 {
                    rendered.line.spans.push(Span::styled(sep, sep_style));
                    col += sep_w as u16;
                }
                let cw = UnicodeWidthStr::width(label) as u16;
                rendered.line.spans.push(Span::styled(label, chip_style));
                rendered.inline_targets.push((
                    col,
                    cw,
                    HitTarget::InlineAction {
                        scope,
                        msg_index,
                        action,
                    },
                ));
                col += cw;
            }
            if trail > 0 {
                rendered.line.spans.push(Span::styled(" ", pad_style));
            }
            added_chips = true;
        }
    }

    if !added_chips && cur_w < width && sel_bg.is_some() {
        rendered
            .line
            .spans
            .push(Span::styled(" ".repeat(width - cur_w), pad_style));
    }
}

pub(crate) fn apply_selection_to_lines(
    slice: &mut [RenderedLine],
    scope: MessageScope,
    msg_index: usize,
    width: usize,
    theme: &Theme,
    show_thread_action: bool,
) {
    for (line_idx, rendered) in slice.iter_mut().enumerate() {
        apply_selection_to_line(
            rendered,
            line_idx == 0,
            scope,
            msg_index,
            width,
            theme,
            show_thread_action,
        );
    }
}

/// Shared `RichBlock` renderer used by Teams chat and thread views.
#[allow(clippy::too_many_arguments)]
pub fn render_rich_blocks(
    out: &mut Vec<RenderedLine>,
    blocks: &[RichBlock],
    indent: &str,
    selected: bool,
    target: Option<HitTarget>,
    theme: &Theme,
    width: usize,
    _hyperlinks: bool,
) {
    let start = out.len();
    render_rich_blocks_inner(
        out,
        blocks,
        indent,
        target,
        theme,
        width,
        "",
        None,
        &[],
        false,
    );
    if selected && out.len() > start {
        if let Some(HitTarget::Message { scope, msg_index }) = target {
            apply_selection_to_lines(&mut out[start..], scope, msg_index, width, theme, false);
        } else if !theme.no_color && theme.bg_selected != Color::Reset {
            for r in &mut out[start..] {
                for s in &mut r.line.spans {
                    if s.style.bg.is_none() || s.style.bg == Some(Color::Reset) {
                        s.style = s.style.bg(theme.bg_selected);
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_rich_blocks_inner(
    out: &mut Vec<RenderedLine>,
    blocks: &[RichBlock],
    indent: &str,
    target: Option<HitTarget>,
    theme: &Theme,
    width: usize,
    my_user_id: &str,
    my_display_name: Option<&str>,
    mentions: &[MentionInfo],
    show_hrefs: bool,
) {
    let indent_w = UnicodeWidthStr::width(indent);
    let body_w = width.saturating_sub(indent_w).max(6);
    let mut push_line = |line: Line<'static>| {
        if let Some(t) = target {
            out.push(RenderedLine::clickable(line, t));
        } else {
            out.push(RenderedLine::plain(line));
        }
    };

    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                let wrapped = wrap_rich_spans(
                    spans,
                    body_w,
                    theme,
                    my_user_id,
                    my_display_name,
                    mentions,
                    show_hrefs,
                );
                for line_spans in wrapped {
                    let mut full = if indent.is_empty() {
                        Vec::new()
                    } else {
                        vec![Span::raw(indent.to_string())]
                    };
                    full.extend(line_spans);
                    push_line(Line::from(full));
                }
            }
            RichBlock::Heading { level, spans } => {
                let prefix = match level {
                    1 => "# ",
                    2 => "## ",
                    _ => "### ",
                };
                let wrapped = wrap_rich_spans(
                    spans,
                    body_w.saturating_sub(prefix.len()),
                    theme,
                    my_user_id,
                    my_display_name,
                    mentions,
                    show_hrefs,
                );
                for (i, line_spans) in wrapped.into_iter().enumerate() {
                    let mut full = vec![Span::styled(
                        if i == 0 {
                            format!("{indent}{prefix}")
                        } else {
                            format!("{indent}{}", " ".repeat(prefix.len()))
                        },
                        theme.accent_style().add_modifier(Modifier::BOLD),
                    )];
                    for s in line_spans {
                        full.push(Span::styled(
                            s.content.into_owned(),
                            s.style.add_modifier(Modifier::BOLD),
                        ));
                    }
                    push_line(Line::from(full));
                }
            }
            RichBlock::Quote(spans) => {
                let wrapped = wrap_rich_spans(
                    spans,
                    body_w.saturating_sub(2),
                    theme,
                    my_user_id,
                    my_display_name,
                    mentions,
                    show_hrefs,
                );
                for line_spans in wrapped {
                    let mut full = Vec::new();
                    if !indent.is_empty() {
                        full.push(Span::raw(indent.to_string()));
                    }
                    full.push(Span::styled("│ ", theme.quote_style()));
                    for s in line_spans {
                        full.push(Span::styled(s.content.into_owned(), theme.quote_style()));
                    }
                    push_line(Line::from(full));
                }
            }
            RichBlock::CodeBlock { lang, code } => {
                let code_bg = if theme.no_color || theme.bg_code == Color::Reset {
                    None
                } else {
                    Some(theme.bg_code)
                };
                let header_text = match lang.as_deref().filter(|l| !l.is_empty()) {
                    Some(l) => format!("┌─ {l}"),
                    None => "┌─ code".to_string(),
                };
                let header_trunc = truncate(&header_text, body_w);
                let header_style = match code_bg {
                    Some(bg) => Style::default().fg(theme.fg_muted).bg(bg),
                    None => theme.muted_style(),
                };
                let mut top_spans = Vec::new();
                if !indent.is_empty() {
                    top_spans.push(Span::raw(indent.to_string()));
                }
                top_spans.push(Span::styled(header_trunc.clone(), header_style));
                if let Some(bg) = code_bg {
                    let used = UnicodeWidthStr::width(header_trunc.as_str());
                    if used < body_w {
                        top_spans.push(Span::styled(
                            " ".repeat(body_w - used),
                            Style::default().bg(bg),
                        ));
                    }
                }
                push_line(Line::from(top_spans));

                let code_prefix = [Span::styled("│ ", header_style)];
                let code_style = theme.code_style();
                for raw_line in code.lines() {
                    let wrapped = bitefoundry_html::layout::wrap_styled_spans(
                        &[Span::styled(raw_line.to_string(), code_style)],
                        body_w,
                        bitefoundry_html::layout::WrapPrefix {
                            first: &code_prefix,
                            cont: &code_prefix,
                        },
                    );
                    for line in wrapped {
                        let used = line_display_width(&line);
                        let mut code_spans = Vec::new();
                        if !indent.is_empty() {
                            code_spans.push(Span::raw(indent.to_string()));
                        }
                        code_spans.extend(line.spans);
                        if let Some(bg) = code_bg {
                            if used < body_w {
                                code_spans.push(Span::styled(
                                    " ".repeat(body_w - used),
                                    Style::default().bg(bg),
                                ));
                            }
                        }
                        push_line(Line::from(code_spans));
                    }
                }

                let mut bot_spans = Vec::new();
                if !indent.is_empty() {
                    bot_spans.push(Span::raw(indent.to_string()));
                }
                bot_spans.push(Span::styled("└─", header_style));
                if let Some(bg) = code_bg {
                    if body_w > 2 {
                        bot_spans.push(Span::styled(
                            " ".repeat(body_w - 2),
                            Style::default().bg(bg),
                        ));
                    }
                }
                push_line(Line::from(bot_spans));
            }
            RichBlock::BulletList(items) => {
                for item in items {
                    let wrapped = wrap_rich_spans(
                        item,
                        body_w.saturating_sub(2),
                        theme,
                        my_user_id,
                        my_display_name,
                        mentions,
                        show_hrefs,
                    );
                    for (i, line_spans) in wrapped.into_iter().enumerate() {
                        let mut full = Vec::new();
                        if !indent.is_empty() {
                            full.push(Span::raw(indent.to_string()));
                        }
                        full.push(Span::styled(
                            if i == 0 { "• " } else { "  " },
                            theme.accent_style(),
                        ));
                        full.extend(line_spans);
                        push_line(Line::from(full));
                    }
                }
            }
            RichBlock::Card {
                title,
                subtitle,
                facts,
                body_lines,
                actions,
            } => {
                let card_bg = if theme.no_color || theme.bg_card == Color::Reset {
                    None
                } else {
                    Some(theme.bg_card)
                };
                let bar_style = match card_bg {
                    Some(bg) => Style::default().fg(theme.accent).bg(bg),
                    None => theme.accent_style(),
                };
                let bar_prefix = [Span::styled("┃ ", bar_style)];
                let push_card_spans =
                    |push: &mut dyn FnMut(Line<'static>), styled_spans: &[Span<'static>]| {
                        let wrapped = bitefoundry_html::wrap_styled_spans(
                            styled_spans,
                            body_w,
                            bitefoundry_html::WrapPrefix {
                                first: &bar_prefix,
                                cont: &bar_prefix,
                            },
                        );
                        for line in wrapped {
                            let used: usize = line
                                .spans
                                .iter()
                                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                                .sum();
                            let mut spans = Vec::new();
                            if !indent.is_empty() {
                                spans.push(Span::raw(indent.to_string()));
                            }
                            spans.extend(line.spans);
                            if let Some(bg) = card_bg {
                                if used < body_w {
                                    spans.push(Span::styled(
                                        " ".repeat(body_w - used),
                                        Style::default().bg(bg),
                                    ));
                                }
                            }
                            push(Line::from(spans));
                        }
                    };

                let card_title = title.as_deref().unwrap_or("Adaptive Card");
                let mut t_style = Style::default().add_modifier(Modifier::BOLD);
                if let Some(bg) = card_bg {
                    t_style = t_style.fg(theme.fg_primary).bg(bg);
                }
                let title_rich = crate::parser::card_in::parse_card_markdown_spans(card_title);
                let title_spans = if title_rich.is_empty() {
                    vec![Span::styled(card_title.to_string(), t_style)]
                } else {
                    card_rich_to_spans(&title_rich, t_style, card_bg, theme, show_hrefs)
                };
                push_card_spans(&mut push_line, &title_spans);

                if let Some(sub) = subtitle {
                    let s_style = match card_bg {
                        Some(bg) => Style::default().fg(theme.fg_muted).bg(bg),
                        None => theme.muted_style(),
                    };
                    let sub_rich = crate::parser::card_in::parse_card_markdown_spans(sub);
                    let sub_spans = if sub_rich.is_empty() {
                        vec![Span::styled(sub.clone(), s_style)]
                    } else {
                        card_rich_to_spans(&sub_rich, s_style, card_bg, theme, show_hrefs)
                    };
                    push_card_spans(&mut push_line, &sub_spans);
                }
                for (k, v) in facts {
                    let f_style = match card_bg {
                        Some(bg) => Style::default().fg(theme.fg_secondary).bg(bg),
                        None => Style::default(),
                    };
                    let key_style = f_style.add_modifier(Modifier::BOLD);
                    let mut fact_spans: Vec<Span<'static>> = Vec::new();
                    if !k.trim().is_empty() {
                        let k_rich = crate::parser::card_in::parse_card_markdown_spans(k);
                        if k_rich.is_empty() {
                            fact_spans.push(Span::styled(k.clone(), key_style));
                        } else {
                            fact_spans.extend(card_rich_to_spans(
                                &k_rich,
                                key_style,
                                card_bg,
                                theme,
                                show_hrefs,
                            ));
                        }
                        fact_spans.push(Span::styled(": ", f_style));
                    }
                    let v_rich = crate::parser::card_in::parse_card_markdown_spans(v);
                    if v_rich.is_empty() {
                        fact_spans.push(Span::styled(v.clone(), f_style));
                    } else {
                        fact_spans.extend(card_rich_to_spans(
                            &v_rich,
                            f_style,
                            card_bg,
                            theme,
                            show_hrefs,
                        ));
                    }
                    push_card_spans(&mut push_line, &fact_spans);
                }
                for bl in body_lines {
                    let b_style = match card_bg {
                        Some(bg) => Style::default().fg(theme.fg_primary).bg(bg),
                        None => Style::default(),
                    };
                    let bl_rich = crate::parser::card_in::parse_card_markdown_spans(bl);
                    let bl_spans = if bl_rich.is_empty() {
                        vec![Span::styled(bl.clone(), b_style)]
                    } else {
                        card_rich_to_spans(&bl_rich, b_style, card_bg, theme, show_hrefs)
                    };
                    push_card_spans(&mut push_line, &bl_spans);
                }
                for (label, url) in actions {
                    let link_label = if show_hrefs
                        && !url.trim().is_empty()
                        && !label.contains(url.trim())
                    {
                        format!("[{label}] ({})", url.trim())
                    } else {
                        format!("[{label}]")
                    };
                    let a_style = match card_bg {
                        Some(bg) => Style::default()
                            .fg(theme.accent)
                            .bg(bg)
                            .add_modifier(Modifier::UNDERLINED),
                        None => theme.accent_style().add_modifier(Modifier::UNDERLINED),
                    };
                    push_card_spans(&mut push_line, &[Span::styled(link_label, a_style)]);
                }
            }
            RichBlock::AttachmentChip { .. } | RichBlock::InlineImageChip { .. } => {}
        }
    }
}

fn extract_hosted_content_id(src: &str) -> Option<&str> {
    let idx = src.find("hostedContents/")?;
    if idx > 0 && src.as_bytes()[idx - 1] != b'/' {
        return None;
    }
    let rest = &src[idx + "hostedContents/".len()..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let seg = &rest[..end];
    if seg.is_empty() {
        None
    } else {
        Some(seg)
    }
}

fn resolve_attachment_index(
    attachments: &[AttachmentInfo],
    id: Option<&str>,
    src: Option<&str>,
) -> Option<usize> {
    if let Some(id) = id {
        if let Some(pos) = attachments.iter().position(|a| a.id == id) {
            return Some(pos);
        }
    }
    if let Some(src) = src {
        if let Some(pos) = attachments
            .iter()
            .position(|a| a.content_url.as_deref() == Some(src))
        {
            return Some(pos);
        }
        if let Some(seg) = extract_hosted_content_id(src) {
            let hosted_id = format!("hosted:{seg}");
            if let Some(pos) = attachments.iter().position(|a| {
                a.id == hosted_id
                    || a.content_url
                        .as_deref()
                        .and_then(extract_hosted_content_id)
                        == Some(seg)
            }) {
                return Some(pos);
            }
        }
    }
    None
}

pub fn reaction_emoji(reaction_type: &str) -> &str {
    match reaction_type {
        "like" => "👍",
        "heart" => "❤️",
        "laugh" => "😆",
        "surprised" => "😮",
        "sad" => "😢",
        "angry" => "😡",
        other => other,
    }
}

fn is_mention_for_me(
    mention_text: &str,
    my_user_id: &str,
    my_display_name: Option<&str>,
    mentions: &[MentionInfo],
) -> bool {
    let bare = mention_text.trim().trim_start_matches('@').trim();
    if bare.is_empty() {
        return false;
    }
    if !my_user_id.is_empty()
        && mentions.iter().any(|m| {
            m.user_id == my_user_id && m.display_name.trim().eq_ignore_ascii_case(bare)
        })
    {
        return true;
    }
    if let Some(dn) = my_display_name.map(str::trim).filter(|s| !s.is_empty()) {
        if dn.eq_ignore_ascii_case(bare) {
            return true;
        }
    }
    false
}

fn card_rich_to_spans(
    rich: &[RichSpan],
    base_style: Style,
    card_bg: Option<Color>,
    theme: &Theme,
    show_hrefs: bool,
) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(rich.len());
    for piece in rich {
        let mut style = base_style;
        if piece.style.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if piece.style.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if piece.style.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if piece.style.underline {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if piece.style.code {
            style = theme.code_style();
            if style.bg.is_none() || style.bg == Some(Color::Reset) {
                if let Some(bg) = card_bg {
                    style = style.bg(bg);
                }
            }
        }
        if piece.style.link_url.is_some() {
            style = style.fg(theme.accent).add_modifier(Modifier::UNDERLINED);
        }
        out.push(Span::styled(piece.text.clone(), style));
        if show_hrefs {
            if let Some(url) = piece
                .style
                .link_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
            {
                let label_trimmed = piece.text.trim();
                if !label_trimmed.eq_ignore_ascii_case(url) && !label_trimmed.contains(url) {
                    let muted = match card_bg {
                        Some(bg) => theme.muted_style().bg(bg),
                        None => theme.muted_style(),
                    };
                    out.push(Span::styled(format!(" ({url})"), muted));
                }
            }
        }
    }
    out
}

fn span_style(
    s: &RichSpan,
    theme: &Theme,
    my_user_id: &str,
    my_display_name: Option<&str>,
    mentions: &[MentionInfo],
) -> Style {
    let mut style = Style::default();
    if s.style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.style.strike {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if s.style.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if s.style.code {
        style = theme.code_style();
    }
    if s.style.mention {
        if is_mention_for_me(&s.text, my_user_id, my_display_name, mentions) {
            style = if theme.no_color {
                style.add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                style
                    .fg(theme.fg_mention_me)
                    .bg(theme.bg_mention_me)
                    .add_modifier(Modifier::BOLD)
            };
        } else {
            style = style.fg(theme.unread).add_modifier(Modifier::BOLD);
        }
    }
    if s.style.link_url.is_some() {
        style = style
            .fg(theme.accent)
            .add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn wrap_rich_spans(
    rich: &[RichSpan],
    width: usize,
    theme: &Theme,
    my_user_id: &str,
    my_display_name: Option<&str>,
    mentions: &[MentionInfo],
    show_hrefs: bool,
) -> Vec<Vec<Span<'static>>> {
    let limit = width.max(1);
    let mut styled: Vec<Span<'static>> = Vec::with_capacity(rich.len());
    for piece in rich {
        let style = span_style(piece, theme, my_user_id, my_display_name, mentions);
        styled.push(Span::styled(piece.text.clone(), style));

        if show_hrefs {
            if let Some(url) = piece
                .style
                .link_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
            {
                let label_trimmed = piece.text.trim();
                if !label_trimmed.eq_ignore_ascii_case(url) && !label_trimmed.contains(url) {
                    styled.push(Span::styled(format!(" ({url})"), theme.muted_style()));
                }
            }
        }
    }

    bitefoundry_html::layout::wrap_styled_spans(
        &styled,
        limit,
        bitefoundry_html::layout::WrapPrefix::empty(),
    )
    .into_iter()
    .map(|line| line.spans)
    .collect()
}

pub fn hyperlinks_enabled() -> bool {
    match std::env::var("TEAMS_HYPERLINKS") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "false" | "off" | "no")
        }
        Err(_) => true,
    }
}

pub fn format_date_key(iso: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&Local).format("%Y-%m-%d").to_string();
    }
    iso.get(..10).unwrap_or(iso).to_string()
}

pub fn format_date(iso: &str) -> String {
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        let local = dt.with_timezone(&Local);
        let today = Local::now().date_naive();
        let date = local.date_naive();
        if date == today {
            return format!("Today · {}", local.format("%m-%d"));
        }
        if Some(date) == today.pred_opt() {
            return format!("Yesterday · {}", local.format("%m-%d"));
        }
        let wd = WEEKDAYS[local.weekday().num_days_from_monday() as usize];
        return format!("{} ({wd})", local.format("%Y-%m-%d"));
    }
    iso.get(..10).unwrap_or(iso).to_string()
}

pub fn format_time(iso: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&Local).format("%H:%M").to_string();
    }
    iso.get(11..16).unwrap_or("--:--").to_string()
}

pub fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
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
    use crate::models::ReactionSummary;

    fn sample_msg(id: &str, subject: Option<&str>, reply_count: usize) -> MessageItem {
        MessageItem {
            msg_key: format!("chan:t1:c1|{id}"),
            conv_key: "chan:t1:c1".into(),
            msg_id: id.into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: subject.map(str::to_string),
            author_id: Some("u1".into()),
            author_name: "Alice".into(),
            is_me: false,
            created_at: "2026-11-15T06:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: "Hello team".into(),
            body_plain: "Hello team".into(),
            attachments: vec![AttachmentInfo {
                id: "att-1".into(),
                content_type: "reference".into(),
                content_url: Some("https://contoso.sharepoint.com/Budget.docx".into()),
                name: Some("Budget.docx".into()),
                content_json: None,
                thumbnail_url: None,
            }],
            mentions: vec![],
            reactions: vec![ReactionSummary {
                reaction_type: "like".into(),
                count: 2,
                me_reacted: true,
                users: vec!["u1".into(), "u2".into()],
            }],
            reply_count,
            web_url: None,
        }
    }

    fn plain_text(lines: &[RenderedLine]) -> String {
        lines
            .iter()
            .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn channel_post_renders_subject_attachment_reactions_and_thread_indicator() {
        let msgs = vec![sample_msg("m1", Some("Weekly Report"), 3)];
        let view = ChatRender {
            messages: &msgs,
            is_channel: true,
            selected_index: Some(0),
        };
        let lines = build_lines_with_hyperlinks(&view, &Theme::default(), 80, false);
        let joined = plain_text(&lines);

        assert!(joined.contains("Weekly Report"));
        assert!(joined.contains("Budget.docx"));
        assert!(joined.contains("👍 2"));
        assert!(joined.contains("3 replies"));
        assert!(joined.contains("▎ "));
        assert!(joined.contains("r reply"));
        assert!(lines.iter().any(|l| l
            .inline_targets
            .iter()
            .any(|(_, _, t)| matches!(t, HitTarget::OpenThread { msg_index: 0 }))));
        assert!(lines.iter().any(|l| l
            .inline_targets
            .iter()
            .any(|(_, _, t)| matches!(t, HitTarget::Reaction { msg_index: 0, reaction_index: 0, .. }))));
        assert!(lines.iter().any(|l| l
            .inline_targets
            .iter()
            .any(|(_, _, t)| matches!(t, HitTarget::Attachment { msg_index: 0, attach_index: 0, .. }))));
        assert!(lines.iter().any(|l| l.inline_targets.iter().any(|(_, _, t)| matches!(
            t,
            HitTarget::InlineAction {
                scope: MessageScope::Chat,
                msg_index: 0,
                action: InlineActionKind::Reply
            }
        ))));
    }

    #[test]
    fn zero_reply_channel_post_omits_start_thread_line() {
        let msgs = vec![sample_msg("m1", Some("Update"), 0)];
        let view = ChatRender {
            messages: &msgs,
            is_channel: true,
            selected_index: None,
        };
        let joined = plain_text(&build_lines_with_hyperlinks(&view, &Theme::default(), 80, false));
        assert!(!joined.contains("Start thread"));
        assert!(!joined.contains("Enter to open"));
    }

    #[test]
    fn five_minute_author_grouping_cozy_mode() {
        let mut m1 = sample_msg("m1", None, 0);
        m1.attachments.clear();
        m1.reactions.clear();
        m1.body_html = "First".into();
        m1.body_plain = "First".into();
        m1.created_at = "2026-11-15T06:00:00Z".into();

        let mut m2 = m1.clone();
        m2.msg_id = "m2".into();
        m2.body_html = "Second".into();
        m2.body_plain = "Second".into();
        m2.created_at = "2026-11-15T06:04:59Z".into(); // +299s -> grouped

        let mut m3 = m1.clone();
        m3.msg_id = "m3".into();
        m3.body_html = "Third".into();
        m3.body_plain = "Third".into();
        m3.created_at = "2026-11-15T06:10:01Z".into(); // +302s -> new group

        let msgs = vec![m1, m2, m3];
        let view = ChatRender {
            messages: &msgs,
            is_channel: false,
            selected_index: None,
        };
        let mut out = Vec::new();
        let ranges = build_chat_body_lines(
            &mut out,
            &view,
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            &Theme::default(),
            80,
            None,
            (0, 0),
            (10, 20),
            false,
            Vec::new(),
        );
        assert_eq!(ranges.len(), 3);
        // m1 and m2 have 0 blank lines between them (m1 ends where m2 starts)
        assert_eq!(ranges[0].1, ranges[1].0);
        // m2 and m3 have 1 blank spacer line between them
        assert_eq!(ranges[1].1 + 1, ranges[2].0);
        // Author name appears twice (m1 and m3, omitted on grouped m2)
        let joined = plain_text(&out);
        assert_eq!(joined.matches("Alice").count(), 2);
    }

    #[test]
    fn compact_mode_renders_22_col_gutter_and_inline_reactions_and_unread_divider() {
        let mut m1 = sample_msg("m1", None, 0);
        m1.attachments.clear();
        m1.reactions.clear();
        m1.body_html = "Earlier".into();
        m1.body_plain = "Earlier".into();
        m1.created_at = "2026-11-15T06:00:00Z".into();

        let mut m2 = sample_msg("m2", None, 4);
        m2.attachments.clear();
        m2.body_html = "Unread reply".into();
        m2.body_plain = "Unread reply".into();
        m2.created_at = "2026-11-15T06:01:00Z".into(); // within 5 min, but after unread divider!

        let msgs = vec![m1, m2];
        let view = ChatRender {
            messages: &msgs,
            is_channel: true,
            selected_index: Some(1),
        };
        let mut out = Vec::new();
        let ranges = build_chat_body_lines(
            &mut out,
            &view,
            DensityMode::Compact,
            Some("2026-11-15T06:00:30Z"),
            "",
            None,
            None,
            None,
            &Theme::default(),
            100,
            None,
            (0, 0),
            (10, 20),
            false,
            Vec::new(),
        );
        let joined = plain_text(&out);
        assert!(joined.contains("─── 1 unread message ───"));
        assert!(joined.contains("│ Unread reply"));
        assert!(joined.contains("[👍2] · ↳ 4"));
        assert_eq!(ranges.len(), 2);
        // Both m1 and m2 show author because unread divider broke grouping
        assert_eq!(joined.matches("Alice").count(), 2);
    }

    #[test]
    fn selection_preserves_child_foreground_colors_and_sets_bg_selected() {
        let mut m1 = sample_msg("m1", None, 0);
        m1.attachments.clear();
        m1.reactions.clear();
        m1.body_html = "<p>Check <a href=\"https://example.com\">link</a> and <at id=\"0\">Bob</at></p>".into();
        m1.body_plain = "Check link and @Bob".into();
        m1.mentions = vec![MentionInfo {
            id: 0,
            user_id: "me".into(),
            display_name: "Bob".into(),
        }];

        let theme = Theme::midnight_dark();
        let view = ChatRender {
            messages: &[m1],
            is_channel: false,
            selected_index: Some(0),
        };
        let mut out = Vec::new();
        build_chat_body_lines(
            &mut out,
            &view,
            DensityMode::Cozy,
            None,
            "me",
            Some("Bob"),
            None,
            None,
            &theme,
            80,
            None,
            (0, 0),
            (10, 20),
            false,
            Vec::new(),
        );
        let joined = plain_text(&out);
        assert!(!joined.contains('\x1b'));
        assert!(joined.contains("link"));
        assert!(joined.contains("@Bob"));

        // Verify link span retains theme.accent fg AND gains bg_selected
        let body_line = out.iter().find(|r| {
            r.line.spans.iter().any(|s| s.content.contains("link"))
        }).unwrap();
        let link_span = body_line.line.spans.iter().find(|s| s.content.contains("link")).unwrap();
        assert_eq!(link_span.style.fg, Some(theme.accent));
        assert_eq!(link_span.style.bg, Some(theme.bg_selected));

        // Verify @Bob mention keeps fg_mention_me and bg_mention_me without being overwritten by bg_selected
        let mention_span = body_line.line.spans.iter().find(|s| s.content.contains("@Bob")).unwrap();
        assert_eq!(mention_span.style.fg, Some(theme.fg_mention_me));
        assert_eq!(mention_span.style.bg, Some(theme.bg_mention_me));
    }

    #[tokio::test]
    async fn tail_edit_and_reaction_render_one_body_and_preserve_the_view() {
        use crate::{models::CoreEvent, ui::app::tests::make_test_app};
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_test_app();
        let conv = "chat:long-history";
        app.active_conv_key = Some(conv.into());
        app.messages = (1..=1580)
            .map(|i| {
                let mut msg = sample_msg(&format!("msg-{i:04}"), None, 0);
                msg.conv_key = conv.into();
                msg.msg_key = MessageItem::make_msg_key(conv, &msg.msg_id);
                msg.body_html = format!("<p>History row {i}</p>");
                msg.body_plain = format!("History row {i}");
                msg
            })
            .collect();
        app.messages.last_mut().unwrap().created_at = "2026-11-16T06:00:00Z".into();
        ui.unread_anchor = Some((conv.into(), app.messages[0].created_at.clone()));
        ui.inline_images = false;
        ui.chat_scroll.selected_idx = Some(1579);
        app.storage.upsert_messages(&app.messages).unwrap();

        let snapshot = |app: &mut AppState, ui: &mut UiState| {
            let area = Rect::new(0, 0, 80, 24);
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hitmap = HitMap::new();
            terminal.draw(|frame| draw(frame, area, app, ui, &mut hitmap)).unwrap();
            let hits: Vec<_> = hitmap.regions().copied().collect();
            (
                terminal.backend().buffer().clone(),
                hits,
                ui.chat_scroll.offset,
                ui.chat_scroll.selected_idx,
            )
        };

        for density in [DensityMode::Cozy, DensityMode::Compact] {
            ui.density = density;
            snapshot(&mut app, &mut ui);
            for reaction in [false, true] {
                let mut tail = app.messages.last().unwrap().clone();
                if reaction {
                    tail.reactions[0].count += 1;
                } else {
                    tail.body_html.push_str("<p>Added <a href=\"https://example.com\">a link</a></p>");
                    tail.body_plain.push_str(" Added a link");
                }
                app.storage.upsert_messages(&[tail.clone()]).unwrap();
                app.handle_core_event(&mut ui, CoreEvent::MessagesUpdated {
                    conv_key: conv.into(),
                    parent_msg_id: None,
                    changed_msg_ids: Some(vec![tail.msg_id.clone()]),
                    messages: vec![tail],
                    notify_candidate: None,
                });
                assert!(app.tail_message_updated);
                RENDERED_MESSAGE_BODIES.set(0);
                let fast = snapshot(&mut app, &mut ui);
                assert_eq!(RENDERED_MESSAGE_BODIES.get(), 1);
                assert!(fast.1.iter().any(|(_, hit)| matches!(hit,
                    HitTarget::Reaction { msg_index: 1579, .. })));
                let fast_stream = CHAT_LINES_CACHE.take().unwrap();
                RENDERED_MESSAGE_BODIES.set(0);
                assert_eq!(fast, snapshot(&mut app, &mut ui));
                assert_eq!(RENDERED_MESSAGE_BODIES.get(), 1580);
                CHAT_LINES_CACHE.with_borrow(|cache| {
                    let full = cache.as_ref().unwrap();
                    assert_eq!(fast_stream.ranges, full.ranges);
                    assert_eq!(fast_stream.lines, full.lines);
                });
            }
        }

        let old_lines = CHAT_LINES_CACHE.with_borrow(|cache| {
            Arc::downgrade(&cache.as_ref().unwrap().lines)
        });
        app.active_conv_key = Some("chat:other".into());
        app.messages.truncate(1);
        snapshot(&mut app, &mut ui);
        assert!(old_lines.upgrade().is_none());
        app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    #[test]
    fn chat_render_signature_and_line_cache_behavior() {
        let msgs = vec![sample_msg("m1", Some("Weekly Report"), 3)];
        let theme = Theme::default();
        let sig_base = {
            let view = ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            };
            compute_chat_lines_sig(
                Some("chan:t1:c1"),
                &view,
                1,
                1,
                0,
                DensityMode::Cozy,
                None,
                "",
                None,
                80,
                false,
                false,
                true,
                0,
                (10, 20),
                false,
                false,
                None,
                &theme,
            )
        };

        let mut cache = LineSliceCache::new(2);
        let lines: Arc<[RenderedLine]> = Arc::from(build_lines_with_hyperlinks(
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            &theme,
            80,
            false,
        ));
        let cached_val = CachedStream {
            lines: Arc::clone(&lines),
            ranges: Arc::from(vec![(0, lines.len())]),
        };
        cache.insert(sig_base.full, cached_val.clone());
        let hit = cache.get(sig_base.full).expect("expected cache hit");
        assert_eq!(hit.lines.len(), lines.len());

        let sig_sel = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: None,
            },
            1,
            1,
            0,
            DensityMode::Cozy,
            None,
            "",
            None,
            80,
            false,
            false,
            true,
            0,
            (10, 20),
            false,
            false,
            None,
            &theme,
        );
        // Selection is applied at render time on the visible slice, so sig_base == sig_sel.
        assert_eq!(sig_base, sig_sel);

        let sig_compact = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            1,
            1,
            0,
            DensityMode::Compact,
            None,
            "",
            None,
            80,
            false,
            false,
            true,
            0,
            (10, 20),
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_compact);

        let sig_width = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            1,
            1,
            0,
            DensityMode::Cozy,
            None,
            "",
            None,
            100,
            false,
            false,
            true,
            0,
            (10, 20),
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_width);

        // Tail-only mutation bumps messages_gen while keeping prefix equal.
        let sig_tail = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            2,
            1,
            0,
            DensityMode::Cozy,
            None,
            "",
            None,
            80,
            false,
            false,
            true,
            0,
            (10, 20),
            false,
            false,
            None,
            &theme,
        );
        assert_eq!(sig_base.prefix, sig_tail.prefix);
        assert_ne!(sig_base.full, sig_tail.full);

        cache.insert(sig_compact.full, cached_val.clone());
        cache.insert(sig_width.full, cached_val);
        assert!(cache.get(sig_base.full).is_none());
        assert!(cache.get(sig_width.full).is_some());

        // Bumping prefix gen or presences gen invalidates both prefix and full signatures.
        let sig_prefix = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: None,
            },
            2,
            2,
            0,
            DensityMode::Cozy,
            None,
            "",
            None,
            80,
            false,
            false,
            true,
            0,
            (10, 20),
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base.prefix, sig_prefix.prefix);
        assert_ne!(sig_base.full, sig_prefix.full);
    }

    #[test]
    fn prepend_anchor_preserves_viewport_offset_without_jump() {
        let mut ui = UiState::new();
        ui.chat_scroll.follow_tail = false;
        ui.chat_scroll.total_lines = 100;
        ui.chat_scroll.offset = 0;
        ui.chat_scroll.scroll_up = 120;
        ui.pending_prepend_anchor = Some((100, 0));

        apply_prepend_anchor(&mut ui, 150, 20);
        ui.chat_scroll.total_lines = 150;
        assert_eq!(ui.chat_scroll.offset(20), 50);
        assert!(ui.pending_prepend_anchor.is_none());
    }

    #[test]
    fn history_banner_renders_all_states_and_registers_click_target() {
        let theme = Theme::default();

        let loading = build_history_banner_line(true, false, None, true, &theme, 80).unwrap();
        let loading_text: String = loading.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(loading_text.contains("Loading older messages"));
        assert_eq!(loading.target, None);

        let err = build_history_banner_line(false, false, Some("Insufficient permissions"), true, &theme, 80).unwrap();
        let err_text: String = err.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(err_text.contains("Insufficient permissions"));
        assert_eq!(err.target, Some(HitTarget::LoadOlderMessages));

        let eof = build_history_banner_line(false, true, None, true, &theme, 80).unwrap();
        let eof_text: String = eof.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(eof_text.contains("Beginning of conversation"));
        assert_eq!(eof.target, None);

        let more = build_history_banner_line(false, false, None, true, &theme, 80).unwrap();
        let more_text: String = more.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(more_text.contains("o / H"));
        assert_eq!(more.target, Some(HitTarget::LoadOlderMessages));
    }

    #[test]
    fn resolve_attachment_index_matches_hosted_content_segment_accurately() {
        let attachments = vec![
            AttachmentInfo {
                id: "hosted:10".into(),
                content_type: "image/png".into(),
                content_url: Some("/chats/c1/messages/m1/hostedContents/10/$value".into()),
                name: Some("image10.png".into()),
                content_json: None,
                thumbnail_url: None,
            },
            AttachmentInfo {
                id: "hosted:1".into(),
                content_type: "image/png".into(),
                content_url: Some("/chats/c1/messages/m1/hostedContents/1/$value".into()),
                name: Some("image1.png".into()),
                content_json: None,
                thumbnail_url: None,
            },
        ];

        assert_eq!(
            resolve_attachment_index(&attachments, None, Some("../hostedContents/1/$value")),
            Some(1)
        );
        assert_eq!(
            resolve_attachment_index(
                &attachments,
                None,
                Some("https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value")
            ),
            Some(1)
        );
        assert_eq!(
            resolve_attachment_index(&attachments, None, Some("../hostedContents/10/$value")),
            Some(0)
        );
    }

    #[test]
    fn inline_hosted_content_renders_clickable_thumb_and_caption() {
        let mut msg = sample_msg("m1", None, 0);
        msg.conv_key = "chat:c1".into();
        msg.msg_key = "chat:c1|m1".into();
        msg.attachments.clear();
        msg.reactions.clear();
        msg.body_html =
            "<p>Look:</p><img src=\"https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value\">"
                .into();
        msg.body_plain = "Look:".into();

        let theme = Theme::default();
        let thumb_dims = (24u16, 16u16);
        let cell_px = (10u16, 20u16);
        let cache_key = AppState::image_cache_key(
            "m1",
            "hosted:1",
            thumb_dims.0,
            thumb_dims.1,
            cell_px,
            AppState::inline_image_bg(&theme),
        );
        let mut cache = HashMap::new();
        cache.insert(
            cache_key,
            RenderedTerminalImage {
                width_px: 100,
                height_px: 80,
                cols: 20,
                rows: 2,
                format_label: "PNG".into(),
                lines: vec![Line::from("▀▄▀▄"), Line::from("▄▀▄▀")],
                protocol: img_preview::TerminalImageProtocol::Halfblock,
                pixel_payload: None,
                scroll_backing: None,
            },
        );

        let view = ChatRender {
            messages: std::slice::from_ref(&msg),
            is_channel: false,
            selected_index: None,
        };
        let mut out = Vec::new();
        let _ranges = build_chat_body_lines(
            &mut out,
            &view,
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            &theme,
            80,
            Some(&cache),
            thumb_dims,
            cell_px,
            false,
            Vec::new(),
        );

        let thumb_lines: Vec<_> = out
            .iter()
            .filter(|l| {
                let text: String = l.line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains("▀▄▀▄") || text.contains("click or Alt+v for HD preview")
            })
            .collect();
        assert_eq!(thumb_lines.len(), 2);
        for line in thumb_lines {
            assert_eq!(
                line.target,
                Some(HitTarget::Attachment {
                    scope: MessageScope::Chat,
                    msg_index: 0,
                    attach_index: 0,
                })
            );
        }
    }

    #[test]
    fn show_hrefs_toggle_appends_url_next_to_labeled_links_only() {
        let mut msg = sample_msg("m-link", None, 0);
        msg.attachments.clear();
        msg.reactions.clear();
        msg.body_html = r#"<p>Read <a href="https://example.com/guide">the guide</a> and <a href="https://bare.example.com">https://bare.example.com</a>.</p>"#.into();
        msg.body_plain = "Read the guide and https://bare.example.com.".into();

        let view = ChatRender {
            messages: std::slice::from_ref(&msg),
            is_channel: false,
            selected_index: None,
        };
        let theme = Theme::default();

        let mut hidden_out = Vec::new();
        build_chat_body_lines(
            &mut hidden_out,
            &view,
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            &theme,
            120,
            None,
            (0, 0),
            (10, 20),
            false,
            Vec::new(),
        );
        let hidden_text = plain_text(&hidden_out);
        assert!(hidden_text.contains("the guide"));
        assert!(!hidden_text.contains("(https://example.com/guide)"));

        let mut shown_out = Vec::new();
        build_chat_body_lines(
            &mut shown_out,
            &view,
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            &theme,
            120,
            None,
            (0, 0),
            (10, 20),
            true,
            Vec::new(),
        );
        let shown_text = plain_text(&shown_out);
        assert!(shown_text.contains("the guide (https://example.com/guide)"));
        assert!(!shown_text.contains("(https://bare.example.com)"));
    }

    #[test]
    fn adaptive_card_and_plain_md_no_raw_leakage() {
        let card_json = r#"{
            "type": "AdaptiveCard",
            "body": [
                { "type": "TextBlock", "text": "**Deploy** Notice", "size": "Large", "weight": "Bolder" },
                { "type": "TextBlock", "text": "**Alert**: [Open](https://ex.com)\nLine 2" },
                { "type": "FactSet", "facts": [{ "title": "**Env**", "value": "`prod` · [Dash](https://ex.com/dash)" }] }
            ]
        }"#;
        let card_att = AttachmentInfo {
            id: "card-md-1".into(),
            content_type: crate::parser::card_in::ADAPTIVE_CARD_CONTENT_TYPE.into(),
            content_url: None,
            name: None,
            content_json: Some(card_json.into()),
            thumbnail_url: None,
        };

        let mut card_msg = sample_msg("m-card-md", None, 0);
        card_msg.attachments = vec![card_att.clone()];
        card_msg.reactions.clear();
        card_msg.body_html = "<attachment id=\"card-md-1\"></attachment>".into();
        card_msg.body_plain = String::new();

        let mut plain_md_msg = sample_msg("m-plain-md", None, 0);
        plain_md_msg.attachments.clear();
        plain_md_msg.reactions.clear();
        plain_md_msg.body_html = "Line 1\n- **item**".into();
        plain_md_msg.body_plain = "Line 1\n- **item**".into();

        // 1. Chat view rendering: 0 raw `**`, newlines preserved for both Adaptive Card and plain-text MD
        let msgs = vec![card_msg.clone(), plain_md_msg.clone()];
        let view = ChatRender {
            messages: &msgs,
            is_channel: false,
            selected_index: None,
        };
        let rendered = build_lines_with_hyperlinks(&view, &Theme::default(), 100, false);
        let text = plain_text(&rendered);
        assert!(
            !text.contains("**"),
            "raw `**` leaked in rendered chat view:\n{text}"
        );
        assert!(text.contains("Alert: Open"), "{text}");
        assert!(text.contains("Line 2"), "{text}");
        let alert_line = rendered
            .iter()
            .position(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("Alert: Open")
            })
            .expect("expected Alert: Open line");
        let line2_line = rendered
            .iter()
            .position(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("Line 2")
            })
            .expect("expected Line 2 line");
        assert!(
            line2_line > alert_line,
            "Adaptive Card newline must place Line 2 on a separate line ({alert_line} vs {line2_line})"
        );

        let plain_l1 = rendered
            .iter()
            .position(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("Line 1")
            })
            .expect("expected Line 1");
        let plain_item = rendered
            .iter()
            .position(|l| {
                let s: String = l.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.contains("• ") && s.contains("item")
            })
            .expect("expected bullet item");
        assert!(
            plain_item > plain_l1,
            "plain-text MD newline must separate Line 1 and list item ({plain_l1} vs {plain_item})"
        );

        // 2. adaptive_card_to_html renders card Markdown to HTML via pulldown-cmark
        let (_title, card_html) =
            crate::ui::overlays::html_preview::adaptive_card_to_html(&card_att)
                .expect("adaptive_card_to_html");
        let rendered_section = card_html.split("<details>").next().unwrap_or(&card_html);
        assert!(
            rendered_section.contains("<strong>Alert</strong>"),
            "{rendered_section}"
        );
        assert!(
            rendered_section.contains("<a href=\"https://ex.com\">Open</a>"),
            "{rendered_section}"
        );
        assert!(rendered_section.contains("Line 2"), "{rendered_section}");
        assert!(!rendered_section.contains("**"), "{rendered_section}");

        // 3. Plain-text conversion also preserves newlines and leaks 0 raw `**`
        let plain_out = crate::parser::html_and_cards_to_plain("Line 1\n- **item**", &[]);
        assert_eq!(plain_out, "Line 1\n- item");
    }

    #[test]
    fn track_d_adaptive_card_and_connector_card_lossless_at_width_33() {
        let long_title = "첫째줄긴한글제목입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하\n둘째줄제목연속확인용긴문자열입니다테스트완료";
        let long_subtitle = "부제목첫줄긴한글설명입니다가나다라마바사아자차카타파하\n부제목둘째줄연속확인";
        let long_fact_val = "속성값첫줄긴한글데이터입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하\n속성값둘째줄연속확인용문자열";
        let long_body = "본문첫줄긴한글메시지입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하\n본문둘째줄연속확인용한글메시지";
        let long_action = "긴한글액션버튼라벨입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하끝";

        let adaptive_json = serde_json::json!({
            "type": "AdaptiveCard",
            "body": [
                { "type": "TextBlock", "text": long_title, "size": "Large", "weight": "Bolder" },
                { "type": "TextBlock", "text": long_subtitle, "isSubtle": true },
                { "type": "FactSet", "facts": [{ "title": "배포상태", "value": long_fact_val }] },
                { "type": "TextBlock", "text": long_body }
            ],
            "actions": [
                { "type": "Action.OpenUrl", "title": long_action, "url": "https://example.com/deploy" }
            ]
        })
        .to_string();

        let connector_json = serde_json::json!({
            "@type": "MessageCard",
            "title": long_title,
            "summary": long_subtitle,
            "sections": [{
                "facts": [{ "name": "커넥터항목", "value": long_fact_val }],
                "text": long_body
            }],
            "potentialAction": [{
                "name": long_action,
                "targets": [{ "uri": "https://example.com/logs" }]
            }]
        })
        .to_string();

        let theme = Theme::midnight_dark();

        for (kind, content_type, json) in [
            ("adaptive", crate::parser::card_in::ADAPTIVE_CARD_CONTENT_TYPE, adaptive_json),
            ("connector", crate::parser::card_in::CONNECTOR_CARD_CONTENT_TYPE, connector_json),
        ] {
            let mut msg = sample_msg("m-card", None, 0);
            msg.attachments = vec![AttachmentInfo {
                id: "card-1".into(),
                content_type: content_type.into(),
                content_url: None,
                name: None,
                content_json: Some(json),
                thumbnail_url: None,
            }];
            msg.reactions.clear();
            msg.body_html = "<attachment id=\"card-1\"></attachment>".into();
            msg.body_plain.clear();

            // Compact Medium width = 55 -> content_w = 55 - 22 = 33
            let view = ChatRender {
                messages: std::slice::from_ref(&msg),
                is_channel: false,
                selected_index: None,
            };
            let mut out = Vec::new();
            build_chat_body_lines(
                &mut out,
                &view,
                DensityMode::Compact,
                None,
                "",
                None,
                None,
                None,
                &theme,
                55,
                None,
                (0, 0),
                (10, 20),
                false,
                Vec::new(),
            );

            // Width invariant: every rendered line <= 55
            for r in &out {
                let w = line_display_width(&r.line);
                assert!(
                    w <= 55,
                    "{kind} card line exceeded width 55 (got {w}): {:?}",
                    r.line
                );
            }

            // Collect card lines (those containing `┃ `) and verify bar + card_bg on every wrapped row
            let card_lines: Vec<&RenderedLine> = out
                .iter()
                .filter(|r| r.line.spans.iter().any(|s| s.content.as_ref() == "┃ "))
                .collect();
            assert!(
                card_lines.len() >= 10,
                "{kind} card should wrap across many lines at content_w=33, got {}",
                card_lines.len()
            );
            for cl in &card_lines {
                let bar_idx = cl
                    .line
                    .spans
                    .iter()
                    .position(|s| s.content.as_ref() == "┃ ")
                    .unwrap();
                for s in &cl.line.spans[bar_idx..] {
                    assert_eq!(
                        s.style.bg,
                        Some(theme.bg_card),
                        "{kind} card wrapped row missing card_bg on span {:?}",
                        s
                    );
                }
            }

            // Reconstruct card text stripping rail and `┃ ` prefix; verify 100% lossless preservation
            let stripped: String = card_lines
                .iter()
                .map(|cl| {
                    let bar_idx = cl
                        .line
                        .spans
                        .iter()
                        .position(|s| s.content.as_ref() == "┃ ")
                        .unwrap();
                    cl.line.spans[bar_idx + 1..]
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("");

            for expected_part in [
                "첫째줄긴한글제목입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하",
                "둘째줄제목연속확인용긴문자열입니다테스트완료",
                "부제목첫줄긴한글설명입니다가나다라마바사아자차카타파하",
                "부제목둘째줄연속확인",
                "속성값첫줄긴한글데이터입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하",
                "속성값둘째줄연속확인용문자열",
                "본문첫줄긴한글메시지입니다가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하",
                "본문둘째줄연속확인용한글메시지",
                long_action,
            ] {
                assert!(
                    stripped.contains(expected_part),
                    "{kind} card lost text at width=33:\nmissing: {expected_part}\nactual:  {stripped}"
                );
            }

            // Explicit `\n` in title and body must separate lines
            let row_texts: Vec<String> = card_lines
                .iter()
                .map(|cl| cl.line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            let title_l1 = row_texts.iter().position(|t| t.contains("첫째줄긴한글제목")).unwrap();
            let title_l2 = row_texts.iter().position(|t| t.contains("둘째줄제목연속")).unwrap();
            assert!(title_l2 > title_l1, "{kind} card title newline did not split lines");
            let body_l1 = row_texts.iter().position(|t| t.contains("본문첫줄긴한글메시지")).unwrap();
            let body_l2 = row_texts.iter().position(|t| t.contains("본문둘째줄연속")).unwrap();
            assert!(body_l2 > body_l1, "{kind} card body newline did not split lines");
        }
    }

    #[test]
    fn track_d_code_block_120_chars_lossless_at_width_33() {
        let long_code = "\tlet extremely_long_identifier_for_track_d_code_wrapping_test = compute_lossless_layout_result_with_korean_comment_테스트문자열완료(1234567890);";
        let blocks = vec![RichBlock::CodeBlock {
            lang: Some("rust".into()),
            code: long_code.into(),
        }];
        let theme = Theme::midnight_dark();
        let mut out = Vec::new();
        render_rich_blocks(&mut out, &blocks, "", false, None, &theme, 33, false);

        for r in &out {
            let w = line_display_width(&r.line);
            assert!(
                w <= 33,
                "CodeBlock line exceeded width 33 (got {w}): {:?}",
                r.line
            );
        }

        let code_lines: Vec<&RenderedLine> = out
            .iter()
            .filter(|r| r.line.spans.first().is_some_and(|s| s.content.as_ref() == "│ "))
            .collect();
        assert!(
            code_lines.len() >= 4,
            "120+ char CodeBlock should wrap across >=4 lines at width 33, got {}",
            code_lines.len()
        );
        for cl in &code_lines {
            for s in &cl.line.spans {
                assert_eq!(
                    s.style.bg,
                    Some(theme.bg_code),
                    "CodeBlock wrapped row missing code_bg: {:?}",
                    s
                );
            }
        }

        let reconstructed: String = code_lines
            .iter()
            .map(|cl| {
                cl.line.spans[1..]
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<String>()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let expected: String = long_code.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(reconstructed, expected, "CodeBlock lost characters at width 33");
    }

    #[test]
    fn track_d_plain_text_three_lines_render_as_three_distinct_lines() {
        let raw_plain: crate::graph::client::GraphChatMessage = serde_json::from_value(
            serde_json::json!({
                "id": "m-plain-3",
                "createdDateTime": "2026-11-15T06:00:00Z",
                "from": { "user": { "id": "u1", "displayName": "Alice" } },
                "body": {
                    "contentType": "text",
                    "content": "첫째 줄 R&D 점검\n둘째 줄 a < b 확인\n셋째 줄 배포 완료"
                }
            }),
        )
        .unwrap();
        let msg = crate::graph::client::normalize_chat_message("chat:c1", None, None, raw_plain);
        let view = ChatRender {
            messages: std::slice::from_ref(&msg),
            is_channel: false,
            selected_index: None,
        };
        let rendered = build_lines_with_hyperlinks(&view, &Theme::default(), 80, false);
        let row_texts: Vec<String> = rendered
            .iter()
            .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let l1 = row_texts
            .iter()
            .position(|t| t.contains("첫째 줄 R&D 점검"))
            .expect("missing line 1");
        let l2 = row_texts
            .iter()
            .position(|t| t.contains("둘째 줄 a < b 확인"))
            .expect("missing line 2");
        let l3 = row_texts
            .iter()
            .position(|t| t.contains("셋째 줄 배포 완료"))
            .expect("missing line 3");
        assert!(l1 < l2 && l2 < l3, "expected 3 distinct lines, got {l1}, {l2}, {l3}:\n{row_texts:#?}");

        // Also test plain text without `<`/`&`
        let raw_simple: crate::graph::client::GraphChatMessage = serde_json::from_value(
            serde_json::json!({
                "id": "m-plain-simple",
                "createdDateTime": "2026-11-15T06:01:00Z",
                "from": { "user": { "id": "u1", "displayName": "Alice" } },
                "body": {
                    "contentType": "text",
                    "content": "Alpha line\nBeta line\nGamma line"
                }
            }),
        )
        .unwrap();
        let simple_msg =
            crate::graph::client::normalize_chat_message("chat:c1", None, None, raw_simple);
        let simple_rendered = build_lines_with_hyperlinks(
            &ChatRender {
                messages: std::slice::from_ref(&simple_msg),
                is_channel: false,
                selected_index: None,
            },
            &Theme::default(),
            80,
            false,
        );
        let simple_rows: Vec<String> = simple_rendered
            .iter()
            .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let a = simple_rows.iter().position(|t| t.contains("Alpha line")).unwrap();
        let b = simple_rows.iter().position(|t| t.contains("Beta line")).unwrap();
        let g = simple_rows.iter().position(|t| t.contains("Gamma line")).unwrap();
        assert!(a < b && b < g, "expected 3 distinct lines, got {a}, {b}, {g}");
    }

    #[test]
    fn track_d_cozy_long_korean_author_and_subject_and_compact_reactions_preserve_metadata() {
        // 1. Cozy: long Korean author preserves timestamp + [URGENT] + (edited), and subject wraps losslessly
        let long_subject = "채널공지제목긴한글텍스트입니다가나다라마바사아자차카타파하배포일정안내끝";
        let mut cozy_msg = sample_msg("m-cozy", Some(long_subject), 0);
        cozy_msg.attachments.clear();
        cozy_msg.reactions.clear();
        cozy_msg.author_name = "김철수플랫폼개발본부아키텍처팀수석엔지니어".into();
        cozy_msg.importance = "urgent".into();
        cozy_msg.last_modified_at = Some("2026-11-15T06:05:00Z".into());
        cozy_msg.body_html = "본문".into();
        cozy_msg.body_plain = "본문".into();

        let expected_time = format_time(&cozy_msg.created_at);
        let cozy_lines = build_lines_with_hyperlinks(
            &ChatRender {
                messages: std::slice::from_ref(&cozy_msg),
                is_channel: true,
                selected_index: None,
            },
            &Theme::default(),
            42,
            false,
        );
        for r in &cozy_lines {
            let w = line_display_width(&r.line);
            assert!(w <= 42, "Cozy line exceeded width 42 (got {w}): {:?}", r.line);
        }
        let header_text: String = cozy_lines
            .iter()
            .find(|r| {
                let s: String = r.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.contains("[URGENT]")
            })
            .map(|r| r.line.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .expect("expected Cozy header with [URGENT]");
        assert!(header_text.contains(&expected_time), "missing timestamp in {header_text}");
        assert!(header_text.contains("[URGENT]"), "missing [URGENT] in {header_text}");
        assert!(header_text.contains("(edited)"), "missing (edited) in {header_text}");

        let subject_joined: String = cozy_lines
            .iter()
            .filter(|r| {
                let s: String = r.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.contains("◆ ") || (s.starts_with("      ") && !s.contains("본문"))
            })
            .map(|r| {
                let s: String = r.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.trim_start_matches(' ')
                    .trim_start_matches("◆ ")
                    .to_string()
            })
            .collect();
        assert_eq!(subject_joined, long_subject, "Cozy subject lost characters");

        // 2. Compact: many reactions preserve `↳ 3` thread pill & clickable OpenThread target
        let mut compact_msg = sample_msg("m-compact", Some(long_subject), 3);
        compact_msg.attachments.clear();
        compact_msg.body_html = "체크 부탁드립니다".into();
        compact_msg.body_plain = "체크 부탁드립니다".into();
        compact_msg.reactions = ["like", "heart", "laugh", "surprised", "sad", "angry"]
            .into_iter()
            .enumerate()
            .map(|(i, rt)| ReactionSummary {
                reaction_type: rt.into(),
                count: i + 1,
                me_reacted: i == 0,
                users: vec![],
            })
            .collect();

        let mut compact_out = Vec::new();
        build_chat_body_lines(
            &mut compact_out,
            &ChatRender {
                messages: std::slice::from_ref(&compact_msg),
                is_channel: true,
                selected_index: None,
            },
            DensityMode::Compact,
            None,
            "",
            None,
            None,
            None,
            &Theme::default(),
            55, // content_w = 33
            None,
            (0, 0),
            (10, 20),
            false,
            Vec::new(),
        );
        for r in &compact_out {
            let w = line_display_width(&r.line);
            assert!(w <= 55, "Compact line exceeded width 55 (got {w}): {:?}", r.line);
        }
        let compact_joined = plain_text(&compact_out);
        assert!(
            compact_joined.contains("↳ 3"),
            "Compact mode dropped thread pill `↳ 3` with many reactions:\n{compact_joined}"
        );
        assert!(
            compact_out.iter().any(|r| r
                .inline_targets
                .iter()
                .any(|(_, _, t)| matches!(t, HitTarget::OpenThread { msg_index: 0 }))),
            "Compact mode missing HitTarget::OpenThread for `↳ 3`"
        );
        // Verify compact subject also preserved 100%
        let stripped_compact: String = compact_out
            .iter()
            .map(|r| {
                let s: String = r.line.spans.iter().map(|sp| sp.content.as_ref()).collect();
                s.split_once("│ ").map(|(_, rhs)| rhs).unwrap_or("").trim().trim_start_matches("◆ ").to_string()
            })
            .collect();
        assert!(
            stripped_compact.contains(long_subject),
            "Compact subject lost characters: {stripped_compact}"
        );
    }
}
