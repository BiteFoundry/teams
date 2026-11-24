//! Multiline Markdown composer: UTF-8 char-boundary cursor/selection/undo/redo,
//! optional Channel Subject field (`Ctrl+S`), `@mention` autocomplete, quoted reply,
//! clipboard image paste (RGBA -> PNG `InlineImageUpload`), and external `$EDITOR`.

use std::{ops::Range, process::Command};

use image::{codecs::png::PngEncoder, ExtendedColorType, ImageBuffer, ImageEncoder, Rgba};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    img_preview::{self, RenderedTerminalImage},
    models::{ConversationKind, InlineImageUpload, MemberInfo, MentionInfo},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::mentions::MentionPopup,
    },
};

const MAX_UNDO: usize = 200;
pub const SUBJECT_PREFIX: &str = "Subject: ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyPreview {
    pub msg_id: String,
    pub author: String,
    pub text: String,
}

impl ReplyPreview {
    pub fn summary(&self, width: usize) -> String {
        truncate_display(&self.text.replace('\n', " "), width)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionRef {
    pub user_id: String,
    pub display_name: String,
    pub start: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    text: String,
    cursor: usize,
    mentions: Vec<MentionRef>,
}

#[derive(Debug, Clone)]
pub struct ComposerState {
    text: String,
    cursor: usize,
    pub subject: String,
    pub subject_cursor: usize,
    pub subject_focused: bool,
    pub reply: Option<ReplyPreview>,
    mentions: Vec<MentionRef>,
    pub pending_images: Vec<InlineImageUpload>,
    pub pending_preview: Option<RenderedTerminalImage>,
    pub last_inner_width: u16,
    anchor: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    pub popup: MentionPopup,
}

impl Default for ComposerState {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            subject: String::new(),
            subject_cursor: 0,
            subject_focused: false,
            reply: None,
            mentions: Vec::new(),
            pending_images: Vec::new(),
            pending_preview: None,
            last_inner_width: 72,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            popup: MentionPopup::default(),
        }
    }
}

