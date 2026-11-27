//! Root UI module: 3-tier responsive layout, rendering pipeline, and shared helpers.

pub mod action;
pub mod app;
pub mod chat_view;
pub mod composer;
pub mod event;
pub mod hitmap;
pub mod keymap;
pub mod nav_pane;
pub mod notification;
pub mod overlays;
pub mod theme;
pub mod thread_view;
pub mod toast;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::app::{AppState, Focus, NarrowPane, UiState};
use self::hitmap::{ButtonId, HitMap, HitTarget};
use self::toast::ToastKind;

/// 3-tier responsive terminal width modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// >= 120 columns: Nav rail + Nav pane + Chat + optional Right Thread Drawer.
    Wide,
    /// 80..=119 columns: Nav pane + Main (Chat or Thread drawer split/swap).
    Medium,
    /// < 80 columns: Single active pane (`NarrowPane`) with compact top bar.
    Narrow,
}

pub fn layout_mode(width: u16) -> LayoutMode {
    if width >= 120 {
        LayoutMode::Wide
    } else if width >= 80 {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    }
}

#[derive(Debug, Clone, Default)]
pub struct LayoutPlan {
    pub mode: Option<LayoutMode>,
    pub top_bar: Option<Rect>,
    pub compact_bar: Option<Rect>,
    pub rail: Option<Rect>,
    pub nav: Option<Rect>,
    pub chat: Option<Rect>,
    pub thread: Option<Rect>,
    pub composer: Option<Rect>,
    pub status: Rect,
}

pub fn compute_layout(
    area: Rect,
    show_nav_rail: bool,
    thread_open: bool,
    auto_thread_pane: bool,
    narrow_pane: NarrowPane,
    composer_height: u16,
) -> LayoutPlan {
    compute_layout_with_options(
        area,
        show_nav_rail,
        thread_open,
        auto_thread_pane,
        narrow_pane,
        composer_height,
        true,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn compute_layout_with_options(
    area: Rect,
    show_nav_rail: bool,
    thread_open: bool,
    _auto_thread_pane: bool,
    narrow_pane: NarrowPane,
    composer_height: u16,
    show_composer: bool,
    _show_top_bar: bool,
) -> LayoutPlan {
    let mode = layout_mode(area.width);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let status = rows[1];

    let comp_h = if show_composer {
        composer_height.clamp(2, (body.height / 2).clamp(4, 12))
    } else {
        0
    };

    match mode {
        LayoutMode::Wide => {
            let nav_w = if show_nav_rail { 28u16 } else { 0 };
            let nav_sep_w = if nav_w > 0 { 1u16 } else { 0 };
            let avail_after_nav = body.width.saturating_sub(nav_w + nav_sep_w);
            let thread_w = if thread_open {
                (avail_after_nav * 40 / 100).clamp(38, 52)
            } else {
                0
            };
            let thread_sep_w = if thread_w > 0 { 1u16 } else { 0 };

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(nav_w),
                    Constraint::Length(nav_sep_w),
                    Constraint::Min(20),
                    Constraint::Length(thread_sep_w),
                    Constraint::Length(thread_w),
                ])
                .split(body);

            let (chat_rect, comp_rect) = if show_composer {
                let center_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                    .split(cols[2]);
                (Some(center_rows[0]), Some(center_rows[1]))
            } else {
                (Some(cols[2]), None)
            };

            LayoutPlan {
                mode: Some(mode),
                top_bar: None,
                compact_bar: None,
                rail: None,
                nav: (nav_w > 0).then_some(cols[0]),
                chat: chat_rect,
                thread: (thread_w > 0).then_some(cols[4]),
                composer: comp_rect,
                status,
            }
        }
        LayoutMode::Medium => {
            let nav_w = if show_nav_rail {
                if area.width >= 100 {
                    26u16
                } else {
                    24u16
                }
            } else {
                0
            };
            let nav_sep_w = if nav_w > 0 { 1u16 } else { 0 };
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(nav_w),
                    Constraint::Length(nav_sep_w),
                    Constraint::Min(20),
                ])
                .split(body);

            let (main_rect, comp_rect) = if show_composer {
                let main_rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                    .split(cols[2]);
                (main_rows[0], Some(main_rows[1]))
            } else {
                (cols[2], None)
            };

            let (chat, thread) = if thread_open {
                (None, Some(main_rect))
            } else {
                (Some(main_rect), None)
            };

            LayoutPlan {
                mode: Some(mode),
                top_bar: None,
                compact_bar: None,
                rail: None,
                nav: (nav_w > 0).then_some(cols[0]),
                chat,
                thread,
                composer: comp_rect,
                status,
            }
        }
        LayoutMode::Narrow => {
            let v = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(body);
            let compact_bar = Some(v[0]);
            let content = v[1];

            match narrow_pane {
                NarrowPane::List => LayoutPlan {
                    mode: Some(mode),
                    top_bar: None,
                    compact_bar,
                    rail: None,
                    nav: Some(content),
                    chat: None,
                    thread: None,
                    composer: None,
                    status,
                },
                NarrowPane::Chat => {
                    let (chat_rect, comp_rect) = if show_composer {
                        let split = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                            .split(content);
                        (Some(split[0]), Some(split[1]))
                    } else {
                        (Some(content), None)
                    };
                    LayoutPlan {
                        mode: Some(mode),
                        top_bar: None,
                        compact_bar,
                        rail: None,
                        nav: None,
                        chat: chat_rect,
                        thread: None,
                        composer: comp_rect,
                        status,
                    }
                }
                NarrowPane::Thread => {
                    let (thread_rect, comp_rect) = if show_composer {
                        let split = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                            .split(content);
                        (Some(split[0]), Some(split[1]))
                    } else {
                        (Some(content), None)
                    };
                    LayoutPlan {
                        mode: Some(mode),
                        top_bar: None,
                        compact_bar,
                        rail: None,
                        nav: None,
                        chat: None,
                        thread: thread_rect,
                        composer: comp_rect,
                        status,
                    }
                }
            }
        }
    }
}

