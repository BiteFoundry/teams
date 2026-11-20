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
    widgets::{Block, Paragraph},
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

    if app.active_conv_key.is_none() {
        ui.chat_scroll.message_ranges = Arc::from([]);
        ui.chat_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new("Select a conversation from the sidebar (or press Ctrl+K)")
                .style(ui.theme.muted_style()),
            message_area,
        );
        return;
    }

    if app.loading_messages && app.messages.is_empty() {
        ui.chat_scroll.message_ranges = Arc::from([]);
        ui.chat_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new("Loading messages…").style(ui.theme.muted_style()),
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
        empty_lines.push(RenderedLine::plain(Line::from(Span::styled(
            "No messages yet — write the first message below.",
            ui.theme.muted_style(),
        ))));
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
    let offset = ui.chat_scroll.offset(height);
    ui.chat_scroll.offset = offset;

    if ui.inline_images {
        let width = (message_area.width as usize).max(12);
        let thumb_cols = width.saturating_sub(4).clamp(16, 40) as u16;
        let thumb_rows = 10u16;
        let vis_start = offset.saturating_sub(20);
        let vis_end = offset.saturating_add(height).saturating_add(20);
        let mut to_fetch = Vec::new();
        for (msg_idx, &(r_start, r_end)) in cached.ranges.iter().enumerate() {
            if r_end > vis_start && r_start < vis_end {
                if let Some(msg) = app.messages.get(msg_idx) {
                    let eff = msg.effective_attachments();
                    for att in eff.iter() {
                        if img_preview::is_image_mime_or_name(
                            &att.content_type,
                            att.name.as_deref(),
                        ) {
                            to_fetch.push((msg.msg_id.clone(), att.clone()));
                        }
                    }
                }
            }
        }
        for (msg_id, att) in to_fetch {
            app.request_teams_image(msg_id, att, thumb_cols, thumb_rows);
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
        let is_selected = selection
            .as_ref()
            .is_some_and(|sel| global_idx >= sel.range.0 && global_idx < sel.range.1);
        if is_selected {
            let sel = selection.as_ref().unwrap();
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

    pub(crate) fn finish(self) -> (u64, u64) {
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
    h.write_hash(&theme.code_fg);
}

pub(crate) fn hash_message_sig(h: &mut DualHasher, msg: &MessageItem) {
    h.write_str(&msg.msg_id);
    h.write_str(&msg.created_at);
    match &msg.parent_msg_id {
        Some(p) => {
            h.write_hash(&1u8);
            h.write_str(p);
        }
        None => h.write_hash(&0u8),
    }
    match &msg.author_id {
        Some(aid) => {
            h.write_hash(&1u8);
            h.write_str(aid);
        }
        None => h.write_hash(&0u8),
    }
    h.write_str(&msg.author_name);
    h.write_hash(&msg.is_me);
    match &msg.last_modified_at {
        Some(ts) => {
            h.write_hash(&1u8);
            h.write_str(ts);
        }
        None => h.write_hash(&0u8),
    }
    match &msg.reply_to_id {
        Some(id) => {
            h.write_hash(&1u8);
            h.write_str(id);
        }
        None => h.write_hash(&0u8),
    }
    match &msg.reply_to_preview {
        Some(p) => {
            h.write_hash(&1u8);
            h.write_str(p);
        }
        None => h.write_hash(&0u8),
    }
    h.write_hash(&msg.deleted);
    h.write_hash(&msg.reply_count);
    h.write_hash(&msg.reactions.len());
    for r in &msg.reactions {
        h.write_str(&r.reaction_type);
        h.write_hash(&r.count);
        h.write_hash(&r.me_reacted);
    }
    h.write_hash(&msg.mentions.len());
    for m in &msg.mentions {
        h.write_str(&m.user_id);
        h.write_str(&m.display_name);
    }
    let eff_attachments = msg.effective_attachments();
    h.write_hash(&eff_attachments.len());
    for att in eff_attachments.iter() {
        h.write_str(&att.id);
        match &att.name {
            Some(name) => {
                h.write_hash(&1u8);
                h.write_str(name);
            }
            None => h.write_hash(&0u8),
        }
        h.write_str(&att.content_type);
    }
    h.write_str(&msg.body_html);
    h.write_str(&msg.body_plain);
    match &msg.subject {
        Some(subj) => {
            h.write_hash(&1u8);
            h.write_str(subj);
        }
        None => h.write_hash(&0u8),
    }
    h.write_str(&msg.importance);
}

#[allow(clippy::too_many_arguments)]
fn compute_chat_lines_sig(
    conv_key: Option<&str>,
    view: &ChatRender,
    density: DensityMode,
    unread_anchor_ts: Option<&str>,
    my_user_id: &str,
    my_display_name: Option<&str>,
    presences: Option<&HashMap<String, PresenceStatus>>,
    my_presence: Option<&PresenceStatus>,
    width: usize,
    hyperlinks: bool,
    show_hrefs: bool,
    inline_images: bool,
    image_cache_gen: u64,
    loading_older: bool,
    oldest_reached: bool,
    history_error: Option<&str>,
    theme: &Theme,
) -> (u64, u64) {
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
    h.write_hash(&loading_older);
    h.write_hash(&oldest_reached);
    match history_error {
        Some(err) => {
            h.write_hash(&1u8);
            h.write_str(err);
        }
        None => h.write_hash(&0u8),
    }
    if let Some(mp) = my_presence {
        h.write_hash(&1u8);
        h.write_str(&mp.availability);
    } else {
        h.write_hash(&0u8);
    }
    hash_theme_sig(&mut h, theme);
    h.write_hash(&view.messages.len());
    for msg in view.messages {
        hash_message_sig(&mut h, msg);
        if let (Some(map), Some(aid)) = (presences, msg.author_id.as_deref()) {
            if let Some(p) = map.get(aid) {
                h.write_hash(&1u8);
                h.write_str(&p.availability);
            } else {
                h.write_hash(&0u8);
            }
        }
    }
    h.finish()
}

thread_local! {
    static CHAT_LINES_CACHE: RefCell<LineSliceCache> = RefCell::new(LineSliceCache::new(4));
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
    build_lines_with_images_cached(app, ui, width).to_vec()
}

pub(crate) fn build_lines_with_images_cached(
    app: &mut AppState,
    ui: &UiState,
    width: usize,
) -> Arc<[RenderedLine]> {
    build_stream_cached(app, ui, width).lines
}

pub(crate) fn build_stream_cached(
    app: &mut AppState,
    ui: &UiState,
    width: usize,
) -> CachedStream {
    let hyperlinks = hyperlinks_enabled();
    let width = width.max(12);
    let mut view = ChatRender::from_app(app, ui);
    view.selected_index = None;
    let active_unread_ts = match (&ui.unread_anchor, app.active_conv_key.as_deref()) {
        (Some((conv, ts)), Some(active)) if conv == active => Some(ts.as_str()),
        _ => None,
    };

    let sig = {
        let my_display_name = app.my_profile.as_ref().map(|p| p.display_name.as_str());
        compute_chat_lines_sig(
            app.active_conv_key.as_deref(),
            &view,
            ui.density,
            active_unread_ts,
            &app.my_user_id,
            my_display_name,
            Some(&app.presences),
            app.my_presence.as_ref(),
            width,
            hyperlinks,
            ui.show_hrefs,
            ui.inline_images,
            app.image_cache_gen,
            app.loading_older,
            app.oldest_reached,
            app.history_error.as_deref(),
            &ui.theme,
        )
    };

    if let Some(cached) = CHAT_LINES_CACHE.with(|cache| cache.borrow_mut().get(sig)) {
        return cached;
    }

    let thumb_cols = width.saturating_sub(4).clamp(16, 40) as u16;
    let thumb_rows = 10u16;

    let mut view = ChatRender::from_app(app, ui);
    view.selected_index = None;
    let my_display_name = app.my_profile.as_ref().map(|p| p.display_name.as_str());
    let mut out = Vec::new();
    if app.active_conv_key.is_some() {
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
        ui.show_hrefs,
    );

    let cached = CachedStream {
        lines: Arc::from(out),
        ranges: Arc::from(ranges),
    };
    CHAT_LINES_CACHE.with(|cache| cache.borrow_mut().insert(sig, cached.clone()));
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
        false,
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
    show_hrefs: bool,
) -> Vec<(usize, usize)> {
    let first_unread = find_first_unread(view.messages, unread_anchor_ts);
    let mut last_date_key: Option<String> = None;
    let mut ranges = Vec::with_capacity(view.messages.len());

    for (msg_index, msg) in view.messages.iter().enumerate() {
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
        let mut header_spans = vec![
            Span::raw("  "),
            Span::styled(
                format!("{dot} "),
                if theme.no_color {
                    Style::default()
                } else {
                    Style::default().fg(dot_color)
                },
            ),
            Span::styled(msg.author_name.clone(), author_style),
            Span::styled(format!("  {time}"), theme.muted_style()),
        ];
        if msg.importance.eq_ignore_ascii_case("urgent") {
            header_spans.push(Span::styled(
                " [URGENT]",
                Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
            ));
        } else if msg.importance.eq_ignore_ascii_case("high") {
            header_spans.push(Span::styled(
                " [IMPORTANT]",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if msg.last_modified_at.is_some() && !msg.deleted {
            header_spans.push(Span::styled(" (edited)", theme.muted_style()));
        }
        out.push(RenderedLine::clickable(Line::from(header_spans), target));
    }

    // Channel root post subject renders BELOW the author header (indented 4 cols)
    if let Some(subject) = msg.subject.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(RenderedLine::clickable(
            Line::from(vec![
                Span::raw("  "),
                Span::styled("  ◆ ", theme.subject_style()),
                Span::styled(
                    truncate(subject, width.saturating_sub(6)),
                    theme.subject_style(),
                ),
            ]),
            target,
        ));
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
        let banner = format!("  {prefix}{}\"", truncate(&preview, avail));
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
        show_hrefs: ctx.show_hrefs,
    };
    render_message_body_and_attachments(out, msg, &body_ctx);

    // Reaction badges row (`    [👍 2] [❤️ 1]`)
    if !msg.reactions.is_empty() {
        let mut spans = vec![Span::raw("    ")];
        let mut inline_targets = Vec::new();
        let mut col = 4u16;
        for (r_idx, r) in msg.reactions.iter().enumerate() {
            let emoji = reaction_emoji(&r.reaction_type);
            let badge = format!("[{emoji} {}]", r.count);
            let badge_w = UnicodeWidthStr::width(badge.as_str()) as u16;
            if (col as usize) + (badge_w as usize) > width {
                break;
            }
            spans.push(Span::styled(badge, theme.reaction_style(r.me_reacted)));
            spans.push(Span::raw(" "));
            inline_targets.push((
                col,
                badge_w,
                HitTarget::Reaction {
                    scope: ctx.scope,
                    msg_index,
                    reaction_index: r_idx,
                },
            ));
            col += badge_w + 1;
        }
        out.push(RenderedLine::with_inline(
            Line::from(spans),
            Some(target),
            inline_targets,
        ));
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
            content_lines.push(RenderedLine::plain(Line::from(vec![
                Span::styled("◆ ", theme.subject_style()),
                Span::styled(
                    truncate(subject, content_w.saturating_sub(2)),
                    theme.subject_style(),
                ),
            ])));
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

        // Compact suffix: `[👍2] [❤️1] · ↳ 4`
        let mut suffix_spans: Vec<Span<'static>> = Vec::new();
        let mut suffix_targets: Vec<(u16, u16, HitTarget)> = Vec::new();
        let mut scol = 0u16;

        for (r_idx, r) in msg.reactions.iter().enumerate() {
            let emoji = reaction_emoji(&r.reaction_type);
            let badge = format!("[{emoji}{}]", r.count);
            let bw = UnicodeWidthStr::width(badge.as_str()) as u16;
            if (scol as usize) + (bw as usize) > content_w {
                break;
            }
            if r_idx > 0 {
                suffix_spans.push(Span::raw(" "));
                scol += 1;
            }
            suffix_spans.push(Span::styled(badge, theme.reaction_style(r.me_reacted)));
            suffix_targets.push((
                scol,
                bw,
                HitTarget::Reaction {
                    scope: ctx.scope,
                    msg_index,
                    reaction_index: r_idx,
                },
            ));
            scol += bw;
        }

        if ctx.is_channel
            && ctx.scope == MessageScope::Chat
            && msg.parent_msg_id.is_none()
            && msg.reply_count > 0
        {
            let pill = format!("↳ {}", msg.reply_count);
            let pw = UnicodeWidthStr::width(pill.as_str()) as u16;
            let sep_w = if suffix_spans.is_empty() { 0u16 } else { 3u16 };
            if (scol as usize) + (sep_w as usize) + (pw as usize) <= content_w {
                if sep_w > 0 {
                    suffix_spans.push(Span::styled(" · ", theme.muted_style()));
                    scol += sep_w;
                }
                suffix_spans.push(Span::styled(pill, theme.accent_style()));
                suffix_targets.push((scol, pw, HitTarget::OpenThread { msg_index }));
                scol += pw;
            }
        }

        if scol > 0 {
            let mut appended = false;
            if last_is_appendable {
                if let Some(last) = content_lines.last_mut() {
                    let lw = line_display_width(&last.line);
                    if lw + 2 + (scol as usize) <= content_w {
                        last.line.spans.push(Span::raw("  "));
                        last.line.spans.extend(suffix_spans.clone());
                        let base_col = (lw + 2) as u16;
                        for (c, w, t) in &suffix_targets {
                            last.inline_targets.push((base_col + *c, *w, *t));
                        }
                        appended = true;
                    }
                }
            }
            if !appended {
                content_lines.push(RenderedLine::with_inline(
                    Line::from(suffix_spans),
                    Some(target),
                    suffix_targets,
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
    show_hrefs: bool,
}

/// Renders parsed `RichBlock`s + attachments + thumbnails.
/// Returns `true` if the last emitted line is plain body text suitable for inline suffixing.
fn render_message_body_and_attachments(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    bctx: &BodyBlockCtx<'_>,
) -> bool {
    let eff_attachments = msg.effective_attachments();
    let attachments = eff_attachments.as_ref();
    let parsed_blocks =
        parser::parse_message_blocks_arc(&msg.body_html, attachments, &msg.mentions);
    let fallback_blocks;
    let blocks: &[RichBlock] = if parsed_blocks.is_empty() && !msg.body_plain.trim().is_empty() {
        fallback_blocks = [RichBlock::Paragraph(vec![RichSpan::plain(&msg.body_plain)])];
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
                let tag = if is_img {
                    "IMG "
                } else if is_html {
                    "HTML "
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
        let tag = if is_img {
            "IMG "
        } else if is_html {
            "HTML "
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
    indent: &str,
    width: usize,
    theme: &Theme,
    scope: MessageScope,
    msg_index: usize,
    attach_idx: usize,
) {
    let key = AppState::image_cache_key(msg_id, &att.id, thumb_dims.0, thumb_dims.1);
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
            let trail = usize::from(cur_w + 2 + chips_w + 1 <= width);
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
                            format!("{indent}  ")
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

                for raw_line in code.lines() {
                    let expanded = raw_line.replace('\t', "    ");
                    let content = truncate(&expanded, body_w.saturating_sub(2));
                    let content_w = UnicodeWidthStr::width(content.as_str());
                    let mut code_spans = Vec::new();
                    if !indent.is_empty() {
                        code_spans.push(Span::raw(indent.to_string()));
                    }
                    code_spans.push(Span::styled("│ ", header_style));
                    code_spans.push(Span::styled(content, theme.code_style()));
                    if let Some(bg) = code_bg {
                        let used = 2 + content_w;
                        if used < body_w {
                            code_spans.push(Span::styled(
                                " ".repeat(body_w - used),
                                Style::default().bg(bg),
                            ));
                        }
                    }
                    push_line(Line::from(code_spans));
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
                let push_card_line = |push: &mut dyn FnMut(Line<'static>), text_span: Span<'static>| {
                    let text_w = UnicodeWidthStr::width(text_span.content.as_ref());
                    let mut spans = Vec::new();
                    if !indent.is_empty() {
                        spans.push(Span::raw(indent.to_string()));
                    }
                    spans.push(Span::styled("┃ ", bar_style));
                    spans.push(text_span);
                    if let Some(bg) = card_bg {
                        let used = 2 + text_w;
                        if used < body_w {
                            spans.push(Span::styled(
                                " ".repeat(body_w - used),
                                Style::default().bg(bg),
                            ));
                        }
                    }
                    push(Line::from(spans));
                };

                let card_title = title.as_deref().unwrap_or("Adaptive Card");
                let mut t_style = Style::default().add_modifier(Modifier::BOLD);
                if let Some(bg) = card_bg {
                    t_style = t_style.fg(theme.fg_primary).bg(bg);
                }
                push_card_line(
                    &mut push_line,
                    Span::styled(truncate(card_title, body_w.saturating_sub(2)), t_style),
                );

                if let Some(sub) = subtitle {
                    let s_style = match card_bg {
                        Some(bg) => Style::default().fg(theme.fg_muted).bg(bg),
                        None => theme.muted_style(),
                    };
                    push_card_line(
                        &mut push_line,
                        Span::styled(truncate(sub, body_w.saturating_sub(2)), s_style),
                    );
                }
                for (k, v) in facts {
                    let text = format!("{k}: {v}");
                    let f_style = match card_bg {
                        Some(bg) => Style::default().fg(theme.fg_secondary).bg(bg),
                        None => Style::default(),
                    };
                    push_card_line(
                        &mut push_line,
                        Span::styled(truncate(&text, body_w.saturating_sub(2)), f_style),
                    );
                }
                for bl in body_lines {
                    let b_style = match card_bg {
                        Some(bg) => Style::default().fg(theme.fg_primary).bg(bg),
                        None => Style::default(),
                    };
                    push_card_line(
                        &mut push_line,
                        Span::styled(truncate(bl, body_w.saturating_sub(2)), b_style),
                    );
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
                    push_card_line(
                        &mut push_line,
                        Span::styled(truncate(&link_label, body_w.saturating_sub(2)), a_style),
                    );
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
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    let mut push_wrapped_text = |text: &str, style: Style| {
        let mut buf = String::new();
        for ch in text.chars() {
            if ch == '\n' {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), style));
                }
                lines.push(std::mem::take(&mut current));
                used = 0;
                continue;
            }
            if ch == '\t' {
                for _ in 0..4 {
                    if used + 1 > limit && (used > 0 || !buf.is_empty()) {
                        if !buf.is_empty() {
                            current.push(Span::styled(std::mem::take(&mut buf), style));
                        }
                        lines.push(std::mem::take(&mut current));
                        used = 0;
                    }
                    buf.push(' ');
                    used += 1;
                }
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > limit && (used > 0 || !buf.is_empty()) {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), style));
                }
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            buf.push(ch);
            used += cw;
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, style));
        }
    };

    for piece in rich {
        let style = span_style(piece, theme, my_user_id, my_display_name, mentions);
        push_wrapped_text(&piece.text, style);

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
                    let href_suffix = format!(" ({url})");
                    push_wrapped_text(&href_suffix, theme.muted_style());
                }
            }
        }
    }

    lines.push(current);
    lines
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
            false,
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
            false,
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
            messages: &vec![m1],
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
            false,
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

    #[test]
    fn chat_render_signature_and_line_cache_behavior() {
        let mut msgs = vec![sample_msg("m1", Some("Weekly Report"), 3)];
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
                DensityMode::Cozy,
                None,
                "",
                None,
                None,
                None,
                80,
                false,
                false,
                true,
                0,
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
        cache.insert(sig_base, cached_val.clone());
        let hit = cache.get(sig_base).expect("expected cache hit");
        assert_eq!(hit.lines.len(), lines.len());

        let sig_sel = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: None,
            },
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            80,
            false,
            false,
            true,
            0,
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
            DensityMode::Compact,
            None,
            "",
            None,
            None,
            None,
            80,
            false,
            false,
            true,
            0,
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
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            100,
            false,
            false,
            true,
            0,
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_width);

        msgs[0].reactions[0].me_reacted = false;
        let sig_react = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            80,
            false,
            false,
            true,
            0,
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_react);

        cache.insert(sig_compact, cached_val.clone());
        cache.insert(sig_width, cached_val);
        assert!(cache.get(sig_base).is_none());
        assert!(cache.get(sig_width).is_some());

        // Changing body_plain or attachment names invalidates the message hash.
        let orig_msg = msgs[0].clone();
        msgs[0].body_plain.push('!');
        let sig_body = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: None,
            },
            DensityMode::Cozy,
            None,
            "",
            None,
            None,
            None,
            80,
            false,
            false,
            true,
            0,
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_body);
        msgs[0] = orig_msg;
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

        let thumb_dims = (24u16, 10u16);
        let cache_key = AppState::image_cache_key("m1", "hosted:1", thumb_dims.0, thumb_dims.1);
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
        let theme = Theme::default();
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
            false,
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
            false,
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
            true,
        );
        let shown_text = plain_text(&shown_out);
        assert!(shown_text.contains("the guide (https://example.com/guide)"));
        assert!(!shown_text.contains("(https://bare.example.com)"));
    }
}