impl ComposerState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.pending_images.is_empty()
    }

    /// True when the composer text buffer itself has zero characters (used for empty-composer arrow scroll).
    pub fn has_no_text(&self) -> bool {
        self.text.is_empty()
    }

    pub fn reply(&self) -> Option<&ReplyPreview> {
        self.reply.as_ref()
    }

    pub fn mentions(&self) -> &[MentionRef] {
        &self.mentions
    }

    pub fn compiled_mentions(&self) -> Vec<MentionInfo> {
        self.mentions
            .iter()
            .enumerate()
            .map(|(idx, m)| MentionInfo {
                id: idx as i32,
                user_id: m.user_id.clone(),
                display_name: m.display_name.clone(),
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Editing
    // ------------------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\r' {
            return;
        }
        if self.subject_focused {
            if ch != '\n' {
                let pos = self.subject_cursor.min(self.subject.len());
                self.subject.insert(pos, ch);
                self.subject_cursor = pos + ch.len_utf8();
            }
            return;
        }
        let mut buf = [0u8; 4];
        self.insert_replacing_selection(ch.encode_utf8(&mut buf));
    }

    pub fn insert_str(&mut self, value: &str) {
        let value = normalize(value);
        if value.is_empty() {
            return;
        }
        if self.subject_focused {
            let single = value.replace('\n', " ");
            let pos = self.subject_cursor.min(self.subject.len());
            self.subject.insert_str(pos, &single);
            self.subject_cursor = pos + single.len();
            return;
        }
        self.insert_replacing_selection(&value);
    }

    fn insert_replacing_selection(&mut self, value: &str) {
        if let Some((start, end)) = self.selection_range() {
            self.record();
            let start_char = self.text[..start].chars().count();
            let removed_chars = self.text[start..end].chars().count();
            let inserted_chars = value.chars().count();
            self.text.replace_range(start..end, value);
            self.cursor = start + value.len();
            self.anchor = None;
            self.adjust_mentions(start_char, removed_chars, inserted_chars);
        } else {
            self.insert_at(self.cursor, value);
        }
    }

    pub fn newline(&mut self) {
        if self.subject_focused {
            self.subject_focused = false;
            return;
        }
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.subject_focused {
            if self.subject_cursor == 0 {
                return;
            }
            if let Some(prev) = prev_boundary(&self.subject, self.subject_cursor) {
                self.subject.replace_range(prev..self.subject_cursor, "");
                self.subject_cursor = prev;
            }
            return;
        }
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let Some(prev) = prev_boundary(&self.text, self.cursor) else {
            return;
        };
        self.delete_range(prev..self.cursor);
    }

    pub fn delete_forward(&mut self) {
        if self.subject_focused {
            if let Some(next) = next_boundary(&self.subject, self.subject_cursor) {
                self.subject.replace_range(self.subject_cursor..next, "");
            }
            return;
        }
        if self.delete_selection() {
            return;
        }
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.delete_range(self.cursor..next);
        }
    }

    pub fn delete_word_backward(&mut self) {
        if self.subject_focused {
            if self.subject_cursor == 0 {
                return;
            }
            let head = &self.subject[..self.subject_cursor.min(self.subject.len())];
            let mut idx = head.len();
            while let Some((pos, ch)) = head[..idx].char_indices().next_back() {
                if ch.is_whitespace() {
                    idx = pos;
                } else {
                    break;
                }
            }
            while let Some((pos, ch)) = head[..idx].char_indices().next_back() {
                if !ch.is_whitespace() {
                    idx = pos;
                } else {
                    break;
                }
            }
            self.subject.replace_range(idx..self.subject_cursor, "");
            self.subject_cursor = idx;
            return;
        }
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor.min(self.text.len());
        if pos == 0 {
            return;
        }
        let head = &self.text[..pos];
        let mut idx = head.len();
        while let Some((p, ch)) = head[..idx].char_indices().next_back() {
            if ch.is_whitespace() && ch != '\n' {
                idx = p;
            } else {
                break;
            }
        }
        if idx == head.len() {
            if let Some((p, '\n')) = head.char_indices().next_back() {
                self.delete_range(p..pos);
                return;
            }
        }
        while let Some((p, ch)) = head[..idx].char_indices().next_back() {
            if !ch.is_whitespace() {
                idx = p;
            } else {
                break;
            }
        }
        if idx < pos {
            self.delete_range(idx..pos);
        }
    }

    pub fn clear(&mut self) {
        if self.text.is_empty()
            && self.subject.is_empty()
            && self.mentions.is_empty()
            && self.pending_images.is_empty()
        {
            return;
        }
        self.record();
        self.text.clear();
        self.cursor = 0;
        self.subject.clear();
        self.subject_cursor = 0;
        self.subject_focused = false;
        self.anchor = None;
        self.mentions.clear();
        self.pending_images.clear();
        self.pending_preview = None;
        self.popup.close();
    }

    pub fn stage_inline_image(&mut self, bytes: Vec<u8>, mime_type: &str, file_name: Option<String>) {
        let preview = img_preview::decode_and_render_halfblocks(&bytes, 24, 5, (20, 22, 26)).ok();
        self.stage_inline_image_with_preview(bytes, mime_type, file_name, preview);
    }

    pub fn stage_inline_image_with_preview(
        &mut self,
        bytes: Vec<u8>,
        mime_type: &str,
        file_name: Option<String>,
        preview: Option<RenderedTerminalImage>,
    ) {
        self.pending_preview = preview;
        let temp_id = (self.pending_images.len() + 1).to_string();
        self.pending_images.push(InlineImageUpload {
            temp_id,
            mime_type: mime_type.to_string(),
            bytes,
            file_name,
        });
    }

    pub fn clear_pending_images(&mut self) {
        self.pending_images.clear();
        self.pending_preview = None;
    }

    pub fn reset_after_send(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.subject.clear();
        self.subject_cursor = 0;
        self.subject_focused = false;
        self.anchor = None;
        self.mentions.clear();
        self.pending_images.clear();
        self.pending_preview = None;
        self.reply = None;
        self.popup.close();
        self.undo.clear();
        self.redo.clear();
    }

    pub fn load_text(&mut self, value: &str) {
        self.text = normalize(value);
        self.cursor = self.text.len();
        self.subject.clear();
        self.subject_cursor = 0;
        self.subject_focused = false;
        self.anchor = None;
        self.mentions.clear();
        self.pending_images.clear();
        self.pending_preview = None;
        self.popup.close();
        self.undo.clear();
        self.redo.clear();
    }

    fn insert_at(&mut self, pos: usize, value: &str) {
        if value.is_empty() || pos > self.text.len() || !self.text.is_char_boundary(pos) {
            return;
        }
        self.record();
        let start_char = self.text[..pos].chars().count();
        let inserted_chars = value.chars().count();
        self.text.insert_str(pos, value);
        self.cursor = pos + value.len();
        self.anchor = None;
        self.adjust_mentions(start_char, 0, inserted_chars);
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        self.record();
        let start_char = self.text[..range.start].chars().count();
        let removed_chars = self.text[range.clone()].chars().count();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = None;
        self.adjust_mentions(start_char, removed_chars, 0);
    }

    fn adjust_mentions(&mut self, start_char: usize, removed: usize, inserted: usize) {
        if removed == 0 && inserted == 0 {
            return;
        }
        let end_char = start_char + removed;
        let mut kept = Vec::with_capacity(self.mentions.len());

        for mention in &self.mentions {
            let m_start = mention.start.saturating_sub(1);
            let m_end = m_start + mention.len;

            if removed == 0 {
                let mut next = mention.clone();
                if start_char <= m_start {
                    next.start += inserted;
                    kept.push(next);
                } else if start_char < m_end {
                    continue;
                } else {
                    kept.push(next);
                }
                continue;
            }

            if m_start < end_char && m_end > start_char {
                continue;
            }
            let mut next = mention.clone();
            if m_start >= end_char {
                let shifted = next.start as isize - removed as isize + inserted as isize;
                next.start = shifted.max(1) as usize;
            } else if start_char < m_start {
                let shifted = next.start as isize + inserted as isize;
                next.start = shifted.max(1) as usize;
            }
            kept.push(next);
        }

        self.mentions = kept;
    }

    // ------------------------------------------------------------------
    // Selection / Cursor
    // ------------------------------------------------------------------

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub fn selected_text(&self) -> Option<&str> {
        let (start, end) = self.selection_range()?;
        self.text.get(start..end)
    }

    pub fn select_all(&mut self) {
        if self.text.is_empty() {
            self.anchor = None;
            return;
        }
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    pub fn delete_selection(&mut self) -> bool {
        match self.selection_range() {
            Some((start, end)) => {
                self.delete_range(start..end);
                true
            }
            None => false,
        }
    }

    pub fn move_left(&mut self, extend: bool) {
        if self.subject_focused {
            if let Some(prev) = prev_boundary(&self.subject, self.subject_cursor) {
                self.subject_cursor = prev;
            }
            return;
        }
        if !extend {
            if let Some((start, _)) = self.selection_range() {
                self.anchor = None;
                self.cursor = start;
                return;
            }
        }
        if let Some(prev) = prev_boundary(&self.text, self.cursor) {
            self.move_to(prev, extend);
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        if self.subject_focused {
            if let Some(next) = next_boundary(&self.subject, self.subject_cursor) {
                self.subject_cursor = next;
            }
            return;
        }
        if !extend {
            if let Some((_, end)) = self.selection_range() {
                self.anchor = None;
                self.cursor = end;
                return;
            }
        }
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.move_to(next, extend);
        }
    }

    pub fn move_up(&mut self, extend: bool) {
        if self.subject_focused {
            return;
        }
        let (line, col) = line_col(&self.text, self.cursor);
        let ranges = line_ranges(&self.text);
        if line == 0 {
            self.move_to(0, extend);
            return;
        }
        let (start, end) = ranges[line - 1];
        let target = byte_at_col(&self.text, start, end, col);
        self.move_to(target, extend);
    }

    pub fn move_down(&mut self, extend: bool) {
        if self.subject_focused {
            self.subject_focused = false;
            return;
        }
        let (line, col) = line_col(&self.text, self.cursor);
        let ranges = line_ranges(&self.text);
        if line + 1 >= ranges.len() {
            self.move_to(self.text.len(), extend);
            return;
        }
        let (start, end) = ranges[line + 1];
        let target = byte_at_col(&self.text, start, end, col);
        self.move_to(target, extend);
    }

    pub fn move_home(&mut self, extend: bool) {
        if self.subject_focused {
            self.subject_cursor = 0;
            return;
        }
        let (line, _) = line_col(&self.text, self.cursor);
        let (start, _) = line_ranges(&self.text)[line];
        self.move_to(start, extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        if self.subject_focused {
            self.subject_cursor = self.subject.len();
            return;
        }
        let (line, _) = line_col(&self.text, self.cursor);
        let (_, end) = line_ranges(&self.text)[line];
        self.move_to(end, extend);
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        if self.subject_focused {
            self.subject_cursor = 0;
            return;
        }
        self.move_to(0, extend);
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        if self.subject_focused {
            self.subject_cursor = self.subject.len();
            return;
        }
        self.move_to(self.text.len(), extend);
    }

    fn move_to(&mut self, pos: usize, extend: bool) {
        let pos = pos.min(self.text.len());
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
    }

    // ------------------------------------------------------------------
    // Undo / Redo
    // ------------------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            mentions: self.mentions.clone(),
        }
    }

    fn record(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.popup.close();
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(next);
        true
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
        self.mentions = snapshot.mentions;
        self.anchor = None;
        self.popup.close();
    }

    // ------------------------------------------------------------------
    // Reply & Mentions
    // ------------------------------------------------------------------

    pub fn clear_reply(&mut self) {
        self.reply = None;
    }

    pub fn set_reply(&mut self, reply: ReplyPreview) {
        self.reply = Some(reply);
    }

    pub fn reference_mentions(&mut self, members: &[MemberInfo]) {
        if self.subject_focused {
            self.popup.close();
            return;
        }
        self.popup.refresh(&self.text, self.cursor, members);
    }

    pub fn mention_active(&self) -> bool {
        self.popup.active
    }

    pub fn mention_next(&mut self) {
        self.popup.next();
    }

    pub fn mention_prev(&mut self) {
        self.popup.prev();
    }

    pub fn close_mentions(&mut self) {
        self.popup.close();
    }

    pub fn accept_mention(&mut self) -> bool {
        if !self.popup.active {
            return false;
        }
        let Some(candidate) = self.popup.current().cloned() else {
            self.popup.close();
            return false;
        };
        let start = self.popup.start;
        let end = self.cursor;
        if start > end
            || end > self.text.len()
            || !self.text.is_char_boundary(start)
            || !self.text.is_char_boundary(end)
        {
            self.popup.close();
            return false;
        }

        let insert = format!("@{} ", candidate.display_name);
        let start_char = self.text[..start].chars().count();
        let removed_chars = self.text[start..end].chars().count();

        self.record();
        self.text.replace_range(start..end, &insert);
        self.cursor = start + insert.len();
        self.anchor = None;
        self.adjust_mentions(start_char, removed_chars, insert.chars().count());
        self.mentions.push(MentionRef {
            user_id: candidate.user_id,
            display_name: candidate.display_name.clone(),
            start: start_char + 1,
            len: candidate.display_name.chars().count(),
        });
        self.popup.close();
        true
    }

    // ------------------------------------------------------------------
    // Send / Display
    // ------------------------------------------------------------------

    pub fn text_to_send(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(self.text.trim_end().to_string())
    }

    pub fn subject_to_send(&self) -> Option<String> {
        let s = self.subject.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }

    pub fn desired_height(&self, show_subject: bool, inline_images: bool) -> u16 {
        let mut extra = 0u16;
        if show_subject {
            extra += 1;
        }
        if self.reply.is_some() && show_subject {
            extra += 1;
        }
        if !self.pending_images.is_empty() {
            extra += 1;
            if inline_images {
                if let Some(ref preview) = self.pending_preview {
                    extra = extra.saturating_add(preview.rows.min(4));
                }
            }
        }
        let wrap_width = self.last_inner_width.max(1) as usize;
        let (visual_lines, _, _) = text_layout(&self.text, wrap_width, self.cursor);
        let body_lines = (visual_lines.len() as u16).clamp(1, 8);
        (1 + extra + body_lines).clamp(2, 13)
    }
}