/// Truncates `text` to fit within `max_width` terminal columns, appending `…` if cut.
pub fn truncate_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw >= max_width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Renders one complete frame, rebuilding `ui.hitmap` cleanly from scratch.
/// Returns the composer terminal cursor position `(x, y)` if focused.
pub fn draw(frame: &mut Frame, app: &mut AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    ui.pending_pixel_graphics.clear();
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.hitmap.clear();
    ui.nav_area = Rect::default();
    ui.chat_scroll.area = Rect::default();
    ui.thread_scroll.area = Rect::default();
    ui.thread_composer_area = Rect::default();
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let row_count = nav_pane::nav_row_count(app, ui);
    if row_count == 0 {
        ui.nav_selected = 0;
    } else if ui.nav_selected >= row_count {
        ui.nav_selected = row_count - 1;
    }

    let thread_open = ui.thread_drawer_open && app.active_thread.is_some();
    let show_subject = ui.show_subject
        && app
            .active_target
            .as_ref()
            .is_some_and(|t| t.is_channel())
        && !(thread_open && ui.composer_target_thread);
    let comp_h = ui.composer.desired_height(show_subject, ui.inline_images);
    let plan = compute_layout_with_options(
        area,
        ui.show_nav_rail,
        thread_open,
        ui.auto_thread_pane,
        ui.narrow_pane,
        comp_h,
        true,
        false,
    );

    let mut hitmap = std::mem::take(&mut ui.hitmap);

    if let Some(bar_area) = plan.compact_bar {
        draw_compact_bar(frame, bar_area, app, ui, thread_open, &mut hitmap);
    }

    if let Some(nav_area) = plan.nav {
        nav_pane::draw(frame, nav_area, app, ui, &mut hitmap);
    }

    let thread_anchored_rects = if plan.mode == Some(LayoutMode::Wide)
        && thread_open
        && ui.composer_target_thread
    {
        match (plan.chat, plan.composer, plan.thread) {
            (Some(chat_r), Some(comp_r), Some(thread_r)) => Some((chat_r, comp_r, thread_r)),
            _ => None,
        }
    } else {
        None
    };
    let thread_anchored_composer = thread_anchored_rects.is_some();

    let mut cursor_pos = None;

    if let Some((chat_r, comp_r, thread_r)) = thread_anchored_rects {
        let full_chat = Rect::new(
            chat_r.x,
            chat_r.y,
            chat_r.width,
            chat_r.height.saturating_add(comp_r.height),
        );
        if full_chat.height >= 2 {
            let chat_draw = Rect::new(
                full_chat.x,
                full_chat.y,
                full_chat.width,
                full_chat.height - 1,
            );
            let footer_rect = Rect::new(
                full_chat.x,
                full_chat.bottom().saturating_sub(1),
                full_chat.width,
                1,
            );
            chat_view::draw(frame, chat_draw, app, ui, &mut hitmap);
            draw_chat_switch_footer(frame, footer_rect, app, ui, &mut hitmap);
        } else {
            chat_view::draw(frame, full_chat, app, ui, &mut hitmap);
        }

        let t_comp_h = comp_r.height.min(thread_r.height.saturating_sub(2)).max(1);
        if thread_r.height >= 3 {
            let thread_draw = Rect::new(
                thread_r.x,
                thread_r.y,
                thread_r.width,
                thread_r.height - t_comp_h,
            );
            let thread_comp = Rect::new(
                thread_r.x,
                thread_r.bottom().saturating_sub(t_comp_h),
                thread_r.width,
                t_comp_h,
            );
            ui.thread_composer_area = thread_comp;
            thread_view::draw(frame, thread_draw, app, ui, &mut hitmap);
            cursor_pos = composer::draw(frame, thread_comp, app, ui, &mut hitmap);
        } else {
            ui.thread_composer_area = Rect::default();
            thread_view::draw(frame, thread_r, app, ui, &mut hitmap);
        }
    } else {
        ui.thread_composer_area = Rect::default();
        if let Some(chat_area) = plan.chat {
            chat_view::draw(frame, chat_area, app, ui, &mut hitmap);
        }
        if let Some(thread_area) = plan.thread {
            thread_view::draw(frame, thread_area, app, ui, &mut hitmap);
        }
        if let Some(comp_area) = plan.composer {
            cursor_pos = composer::draw(frame, comp_area, app, ui, &mut hitmap);
        }
    }

    // 1-cell vertical hairline separators between adjacent panes
    if let Some(nav_r) = plan.nav {
        if let Some(right_r) = plan.chat.or(plan.thread) {
            let sep_x = nav_r.right();
            if sep_x < right_r.x && sep_x < area.right() {
                let sep_focused = match ui.focus() {
                    Focus::Nav => true,
                    Focus::Chat => plan.chat.is_some(),
                    Focus::Thread => plan.mode == Some(LayoutMode::Medium) && plan.chat.is_none(),
                    Focus::Composer => !thread_anchored_composer,
                    Focus::Overlay => false,
                };
                let sep_style = if sep_focused {
                    ui.theme.accent_style()
                } else {
                    ui.theme.subtle_style()
                };
                draw_vertical_hairline(frame, sep_x, nav_r.y, nav_r.height, sep_style);
            }
        }
    }

    if let (Some(chat_r), Some(thread_r)) = (plan.chat, plan.thread) {
        let sep_x = chat_r.right();
        if sep_x < thread_r.x && sep_x < area.right() {
            let sep_focused = matches!(
                ui.focus(),
                Focus::Chat | Focus::Thread | Focus::Composer
            );
            let sep_style = if sep_focused {
                ui.theme.accent_style()
            } else {
                ui.theme.subtle_style()
            };
            draw_vertical_hairline(frame, sep_x, thread_r.y, thread_r.height, sep_style);
        }
    }

    draw_status_bar(frame, plan.status, app, ui, &mut hitmap);
    draw_toasts(frame, area, ui);

    let mut overlays = std::mem::take(&mut ui.overlays);
    for overlay in &mut overlays {
        match overlay {
            overlays::Overlay::ImagePreview(preview) => {
                ui.pending_pixel_graphics.clear();
                preview.draw_sync(frame, area, app, ui, &mut hitmap);
            }
            overlays::Overlay::HtmlPreview(preview) => {
                ui.pending_pixel_graphics.clear();
                preview.draw(frame, area, ui, &mut hitmap);
            }
            other => {
                ui.pending_pixel_graphics.clear();
                other.draw(frame, area, app, ui, &mut hitmap);
            }
        }
    }
    ui.overlays = overlays;

    ui.hitmap = hitmap;
    cursor_pos
}

