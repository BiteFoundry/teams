//! Right Thread Drawer for Team Channel threads (`src/ui/thread_view.rs`).
//!
//! Shows the root post summary, scrollable chronological replies, composer target
//! toggle (`[Target: Thread]` vs `[Target: Channel]`), and close button (`Esc` / `[✕]`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::DensityMode,
    img_preview::RenderedTerminalImage,
    models::{PresenceStatus, ThreadView},
    ui::{
        app::{AppState, Focus, UiState},
        chat_view::{
            apply_selection_to_lines, hash_message_sig, hash_theme_sig, messages_groupable,
            render_message_item_with_ctx, render_scrolled_lines_with_selection, truncate,
            CachedStream, DualHasher, LineSliceCache, MessageRenderCtx, RenderedLine,
            VisibleSelection,
        },
        hitmap::{ButtonId, HitMap, HitTarget, MessageScope},
        theme::Theme,
    },
};

thread_local! {
    static THREAD_LINES_CACHE: RefCell<LineSliceCache> = RefCell::new(LineSliceCache::new(4));
}

#[allow(clippy::too_many_arguments)]
fn compute_thread_sig(
    thread: &ThreadView,
    density: DensityMode,
    my_user_id: &str,
    my_display_name: Option<&str>,
    presences: Option<&HashMap<String, PresenceStatus>>,
    my_presence: Option<&PresenceStatus>,
    image_cache_gen: u64,
    inline_images: bool,
    show_hrefs: bool,
    theme: &Theme,
    width: usize,
) -> (u64, u64) {
    let mut h = DualHasher::new(0x5448_5245_4144_5f4c);
    h.write_str(&thread.conv_key);
    hash_message_sig(&mut h, &thread.root);
    h.write_hash(&thread.replies.len());
    for reply in &thread.replies {
        hash_message_sig(&mut h, reply);
        if let (Some(map), Some(aid)) = (presences, reply.author_id.as_deref()) {
            if let Some(p) = map.get(aid) {
                h.write_hash(&1u8);
                h.write_str(&p.availability);
            } else {
                h.write_hash(&0u8);
            }
        }
    }
    if let (Some(map), Some(aid)) = (presences, thread.root.author_id.as_deref()) {
        if let Some(p) = map.get(aid) {
            h.write_hash(&1u8);
            h.write_str(&p.availability);
        } else {
            h.write_hash(&0u8);
        }
    }
    if let Some(mp) = my_presence {
        h.write_hash(&1u8);
        h.write_str(&mp.availability);
    } else {
        h.write_hash(&0u8);
    }
    h.write_hash(&density);
    h.write_str(my_user_id);
    if let Some(dn) = my_display_name {
        h.write_hash(&1u8);
        h.write_str(dn);
    } else {
        h.write_hash(&0u8);
    }
    h.write_hash(&inline_images);
    h.write_hash(&show_hrefs);
    h.write_hash(&image_cache_gen);
    h.write_hash(&width);
    hash_theme_sig(&mut h, theme);
    h.finish()
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if ui.theme.bg_canvas != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(ui.theme.bg_canvas)),
            area,
        );
    }

    let reply_count = app.active_thread.as_ref().map(|t| t.replies.len());
    draw_header(frame, area, ui, reply_count, hitmap);

    let body_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.thread_scroll.area = body_area;

    if body_area.height == 0 {
        return;
    }

    let Some(thread) = &app.active_thread else {
        ui.thread_scroll.message_ranges = Arc::from([]);
        ui.thread_scroll.total_lines = 0;
        let msg = if app.loading_thread {
            "Loading thread…"
        } else {
            "No thread open"
        };
        frame.render_widget(
            Paragraph::new(msg).style(ui.theme.muted_style()),
            body_area,
        );
        return;
    };

    let width = (body_area.width as usize).max(12);
    let thumb_cols = width.saturating_sub(4).clamp(16, 40) as u16;
    let thumb_rows = 10u16;
    let my_display_name = app.my_profile.as_ref().map(|p| p.display_name.as_str());
    let thumbs = if ui.inline_images {
        Some(&app.image_cache)
    } else {
        None
    };

    let cached = build_thread_stream_cached(
        thread,
        ui.density,
        &app.my_user_id,
        my_display_name,
        Some(&app.presences),
        app.my_presence.as_ref(),
        thumbs,
        app.image_cache_gen,
        (thumb_cols, thumb_rows),
        ui.show_hrefs,
        &ui.theme,
        width,
    );

    ui.thread_scroll.message_ranges = Arc::clone(&cached.ranges);
    if cached.lines.is_empty() {
        ui.thread_scroll.total_lines = 0;
        return;
    }

    let height = body_area.height as usize;
    ui.thread_scroll.total_lines = cached.lines.len();
    ui.thread_scroll.clamp_to_viewport(height);
    let offset = ui.thread_scroll.offset(height);
    ui.thread_scroll.offset = offset;

    if ui.inline_images {
        let vis_start = offset.saturating_sub(20);
        let vis_end = offset.saturating_add(height).saturating_add(20);
        let mut to_fetch = Vec::new();
        for (sel_idx, &(r_start, r_end)) in cached.ranges.iter().enumerate() {
            if r_end > vis_start && r_start < vis_end {
                let msg = if sel_idx == 0 {
                    Some(&thread.root)
                } else {
                    thread.replies.get(sel_idx - 1)
                };
                if let Some(msg) = msg {
                    let eff = msg.effective_attachments();
                    for att in eff.iter() {
                        if crate::img_preview::is_image_mime_or_name(
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

    let selection = ui.thread_scroll.selected_idx.and_then(|sel_idx| {
        let range = *cached.ranges.get(sel_idx)?;
        let (scope, msg_index) = if sel_idx == 0 {
            (MessageScope::ThreadRoot, 0)
        } else {
            (MessageScope::ThreadReply, sel_idx - 1)
        };
        Some(VisibleSelection {
            range,
            scope,
            msg_index,
            show_thread_action: false,
            theme: &ui.theme,
        })
    });

    render_scrolled_lines_with_selection(
        frame,
        body_area,
        &cached.lines,
        offset,
        height,
        selection,
        hitmap,
    );
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    ui: &UiState,
    reply_count: Option<usize>,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let header_rect = Rect::new(area.x, area.y, area.width, 1);
    let header_bg = if theme.no_color {
        Color::Reset
    } else {
        theme.bg_header
    };
    let header_style = Style::default().bg(header_bg);

    let target_label = if ui.composer_target_thread {
        "[Target: Thread]"
    } else {
        "[Target: Channel]"
    };
    let close_label = "[✕]";

    let target_w = UnicodeWidthStr::width(target_label) as u16;
    let close_w = UnicodeWidthStr::width(close_label) as u16;
    let buttons_w = target_w + 1 + close_w;

    let raw_title = match reply_count {
        Some(1) => "Thread · 1 reply".to_string(),
        Some(n) => format!("Thread · {n} replies"),
        None => "Thread".to_string(),
    };

    let title_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(if ui.focus == Focus::Thread {
                theme.accent
            } else {
                theme.fg_primary
            })
            .bg(header_bg)
            .add_modifier(Modifier::BOLD)
    };

    let show_target = area.width >= 10 + buttons_w + 2;
    let used_buttons_w = if show_target {
        buttons_w
    } else if area.width >= close_w + 4 {
        close_w
    } else {
        0
    };

    let title_budget = area.width.saturating_sub(used_buttons_w + 2).max(1) as usize;
    let title = truncate(&raw_title, title_budget);
    let title_w = UnicodeWidthStr::width(title.as_str()) as u16;
    let pad = area
        .width
        .saturating_sub(1 + title_w)
        .saturating_sub(used_buttons_w);

    let mut spans = vec![
        Span::styled(" ", header_style),
        Span::styled(title, title_style),
        Span::styled(" ".repeat(pad as usize), header_style),
    ];

    if show_target {
        let target_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.accent)
                .bg(header_bg)
                .add_modifier(Modifier::BOLD)
        };
        let close_style = if theme.no_color {
            theme.muted_style()
        } else {
            Style::default().fg(theme.fg_muted).bg(header_bg)
        };
        spans.push(Span::styled(target_label, target_style));
        spans.push(Span::styled(" ", header_style));
        spans.push(Span::styled(close_label, close_style));

        let target_x = area.x + 1 + title_w + pad;
        hitmap.add_line(
            target_x,
            area.y,
            target_w,
            HitTarget::Button(ButtonId::ComposerTargetToggle),
        );
        let close_x = target_x + target_w + 1;
        hitmap.add_line(
            close_x,
            area.y,
            close_w,
            HitTarget::Button(ButtonId::CloseThread),
        );
    } else if used_buttons_w == close_w {
        let close_style = if theme.no_color {
            theme.muted_style()
        } else {
            Style::default().fg(theme.fg_muted).bg(header_bg)
        };
        spans.push(Span::styled(close_label, close_style));
        let close_x = area.x + 1 + title_w + pad;
        hitmap.add_line(
            close_x,
            area.y,
            close_w,
            HitTarget::Button(ButtonId::CloseThread),
        );
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(header_style), header_rect);
}

/// Pure thread line builder: root post summary + divider + chronological replies.
/// `selected_idx` uses `Some(0)` for root and `Some(i + 1)` for reply `i`.
pub fn build_thread_lines(
    thread: &ThreadView,
    selected_idx: Option<usize>,
    theme: &Theme,
    width: usize,
    _hyperlinks: bool,
) -> Vec<RenderedLine> {
    let width = width.max(12);
    let cached = build_thread_stream_cached(
        thread,
        DensityMode::Cozy,
        "",
        None,
        None,
        None,
        None,
        0,
        (0, 0),
        false,
        theme,
        width,
    );
    let mut lines = cached.lines.to_vec();
    if let Some(sel_idx) = selected_idx {
        if let Some(&(start, end)) = cached.ranges.get(sel_idx) {
            let (scope, msg_index) = if sel_idx == 0 {
                (MessageScope::ThreadRoot, 0)
            } else {
                (MessageScope::ThreadReply, sel_idx - 1)
            };
            apply_selection_to_lines(&mut lines[start..end], scope, msg_index, width, theme, false);
        }
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn build_thread_stream_cached(
    thread: &ThreadView,
    density: DensityMode,
    my_user_id: &str,
    my_display_name: Option<&str>,
    presences: Option<&HashMap<String, PresenceStatus>>,
    my_presence: Option<&PresenceStatus>,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    image_cache_gen: u64,
    thumb_dims: (u16, u16),
    show_hrefs: bool,
    theme: &Theme,
    width: usize,
) -> CachedStream {
    let width = width.max(12);
    let sig = compute_thread_sig(
        thread,
        density,
        my_user_id,
        my_display_name,
        presences,
        my_presence,
        image_cache_gen,
        image_cache.is_some(),
        show_hrefs,
        theme,
        width,
    );
    if let Some(cached) = THREAD_LINES_CACHE.with(|cache| cache.borrow_mut().get(sig)) {
        return cached;
    }

    let (out, ranges) = build_thread_lines_uncached(
        thread,
        None,
        density,
        my_user_id,
        my_display_name,
        presences,
        my_presence,
        image_cache,
        thumb_dims,
        show_hrefs,
        theme,
        width,
    );

    let cached = CachedStream {
        lines: Arc::from(out),
        ranges: Arc::from(ranges),
    };
    THREAD_LINES_CACHE.with(|cache| cache.borrow_mut().insert(sig, cached.clone()));
    cached
}

#[allow(clippy::too_many_arguments)]
fn build_thread_lines_uncached(
    thread: &ThreadView,
    selected_idx: Option<usize>,
    density: DensityMode,
    my_user_id: &str,
    my_display_name: Option<&str>,
    presences: Option<&HashMap<String, PresenceStatus>>,
    my_presence: Option<&PresenceStatus>,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
    show_hrefs: bool,
    theme: &Theme,
    width: usize,
) -> (Vec<RenderedLine>, Vec<(usize, usize)>) {
    let mut out = Vec::new();
    let mut ranges = Vec::with_capacity(1 + thread.replies.len());

    let root_start = out.len();
    let root_ctx = MessageRenderCtx {
        scope: MessageScope::ThreadRoot,
        is_channel: true,
        density,
        is_grouped: false,
        selected: selected_idx == Some(0),
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
    render_message_item_with_ctx(&mut out, &thread.root, 0, &root_ctx);
    let root_end = out.len();
    ranges.push((root_start, root_end));

    if density == DensityMode::Cozy {
        out.push(RenderedLine::plain(Line::raw("")));
    }
    let divider_label = if thread.replies.len() == 1 {
        "── 1 reply ──".to_string()
    } else {
        format!("── {} replies ──", thread.replies.len())
    };
    out.push(RenderedLine::plain(Line::from(Span::styled(
        truncate(&divider_label, width),
        theme.accent_style().add_modifier(Modifier::BOLD),
    ))));

    if thread.replies.is_empty() {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "  No replies yet — write the first reply below.",
            theme.muted_style(),
        ))));
    } else {
        for (i, reply) in thread.replies.iter().enumerate() {
            let is_grouped = i > 0 && messages_groupable(&thread.replies[i - 1], reply, false);
            if density == DensityMode::Cozy && !is_grouped && i > 0 {
                out.push(RenderedLine::plain(Line::raw("")));
            }
            let reply_start = out.len();
            let reply_ctx = MessageRenderCtx {
                scope: MessageScope::ThreadReply,
                is_channel: true,
                density,
                is_grouped,
                selected: selected_idx == Some(i + 1),
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
            render_message_item_with_ctx(&mut out, reply, i, &reply_ctx);
            let reply_end = out.len();
            ranges.push((reply_start, reply_end));
        }
    }

    (out, ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MessageItem;

    fn msg(id: &str, author: &str, body: &str, parent: Option<&str>) -> MessageItem {
        MessageItem {
            msg_key: format!("chan:t1:c1|{id}"),
            conv_key: "chan:t1:c1".into(),
            msg_id: id.into(),
            parent_msg_id: parent.map(str::to_string),
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: if parent.is_none() {
                Some("Release Notice".into())
            } else {
                None
            },
            author_id: Some("u1".into()),
            author_name: author.into(),
            is_me: false,
            created_at: "2026-11-15T06:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: body.into(),
            body_plain: body.into(),
            attachments: vec![],
            mentions: vec![],
            reactions: vec![],
            reply_count: 1,
            web_url: None,
        }
    }

    fn joined_text(lines: &[RenderedLine]) -> String {
        lines
            .iter()
            .map(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn thread_lines_include_root_and_replies() {
        let thread = ThreadView {
            conv_key: "chan:t1:c1".into(),
            root: msg("r1", "Alice", "Deploy at 5 PM", None),
            replies: vec![msg("rep1", "Bob", "Acknowledged!", Some("r1"))],
        };
        let lines = build_thread_lines(&thread, Some(0), &Theme::default(), 60, false);
        let joined = joined_text(&lines);

        assert!(joined.contains("Release Notice"));
        assert!(joined.contains("Deploy at 5 PM"));
        assert!(joined.contains("1 reply"));
        assert!(joined.contains("Acknowledged!"));
        assert!(!joined.contains('\x1b'));
    }

    #[test]
    fn thread_selection_aligns_root_at_0_and_replies_at_1_plus_i() {
        let mut rep1 = msg("rep1", "Bob", "First reply", Some("r1"));
        rep1.author_id = Some("u2".into());
        let mut rep2 = msg("rep2", "Carol", "Second reply", Some("r1"));
        rep2.author_id = Some("u3".into());
        let thread = ThreadView {
            conv_key: "chan:t1:c1".into(),
            root: msg("r1", "Alice", "Root body", None),
            replies: vec![rep1, rep2],
        };
        let theme = Theme::midnight_dark();

        // Select root (selected_idx = Some(0))
        let (lines_root, ranges) = build_thread_lines_uncached(
            &thread,
            Some(0),
            DensityMode::Cozy,
            "",
            None,
            None,
            None,
            None,
            (0, 0),
            false,
            &theme,
            80,
        );
        assert_eq!(ranges.len(), 3);
        let root_slice = &lines_root[ranges[0].0..ranges[0].1];
        let rep0_slice = &lines_root[ranges[1].0..ranges[1].1];
        assert!(joined_text(root_slice).contains("▎ "));
        assert!(!joined_text(rep0_slice).contains("▎ "));

        // Select reply 0 (selected_idx = Some(1))
        let (lines_rep0, ranges_rep0) = build_thread_lines_uncached(
            &thread,
            Some(1),
            DensityMode::Cozy,
            "",
            None,
            None,
            None,
            None,
            (0, 0),
            false,
            &theme,
            80,
        );
        let root_slice_2 = &lines_rep0[ranges_rep0[0].0..ranges_rep0[0].1];
        let rep0_slice_2 = &lines_rep0[ranges_rep0[1].0..ranges_rep0[1].1];
        let rep1_slice_2 = &lines_rep0[ranges_rep0[2].0..ranges_rep0[2].1];
        assert!(!joined_text(root_slice_2).contains("▎ "));
        assert!(joined_text(rep0_slice_2).contains("▎ "));
        assert!(!joined_text(rep1_slice_2).contains("▎ "));
    }
}