/// Encodes raw RGBA8 pixel buffer into PNG bytes.
pub fn encode_rgba_to_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("Clipboard has 0x0 image data".to_string());
    }
    let img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)
        .ok_or_else(|| "Clipboard image RGBA buffer size mismatch".to_string())?;
    let mut out = Vec::new();
    let encoder = PngEncoder::new(&mut out);
    encoder
        .write_image(img.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| format!("Failed to encode clipboard image as PNG: {err}"))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let focused = ui.focus == Focus::Composer && ui.overlays.is_empty();
    let theme = ui.theme.clone();
    let surface_style = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_composer).fg(theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(surface_style), area);
    let is_thread_composer = ui.thread_drawer_open
        && ui.composer_target_thread
        && app.active_thread.is_some()
        && ui.term_width >= 120
        && ui.thread_composer_area == area
        && ui.thread_composer_area.width > 0;
    if is_thread_composer {
        hitmap.add(area, HitTarget::ThreadComposer);
    } else {
        hitmap.add(area, HitTarget::Composer);
    }

    let is_channel_root = is_composing_channel_root(app, ui);
    let show_subject = is_channel_root && ui.show_subject;

    // Row 0: top hairline (`─`) with embedded context label + clickable [Subject] / [+File] pills
    let hairline_style = if theme.no_color {
        if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    } else if focused {
        Style::default().fg(theme.accent).bg(theme.bg_composer)
    } else {
        Style::default().fg(theme.fg_subtle).bg(theme.bg_composer)
    };
    let pill_style = if theme.no_color {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default().fg(theme.accent_soft).bg(theme.bg_composer)
    };
    draw_composer_hairline(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        ui,
        app,
        show_subject,
        is_channel_root,
        hairline_style,
        pill_style,
        hitmap,
    );

    let mut y = area.y.saturating_add(1);
    let mut subject_y: Option<u16> = None;
    let mut subject_cursor_col: u16 = 0;

    if show_subject && y < area.bottom() {
        subject_y = Some(y);
        let prefix_w = UnicodeWidthStr::width(SUBJECT_PREFIX);
        let subj_style = if focused && ui.composer.subject_focused {
            theme.subject_style()
        } else {
            theme.accent_style()
        };
        let max_body = (area.width as usize).saturating_sub(prefix_w);
        let (visible_body, cursor_col) = if ui.composer.subject.is_empty() {
            (
                truncate_display("(Post subject · Ctrl+S to hide)", max_body),
                0usize,
            )
        } else {
            slice_display_window(&ui.composer.subject, ui.composer.subject_cursor, max_body)
        };
        subject_cursor_col = cursor_col as u16;
        let body_style = if ui.composer.subject.is_empty() {
            theme.muted_style().add_modifier(Modifier::ITALIC)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(SUBJECT_PREFIX, subj_style),
                Span::styled(visible_body, body_style),
            ]))
            .style(surface_style),
            Rect::new(area.x, y, area.width, 1),
        );
        hitmap.add_line(area.x, y, area.width, HitTarget::SubjectInput);
        y = y.saturating_add(1);
    }

    if show_subject {
        if let Some(reply) = ui.composer.reply() {
            if y < area.bottom() {
                let text = format!(
                    "↩ Replying to {}: \"{}\" · Esc cancel",
                    reply.author,
                    reply.summary(area.width.saturating_sub(32).max(6) as usize)
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate_display(&text, area.width as usize),
                        theme.quote_style(),
                    )))
                    .style(surface_style),
                    Rect::new(area.x, y, area.width, 1),
                );
                y = y.saturating_add(1);
            }
        }
    }

    if !ui.composer.pending_images.is_empty() && y < area.bottom() {
        let count = ui.composer.pending_images.len();
        let chip = if let Some(ref preview) = ui.composer.pending_preview {
            if count == 1 {
                format!(
                    "[Clipboard image ({}x{}) — uploaded inline on send]",
                    preview.width_px, preview.height_px
                )
            } else {
                format!(
                    "[{count} clipboard images ({}x{}) — uploaded inline on send]",
                    preview.width_px, preview.height_px
                )
            }
        } else if count == 1 {
            "[Clipboard image attached — uploaded inline on send]".to_string()
        } else {
            format!("[{count} clipboard images attached — uploaded inline on send]")
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_display(&chip, area.width as usize),
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )))
            .style(surface_style),
            Rect::new(area.x, y, area.width, 1),
        );
        y = y.saturating_add(1);

        if ui.inline_images {
            if let Some(ref preview) = ui.composer.pending_preview {
                for thumb_line in preview.lines.iter().take(4) {
                    if y >= area.bottom().saturating_sub(1) {
                        break;
                    }
                    frame.render_widget(
                        Paragraph::new(thumb_line.clone()).style(surface_style),
                        Rect::new(area.x, y, area.width, 1),
                    );
                    y = y.saturating_add(1);
                }
            }
        }
    }

    let text_height = area.bottom().saturating_sub(y);
    if text_height == 0 {
        return None;
    }
    let text_area = Rect::new(area.x, y, area.width, text_height);

    let gutter_w = if text_area.width > 4 { 2usize } else { 0usize };
    let body_width = (text_area.width as usize).saturating_sub(gutter_w).max(1);
    ui.composer.last_inner_width = body_width.min(u16::MAX as usize) as u16;
    let text = ui.composer.text();
    let selection = ui.composer.selection_range();
    let (visual, cursor_row, cursor_col) = text_layout(text, body_width, ui.composer.cursor());

    let height = text_height as usize;
    let mut offset = if cursor_row >= height {
        cursor_row + 1 - height
    } else {
        0
    };
    offset = offset.min(visual.len().saturating_sub(height));

    let prompt_style = if focused {
        theme.accent_style().add_modifier(Modifier::BOLD)
    } else {
        theme.muted_style()
    };

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    if text.is_empty() {
        let nl = if ui.kitty_keyboard_supported {
            "Shift+Enter"
        } else {
            "Alt+Enter / Ctrl+J"
        };
        let hint = if focused {
            format!("Write a message (Enter send · {nl} newline · PgUp/↑↓ scroll · @ mention)")
        } else {
            "Press i or Enter to compose · j/k select · Ctrl+K jump".to_string()
        };
        let placeholder_span = Span::styled(
            truncate_display(&hint, body_width),
            theme.muted_style().add_modifier(Modifier::ITALIC),
        );
        if gutter_w == 2 {
            lines.push(Line::from(vec![
                Span::styled("> ", prompt_style),
                placeholder_span,
            ]));
        } else {
            lines.push(Line::from(placeholder_span));
        }
    } else {
        for (vis_idx, &(start, end)) in visual.iter().enumerate().skip(offset).take(height) {
            let gutter = if gutter_w == 2 {
                Some(if vis_idx == 0 {
                    Span::styled("> ", prompt_style)
                } else {
                    Span::raw("  ")
                })
            } else {
                None
            };
            push_line_spans(
                &mut lines,
                text,
                start,
                end,
                selection,
                theme.selection(),
                gutter,
            );
        }
    }

    frame.render_widget(Paragraph::new(lines).style(surface_style), text_area);

    if focused && !ui.composer.subject_focused {
        ui.composer.popup.draw(frame, area, &theme, hitmap);
    }
    if !focused {
        return None;
    }

    if let Some(subj_y) = subject_y.filter(|_| ui.composer.subject_focused) {
        let prefix_w = UnicodeWidthStr::width(SUBJECT_PREFIX) as u16;
        let cx = area
            .x
            .saturating_add(prefix_w)
            .saturating_add(subject_cursor_col)
            .min(area.right().saturating_sub(1));
        return Some((cx, subj_y));
    }

    let cursor_y = text_area
        .y
        .saturating_add(cursor_row.saturating_sub(offset) as u16)
        .min(text_area.bottom().saturating_sub(1));
    let cursor_x = text_area
        .x
        .saturating_add(gutter_w as u16)
        .saturating_add(cursor_col as u16)
        .min(text_area.right().saturating_sub(1));

    Some((cursor_x, cursor_y))
}