fn draw_vertical_hairline(frame: &mut Frame, x: u16, y: u16, height: u16, style: Style) {
    let buf_area = frame.area();
    if height == 0 || x < buf_area.left() || x >= buf_area.right() {
        return;
    }
    let start_y = y.max(buf_area.top());
    let end_y = y.saturating_add(height).min(buf_area.bottom());
    let buf = frame.buffer_mut();
    for row_y in start_y..end_y {
        buf[(x, row_y)].set_symbol("│").set_style(style);
    }
}

fn draw_chat_switch_footer(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let target_name = app.active_title();
    let label = format!("─ Message {target_name} (Alt+T to switch) ");
    let mut line = truncate_width(&label, area.width as usize);
    let used = UnicodeWidthStr::width(line.as_str());
    for _ in used..(area.width as usize) {
        line.push('─');
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(line, ui.theme.subtle_style()))),
        area,
    );
    hitmap.add(area, HitTarget::Button(ButtonId::ComposerTargetToggle));
}

fn draw_compact_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    thread_open: bool,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let header_style = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_header)
            .fg(ui.theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(header_style), area);

    let mut pills: Vec<(&'static str, HitTarget, bool)> = vec![
        (
            " List ",
            HitTarget::Button(ButtonId::NarrowList),
            ui.narrow_pane == NarrowPane::List,
        ),
        (
            " Chat ",
            HitTarget::Button(ButtonId::NarrowChat),
            ui.narrow_pane == NarrowPane::Chat,
        ),
    ];
    if thread_open {
        pills.push((
            " Thread ",
            HitTarget::Button(ButtonId::NarrowThread),
            ui.narrow_pane == NarrowPane::Thread,
        ));
    }
    pills.push((" ⇄ ", HitTarget::Button(ButtonId::NarrowToggle), false));
    pills.push((
        " [Jump] ",
        HitTarget::Button(ButtonId::CommandPalette),
        false,
    ));
    pills.push((" [Search] ", HitTarget::Button(ButtonId::Search), false));

    let mut x = area.x;
    for (label, target, active) in pills {
        let w = label.width() as u16;
        if x.saturating_add(w) > area.right() {
            break;
        }
        let r = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        let st = if active {
            if ui.theme.no_color {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(ui.theme.accent)
                    .bg(ui.theme.bg_selected)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            header_style
        };
        frame.render_widget(Paragraph::new(label).style(st), r);
        hitmap.add(r, target);
        x = x.saturating_add(w + 1);
    }

    if x + 4 < area.right() {
        let title = truncate_width(&app.active_title(), (area.right() - x) as usize);
        let title_style = if ui.theme.no_color {
            ui.theme.muted_style()
        } else {
            ui.theme.muted_style().bg(ui.theme.bg_header)
        };
        frame.render_widget(
            Paragraph::new(title).style(title_style),
            Rect {
                x,
                y: area.y,
                width: area.right() - x,
                height: 1,
            },
        );
    }
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn draw_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bar_style = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_header)
            .fg(ui.theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(bar_style), area);

    let spinner_glyph = SPINNER_FRAMES[(ui.spinner_tick as usize) % SPINNER_FRAMES.len()];
    let (sync_glyph, sync_glyph_color, sync_tail) = if app.syncing {
        (spinner_glyph, Some(ui.theme.accent), format!(" {}", app.sync_summary))
    } else if ui.poll_retrying {
        (spinner_glyph, Some(ui.theme.warning), String::new())
    } else {
        ("✓", None, format!(" {}", app.sync_summary))
    };
    let sync_tag = format!("{sync_glyph}{sync_tail}");
    let (dot, dot_color) = ui
        .theme
        .presence_indicator(app.my_presence.as_ref().map(|p| p.availability.as_str()));
    let avail = app
        .my_presence
        .as_ref()
        .map(|p| p.availability.as_str())
        .unwrap_or("Offline");

    let focus = ui.focus();
    let mode_badge = match focus {
        Focus::Composer => " [COMPOSE] ",
        Focus::Chat => " [CHAT] ",
        Focus::Nav => " [NAV] ",
        Focus::Thread => " [THREAD] ",
        Focus::Overlay => " [MODAL] ",
    };

    let task_note = ui
        .tasks
        .first_running()
        .map(|t| format!(" · ⏳ {}", t.display()))
        .unwrap_or_default();

    let density_hint = format!("Ctrl+D {}", ui.density.label());
    let full_segments: Vec<(&str, Option<ButtonId>)> = match focus {
        Focus::Composer => vec![
            ("Enter Send · PgUp/↑↓ Scroll · Alt+1-3 Tab · ", None),
            (density_hint.as_str(), Some(ButtonId::DensityToggle)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" · Esc Browse ", None),
        ],
        Focus::Chat => vec![
            ("1-3 Tab · j/k Select · i Compose · r Reply · t Thread · + React · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        Focus::Nav => vec![
            ("1-3 Tab · j/k Move · Enter Open · u Unread · p Pin · f Filter · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        Focus::Thread => vec![
            ("j/k Select · i Reply · Alt+T Target · Esc Close · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        Focus::Overlay => vec![("↑↓ Select · Enter Confirm · Esc Close ", None)],
    };

    let compact_segments: Vec<(&str, Option<ButtonId>)> = match focus {
        Focus::Composer => vec![
            (density_hint.as_str(), Some(ButtonId::DensityToggle)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" ", None),
        ],
        Focus::Overlay => vec![("Esc Close ", None)],
        _ => vec![
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
    };

    let badge_w = mode_badge.width() as u16;
    let presence_prefix = format!("{dot} ");
    let left_rest = format!("{avail} · {sync_tag}{task_note}");
    let full_left_w = badge_w + presence_prefix.width() as u16 + left_rest.width() as u16;

    let seg_width = |segs: &[(&str, Option<ButtonId>)]| -> u16 {
        segs.iter().map(|(s, _)| s.width() as u16).sum()
    };
    let full_right_w = seg_width(&full_segments);
    let compact_right_w = seg_width(&compact_segments);

    let chosen_right: &[(&str, Option<ButtonId>)] =
        if full_left_w + 2 + full_right_w <= area.width
            || badge_w + 12 + full_right_w <= area.width
        {
            &full_segments
        } else if badge_w + 10 + compact_right_w <= area.width {
            &compact_segments
        } else {
            &[]
        };

    let right_w = seg_width(chosen_right);
    let left_budget = area
        .width
        .saturating_sub(badge_w + presence_prefix.width() as u16 + right_w + 1)
        as usize;
    let left_trimmed = truncate_width(&left_rest, left_budget);
    let left_total_w = badge_w + presence_prefix.width() as u16 + left_trimmed.width() as u16;
    let pad = area.width.saturating_sub(left_total_w + right_w) as usize;

    let badge_style = if ui.theme.no_color {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
            .fg(ui.theme.accent)
            .bg(ui.theme.bg_header)
            .add_modifier(Modifier::BOLD)
    };
    let dot_style = if ui.theme.no_color {
        bar_style
    } else {
        bar_style.fg(dot_color)
    };
    let hint_style = if ui.theme.no_color {
        ui.theme.muted_style()
    } else {
        Style::default()
            .fg(ui.theme.fg_secondary)
            .bg(ui.theme.bg_header)
    };

    let presence_prefix_w = presence_prefix.width() as u16;
    let presence_x = area.x.saturating_add(badge_w);
    let avail_vis_w = avail.width().min(left_budget) as u16;
    let presence_hit_w = presence_prefix_w
        .saturating_add(avail_vis_w)
        .min(area.right().saturating_sub(presence_x));
    if presence_hit_w > 0 {
        hitmap.add_line(
            presence_x,
            area.y,
            presence_hit_w,
            HitTarget::Button(ButtonId::Presence),
        );
    }

    let mut spans = vec![
        Span::styled(mode_badge, badge_style),
        Span::styled(presence_prefix, dot_style),
    ];
    let avail_sep = format!("{avail} · ");
    let avail_sep_w = avail_sep.width();
    let glyph_w = sync_glyph.width();
    if let Some(glyph_color) = sync_glyph_color {
        if !ui.theme.no_color && left_budget >= avail_sep_w + glyph_w {
            let after_glyph = truncate_width(
                &format!("{sync_tail}{task_note}"),
                left_budget.saturating_sub(avail_sep_w + glyph_w),
            );
            spans.push(Span::styled(avail_sep, bar_style));
            spans.push(Span::styled(sync_glyph, bar_style.fg(glyph_color)));
            if !after_glyph.is_empty() {
                spans.push(Span::styled(after_glyph, bar_style));
            }
        } else {
            spans.push(Span::styled(left_trimmed, bar_style));
        }
    } else {
        spans.push(Span::styled(left_trimmed, bar_style));
    }
    spans.push(Span::styled(" ".repeat(pad), bar_style));

    let mut rx = area.x.saturating_add(left_total_w + pad as u16);
    for (seg, btn_opt) in chosen_right {
        let sw = seg.width() as u16;
        let st = if btn_opt.is_some() {
            if ui.theme.no_color {
                hint_style.add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(ui.theme.accent_soft)
                    .bg(ui.theme.bg_header)
            }
        } else {
            hint_style
        };
        spans.push(Span::styled((*seg).to_string(), st));
        if let Some(btn) = btn_opt {
            if rx + sw <= area.right() {
                hitmap.add_line(rx, area.y, sw, HitTarget::Button(*btn));
            }
        }
        rx = rx.saturating_add(sw);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), area);
}

fn draw_toasts(frame: &mut Frame, area: Rect, ui: &UiState) {
    let Some(toast) = ui.toasts.current() else {
        return;
    };
    if area.width < 24 || area.height < 5 {
        return;
    }
    let width = (toast.message.width() + 4).clamp(20, (area.width / 2) as usize) as u16;
    let rect = Rect {
        x: area.right().saturating_sub(width + 2),
        y: area.bottom().saturating_sub(4),
        width,
        height: 3,
    };
    let color = match toast.kind {
        ToastKind::Info => ui.theme.toast,
        ToastKind::Success => ui.theme.success,
        ToastKind::Error => ui.theme.error,
    };
    let surface = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_elevated)
            .fg(ui.theme.fg_primary)
    };
    let border_st = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default().fg(color).bg(ui.theme.bg_elevated)
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(truncate_width(
            &toast.message,
            width.saturating_sub(2) as usize,
        ))
        .style(surface)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(surface)
                .border_style(border_st),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_layout_breakpoints() {
        assert_eq!(layout_mode(140), LayoutMode::Wide);
        assert_eq!(layout_mode(120), LayoutMode::Wide);
        assert_eq!(layout_mode(119), LayoutMode::Medium);
        assert_eq!(layout_mode(80), LayoutMode::Medium);
        assert_eq!(layout_mode(79), LayoutMode::Narrow);
    }

    #[test]
    fn wide_layout_includes_thread_drawer_when_open() {
        let area = Rect::new(0, 0, 140, 40);
        let closed = compute_layout(area, true, false, true, NarrowPane::Chat, 4);
        assert!(closed.top_bar.is_none());
        assert!(closed.rail.is_none());
        assert_eq!(closed.nav.map(|r| r.width), Some(28));
        assert!(closed.chat.is_some());
        assert_eq!(
            closed.nav.unwrap().right() + 1,
            closed.chat.unwrap().x
        );
        assert!(closed.thread.is_none());

        let open = compute_layout(area, true, true, true, NarrowPane::Chat, 4);
        assert!(open.chat.is_some());
        assert!(open.thread.is_some());
        assert_eq!(
            open.chat.unwrap().right() + 1,
            open.thread.unwrap().x
        );
        assert!(open.composer.is_some());
    }

    #[test]
    fn minimal_chrome_layout_supports_two_row_composer_and_medium_breakpoints() {
        let wide = Rect::new(0, 0, 140, 40);
        let min_comp = compute_layout_with_options(
            wide,
            false,
            false,
            true,
            NarrowPane::Chat,
            2,
            true,
            true,
        );
        assert!(min_comp.top_bar.is_none());
        assert!(min_comp.rail.is_none());
        assert!(min_comp.nav.is_none());
        assert_eq!(min_comp.composer.map(|r| r.height), Some(2));

        let med100 = compute_layout(Rect::new(0, 0, 100, 30), true, false, true, NarrowPane::Chat, 2);
        assert_eq!(med100.nav.map(|r| r.width), Some(26));
        assert_eq!(med100.nav.unwrap().right() + 1, med100.chat.unwrap().x);

        let med85 = compute_layout(Rect::new(0, 0, 85, 30), true, false, true, NarrowPane::Chat, 2);
        assert_eq!(med85.nav.map(|r| r.width), Some(24));
        assert_eq!(med85.nav.unwrap().right() + 1, med85.chat.unwrap().x);
    }

    #[test]
    fn narrow_layout_switches_single_pane() {
        let area = Rect::new(0, 0, 72, 24);
        let list = compute_layout(area, true, true, true, NarrowPane::List, 3);
        assert!(list.nav.is_some());
        assert!(list.chat.is_none());
        assert!(list.composer.is_none());

        let chat = compute_layout(area, true, true, true, NarrowPane::Chat, 3);
        assert!(chat.nav.is_none());
        assert!(chat.chat.is_some());
        assert!(chat.composer.is_some());

        let thread = compute_layout(area, true, true, true, NarrowPane::Thread, 3);
        assert!(thread.thread.is_some());
        assert!(thread.composer.is_some());
    }

    #[test]
    fn truncate_width_handles_multibyte_korean() {
        assert_eq!(truncate_width("안녕하세요", 4), "안…");
        assert_eq!(truncate_width("abc", 5), "abc");
    }

    #[tokio::test]
    async fn draw_resets_hidden_pane_rects_and_medium_thread_scrolls_thread() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = crate::ui::app::tests::make_test_app();
        let conv = "chan:t1:ch1";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(crate::models::ConversationTarget::Channel {
            team_id: "t1".to_string(),
            channel_id: "ch1".to_string(),
        });
        let root = crate::models::MessageItem {
            msg_key: format!("{conv}|r1"),
            conv_key: conv.to_string(),
            msg_id: "r1".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-20T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: "<p>root</p>".to_string(),
            body_plain: "root".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 1,
            web_url: None,
        };
        app.messages = vec![root.clone()];
        app.active_thread = Some(crate::models::ThreadView {
            conv_key: conv.to_string(),
            root,
            replies: Vec::new(),
        });

        // 1) Wide mode with nav rail visible: nav_area and chat_scroll.area are populated.
        let mut wide_term = Terminal::new(TestBackend::new(140, 30)).unwrap();
        wide_term.draw(|f| {
            draw(f, &mut app, &mut ui);
        }).unwrap();
        assert!(ui.nav_area.width > 0);
        assert!(ui.chat_scroll.area.width > 0);
        assert_eq!(ui.thread_scroll.area, Rect::default());

        // 2) Hide nav rail (Ctrl+B): nav_area must be cleared so it cannot hijack wheel/clicks.
        ui.show_nav_rail = false;
        wide_term.draw(|f| {
            draw(f, &mut app, &mut ui);
        }).unwrap();
        assert_eq!(ui.nav_area, Rect::default());
        assert!(ui.chat_scroll.area.width > 0);

        // 3) Medium mode with Thread open: chat_scroll.area must be cleared; hovering over composer scrolls Thread.
        ui.show_nav_rail = true;
        ui.thread_drawer_open = true;
        let mut med_term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        med_term.draw(|f| {
            draw(f, &mut app, &mut ui);
        }).unwrap();
        assert_eq!(ui.chat_scroll.area, Rect::default());
        assert!(ui.thread_scroll.area.width > 0);

        ui.thread_scroll.total_lines = 40;
        ui.chat_scroll.total_lines = 40;
        ui.thread_scroll.bottom();
        ui.chat_scroll.bottom();
        // Scroll over composer row (bottom of right pane, outside nav_area and outside thread_scroll.area)
        let composer_x = ui.thread_scroll.area.x + 2;
        let composer_y = 27;
        crate::ui::action::dispatch(
            crate::ui::action::Action::MouseScroll {
                delta: -3,
                column: composer_x,
                row: composer_y,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.thread_scroll.scroll_up > 0, "Medium thread mode should scroll Thread");
        assert_eq!(ui.chat_scroll.scroll_up, 0, "Hidden Chat must not be scrolled");

        app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }
}