fn is_composing_channel_root(app: &AppState, ui: &UiState) -> bool {
    let is_channel = app
        .active_conversation()
        .is_some_and(|c| c.kind == ConversationKind::Channel);
    if !is_channel {
        return false;
    }
    !(ui.thread_drawer_open && ui.composer_target_thread && app.active_thread.is_some())
}

#[allow(clippy::too_many_arguments)]
fn draw_composer_hairline(
    frame: &mut Frame,
    rect: Rect,
    ui: &UiState,
    app: &AppState,
    show_subject: bool,
    is_channel_root: bool,
    hairline_style: Style,
    pill_style: Style,
    hitmap: &mut HitMap,
) {
    let width = rect.width as usize;
    if width == 0 || rect.height == 0 {
        return;
    }

    let conv_name = app
        .active_conversation()
        .map(|c| c.title.as_str())
        .unwrap_or("No conversation");

    let file_pill = "[+File]";
    let file_pill_w = UnicodeWidthStr::width(file_pill);
    let show_file_pill = width >= 20 && width >= file_pill_w + 10;
    let right_reserved = if show_file_pill { file_pill_w + 3 } else { 0 };
    let left_max = width.saturating_sub(right_reserved);

    let subj_pill = if show_subject {
        "[Subject ✓]"
    } else {
        "[Subject]"
    };
    let subj_pill_w = UnicodeWidthStr::width(subj_pill);
    let in_thread = ui.thread_drawer_open && ui.composer_target_thread && app.active_thread.is_some();

    let (base_prefix, include_subject_pill) = if !show_subject {
        if let Some(reply) = ui.composer.reply() {
            let max_summary = left_max
                .saturating_sub(UnicodeWidthStr::width(reply.author.as_str()) + 34)
                .max(6);
            (
                format!(
                    "─ ↩ Replying to {}: \"{}\" · Esc cancel ",
                    reply.author,
                    reply.summary(max_summary)
                ),
                false,
            )
        } else if in_thread {
            ("─ Reply in thread · Alt+T channel ".to_string(), false)
        } else if is_channel_root {
            (format!("─ New post in {conv_name} "), true)
        } else {
            (format!("─ {conv_name} "), false)
        }
    } else if in_thread {
        ("─ Reply in thread · Alt+T channel ".to_string(), false)
    } else if is_channel_root {
        (format!("─ New post in {conv_name} "), true)
    } else {
        (format!("─ {conv_name} "), false)
    };

    let show_subj_pill = include_subject_pill && left_max >= subj_pill_w + 8;
    let prefix_budget = if show_subj_pill {
        left_max.saturating_sub(subj_pill_w + 2)
    } else {
        left_max
    };
    let clipped_prefix = truncate_display(&base_prefix, prefix_budget);
    let mut used = UnicodeWidthStr::width(clipped_prefix.as_str());

    let mut spans = vec![Span::styled(clipped_prefix, hairline_style)];

    if show_subj_pill {
        let subj_x = rect.x.saturating_add(used as u16);
        spans.push(Span::styled(subj_pill, pill_style));
        spans.push(Span::styled(" ", hairline_style));
        hitmap.add_line(
            subj_x,
            rect.y,
            subj_pill_w as u16,
            HitTarget::Button(ButtonId::SubjectToggle),
        );
        used += subj_pill_w + 1;
    }

    if show_file_pill {
        let fill_len = width.saturating_sub(used + file_pill_w + 2);
        if fill_len > 0 {
            spans.push(Span::styled("─".repeat(fill_len), hairline_style));
            used += fill_len;
        }
        spans.push(Span::styled(" ", hairline_style));
        used += 1;
        let file_x = rect.x.saturating_add(used as u16);
        spans.push(Span::styled(file_pill, pill_style));
        hitmap.add_line(
            file_x,
            rect.y,
            file_pill_w as u16,
            HitTarget::Button(ButtonId::SendFile),
        );
        used += file_pill_w;
        if used < width {
            spans.push(Span::styled("─".repeat(width - used), hairline_style));
        }
    } else if used < width {
        spans.push(Span::styled("─".repeat(width - used), hairline_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

fn push_line_spans(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    start: usize,
    end: usize,
    selection: Option<(usize, usize)>,
    selection_style: Style,
    gutter: Option<Span<'static>>,
) {
    let mut spans: Vec<Span> = Vec::new();
    if let Some(g) = gutter {
        spans.push(g);
    }
    let mut run = String::new();
    let mut run_selected = false;

    let slice = text.get(start..end).unwrap_or("");
    for (offset, ch) in slice.char_indices() {
        let pos = start + offset;
        let selected = selection
            .map(|(from, to)| pos >= from && pos < to)
            .unwrap_or(false);
        if !run.is_empty() && selected != run_selected {
            let style = if run_selected {
                selection_style
            } else {
                Style::default()
            };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_selected = selected;
        if ch == '\t' {
            run.push_str("    ");
        } else {
            run.push(ch);
        }
    }
    if !run.is_empty() {
        let style = if run_selected {
            selection_style
        } else {
            Style::default()
        };
        spans.push(Span::styled(run, style));
    }
    if spans.is_empty() {
        lines.push(Line::from(""));
    } else {
        lines.push(Line::from(spans));
    }
}

// ---------------------------------------------------------------------------
// Text layout helpers
// ---------------------------------------------------------------------------

pub fn text_layout(text: &str, width: usize, cursor: usize) -> (Vec<(usize, usize)>, usize, usize) {
    let width = width.max(1);
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut row = 0usize;
    let mut col = 0usize;
    let mut line_start = 0usize;
    let mut cursor_pos: Option<(usize, usize)> = None;

    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            if index == cursor {
                cursor_pos = Some((row, col));
            }
            lines.push((line_start, index));
            row += 1;
            col = 0;
            line_start = index + 1;
            continue;
        }
        let char_width = if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if col > 0 && col + char_width > width {
            lines.push((line_start, index));
            row += 1;
            col = 0;
            line_start = index;
        }
        if index == cursor {
            cursor_pos = Some((row, col));
        }
        col += char_width;
    }
    if cursor >= text.len() {
        cursor_pos = Some((row, col));
    }
    lines.push((line_start, text.len()));

    let (cursor_row, cursor_col) = cursor_pos.unwrap_or((row, col));
    (lines, cursor_row, cursor_col)
}

fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            ranges.push((start, index));
            start = index + 1;
        }
    }
    ranges.push((start, text.len()));
    ranges
}

fn line_col(text: &str, cursor: usize) -> (usize, usize) {
    let ranges = line_ranges(text);
    let cursor = cursor.min(text.len());
    let mut line = 0;
    for (index, &(start, end)) in ranges.iter().enumerate() {
        if cursor <= end && cursor >= start {
            line = index;
            break;
        }
    }
    let (start, _) = ranges[line];
    (line, text[start..cursor.min(text.len())].chars().count())
}

fn byte_at_col(text: &str, start: usize, end: usize, col: usize) -> usize {
    let mut pos = start;
    for (count, ch) in text[start..end].chars().enumerate() {
        if count >= col {
            break;
        }
        pos += ch.len_utf8();
    }
    pos
}

fn prev_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 || cursor > text.len() {
        return None;
    }
    let mut prev = cursor - 1;
    while !text.is_char_boundary(prev) {
        if prev == 0 {
            return None;
        }
        prev -= 1;
    }
    Some(prev)
}

fn next_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor >= text.len() {
        return None;
    }
    let ch = text[cursor..].chars().next()?;
    Some(cursor + ch.len_utf8())
}

pub fn normalize(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn truncate_display(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if used + w > width {
            break;
        }
        if ch == '\t' {
            out.push_str("    ");
        } else {
            out.push(ch);
        }
        used += w;
    }
    out
}

fn slice_display_window(text: &str, cursor: usize, max_width: usize) -> (String, usize) {
    if max_width == 0 {
        return (String::new(), 0);
    }
    let mut safe_cursor = cursor.min(text.len());
    while safe_cursor > 0 && !text.is_char_boundary(safe_cursor) {
        safe_cursor -= 1;
    }
    let cursor_total_col: usize = text[..safe_cursor]
        .chars()
        .map(|ch| {
            if ch == '\t' {
                1
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            }
        })
        .sum();
    let scroll_col = if cursor_total_col < max_width {
        0
    } else {
        cursor_total_col.saturating_sub(max_width.saturating_sub(1))
    };

    let mut out = String::new();
    let mut col = 0usize;
    let mut start_col: Option<usize> = None;
    let mut vis_w = 0usize;
    for ch in text.chars() {
        let display_ch = if ch == '\t' { ' ' } else { ch };
        let cw = UnicodeWidthChar::width(display_ch).unwrap_or(0);
        if col < scroll_col {
            col += cw;
            continue;
        }
        if start_col.is_none() {
            start_col = Some(col);
        }
        if vis_w + cw > max_width {
            break;
        }
        out.push(display_ch);
        vis_w += cw;
        col += cw;
    }
    let origin = start_col.unwrap_or(col);
    let cursor_col = cursor_total_col
        .saturating_sub(origin)
        .min(max_width.saturating_sub(1));
    (out, cursor_col)
}

// ---------------------------------------------------------------------------
// External Editor
// ---------------------------------------------------------------------------

pub fn external_edit(current: &str) -> std::io::Result<String> {
    let path = std::env::temp_dir().join(format!(
        "teams-compose-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ));

    std::fs::write(&path, current)?;

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(default_editor);

    let (program, args) = resolve_editor_command(&editor);

    let status = Command::new(&program).args(&args).arg(&path).status();

    let contents = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);

    let _ = status?;
    Ok(normalize(&contents?).trim_end().to_string())
}

fn resolve_editor_command(editor: &str) -> (std::path::PathBuf, Vec<String>) {
    let parts = parse_editor_command(editor);
    let raw_program = parts.first().map(String::as_str).unwrap_or("vi");
    let program = crate::config::expand_tilde(raw_program);
    let args = parts.get(1..).unwrap_or(&[]).to_vec();
    (program, args)
}

fn parse_editor_command(raw: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_token = false;

    for ch in raw.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    in_token = true;
                } else if ch.is_whitespace() {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else {
                    current.push(ch);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        args.push(current);
    }
    args
}

fn default_editor() -> String {
    if cfg!(windows) {
        "notepad".to_string()
    } else if cfg!(target_os = "macos") {
        "open -W -t".to_string()
    } else {
        "vi".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer_with(text: &str) -> ComposerState {
        let mut composer = ComposerState::new();
        composer.insert_str(text);
        composer
    }

    fn sample_members() -> Vec<MemberInfo> {
        vec![MemberInfo {
            user_id: "user-7".to_string(),
            member_id: None,
            display_name: "kim".to_string(),
            email: Some("kim@contoso.com".to_string()),
            roles: vec![],
            presence: None,
        }]
    }

    #[test]
    fn insert_and_backspace_utf8() {
        let mut composer = ComposerState::new();
        composer.insert_char('가');
        composer.insert_char('a');
        assert_eq!(composer.text(), "가a");
        composer.backspace();
        assert_eq!(composer.text(), "가");
        composer.backspace();
        assert_eq!(composer.text(), "");
        composer.backspace();
    }

    #[test]
    fn cursor_moves_across_utf8_lines() {
        let mut composer = composer_with("ab\n가나");
        assert_eq!(composer.cursor(), 9);
        composer.move_left(false);
        assert_eq!(composer.cursor(), 6);
        composer.move_home(false);
        assert_eq!(composer.cursor(), 3);
        composer.move_up(false);
        assert_eq!(composer.cursor(), 0);
        composer.move_end(false);
        assert_eq!(composer.cursor(), 2);
        composer.move_down(false);
        assert_eq!(composer.cursor(), 9);
    }

    #[test]
    fn undo_redo_and_selection_work() {
        let mut composer = composer_with("hello");
        composer.select_all();
        assert_eq!(composer.selected_text(), Some("hello"));
        composer.insert_char('X');
        assert_eq!(composer.text(), "X");
        assert!(composer.undo());
        assert_eq!(composer.text(), "hello");
        assert!(composer.redo());
        assert_eq!(composer.text(), "X");
    }

    #[test]
    fn mention_autocomplete_and_shift() {
        let mut composer = ComposerState::new();
        let members = sample_members();
        composer.insert_str("hi @ki");
        composer.reference_mentions(&members);
        assert!(composer.mention_active());
        assert!(composer.accept_mention());
        assert_eq!(composer.text(), "hi @kim ");
        assert_eq!(composer.mentions().len(), 1);
        assert_eq!(composer.mentions()[0].user_id, "user-7");
        assert_eq!(composer.mentions()[0].start, 4);

        composer.move_doc_start(false);
        composer.insert_str("yo ");
        assert_eq!(composer.mentions()[0].start, 7);
    }

    #[test]
    fn stage_inline_image_makes_composer_non_empty() {
        let mut composer = ComposerState::new();
        assert!(composer.is_empty());
        let png = encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        composer.stage_inline_image(png, "image/png", Some("clip.png".into()));
        assert!(!composer.is_empty());
        assert_eq!(composer.pending_images.len(), 1);
        assert_eq!(
            encode_rgba_to_png(0, 2, vec![]),
            Err("Clipboard has 0x0 image data".to_string())
        );
    }

    #[test]
    fn desired_height_auto_sizes_with_two_row_minimum() {
        let mut composer = ComposerState::new();
        assert_eq!(composer.desired_height(false, false), 2);
        assert_eq!(composer.desired_height(true, false), 3);

        composer.set_reply(ReplyPreview {
            msg_id: "m1".into(),
            author: "Alice".into(),
            text: "hi".into(),
        });
        assert_eq!(composer.desired_height(false, false), 2);
        assert_eq!(composer.desired_height(true, false), 4);

        composer.insert_str("line1\nline2\nline3");
        assert_eq!(composer.desired_height(false, false), 4);

        let png = encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        composer.stage_inline_image(png, "image/png", None);
        assert!(composer.desired_height(false, false) < composer.desired_height(false, true));
    }

    #[test]
    fn text_layout_wrap_boundary_places_cursor_on_next_row_and_soft_wrap_expands_height() {
        // Cursor at byte 5 in "abcdeX" with width 5 wraps 'X' to row 1 col 0.
        let (lines, row, col) = text_layout("abcdeX", 5, 5);
        assert_eq!(lines, vec![(0, 5), (5, 6)]);
        assert_eq!((row, col), (1, 0));

        // Tab has deterministic width 4.
        let (_, tab_row, tab_col) = text_layout("\ta", 10, 1);
        assert_eq!((tab_row, tab_col), (0, 4));

        // Long paragraph without '\n' soft-wraps and grows desired_height.
        let mut composer = ComposerState::new();
        composer.last_inner_width = 72;
        composer.insert_str(&"x".repeat(250));
        let h = composer.desired_height(false, false);
        assert!(h > 2 && h <= 9);

        // Subject-focused Ctrl+Home / Ctrl+End and horizontal window scrolling.
        composer.subject = "0123456789ABCDEF".to_string();
        composer.subject_cursor = 5;
        composer.subject_focused = true;
        composer.move_doc_end(false);
        assert_eq!(composer.subject_cursor, composer.subject.len());
        let (win, ccol) = slice_display_window(&composer.subject, composer.subject_cursor, 8);
        assert_eq!(win.len(), 7);
        assert!(ccol < 8);
        composer.move_doc_start(false);
        assert_eq!(composer.subject_cursor, 0);

        // Quoted Windows $EDITOR paths stay intact.
        assert_eq!(
            parse_editor_command(r#""C:\Program Files\Sublime Text\subl.exe" -w"#),
            vec![r"C:\Program Files\Sublime Text\subl.exe", "-w"]
        );
    }

    #[test]
    fn resolve_editor_command_expands_leading_tilde_on_program_path() {
        let Some(home) = dirs::home_dir() else {
            return;
        };

        let (prog, args) = resolve_editor_command("~/bin/my-editor --wait ~/keep-arg");
        assert_eq!(prog, home.join("bin/my-editor"));
        assert_eq!(args, vec!["--wait", "~/keep-arg"]);

        let (quoted_prog, quoted_args) =
            resolve_editor_command(r#""~/My Editors/code-wait" -w"#);
        assert_eq!(quoted_prog, home.join("My Editors/code-wait"));
        assert_eq!(quoted_args, vec!["-w"]);

        let (bare_prog, bare_args) = resolve_editor_command("vim -f");
        assert_eq!(bare_prog, std::path::PathBuf::from("vim"));
        assert_eq!(bare_args, vec!["-f"]);
    }
}
