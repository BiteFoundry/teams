//! Left navigation pane:
//! - Tab 0 (`Ctrl+1`): Chats (1:1 · Group · Meeting)
//! - Tab 1 (`Ctrl+2`): Teams / Channels
//! - Tab 2 (`Ctrl+3`): Activity (`@mentions`)

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    models::ConversationKind,
    ui::{
        app::{AppState, UiState},
        chat_view::DualHasher,
        hitmap::{ButtonId, HitMap, HitTarget},
    },
};

pub const TAB_CHATS: usize = 0;
pub const TAB_TEAMS: usize = 1;
pub const TAB_ACTIVITY: usize = 2;
pub const TAB_COUNT: usize = 3;

pub const TAB_LABELS: [&str; TAB_COUNT] = ["Chats", "Teams", "Activity"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    Chats = 0,
    Teams = 1,
    Activity = 2,
}

impl NavTab {
    pub fn from_index(idx: usize) -> Self {
        match idx {
            TAB_TEAMS => Self::Teams,
            TAB_ACTIVITY => Self::Activity,
            _ => Self::Chats,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    SectionHeader {
        label: &'static str,
    },
    Chat {
        conv_key: String,
        title: String,
        kind: ConversationKind,
        peer_user_id: Option<String>,
        unread: i32,
    },
    TeamHeader {
        team_id: String,
        display_name: String,
        collapsed: bool,
        channel_count: usize,
        unread: i32,
    },
    TeamChannel {
        conv_key: String,
        display_name: String,
        membership: ChannelMembership,
        unread: i32,
    },
    ActivityMention {
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
        author_name: String,
        preview: String,
        created_at: String,
    },
}

/// Who can see a channel, from Graph's `membershipType`; it picks the row's icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelMembership {
    Standard,
    Private,
    /// Shared with people outside the team, including channels another team shares in.
    Shared,
}

impl ChannelMembership {
    pub fn from_membership_type(membership_type: &str) -> Self {
        let kind = membership_type.trim();
        if kind.eq_ignore_ascii_case("standard") {
            Self::Standard
        } else if kind.eq_ignore_ascii_case("shared") {
            Self::Shared
        } else {
            Self::Private
        }
    }

    /// The icon cell drawn before the channel name.
    fn icon(self) -> &'static str {
        match self {
            Self::Standard => "  # ",
            Self::Private => "  🔒 ",
            Self::Shared => "  ⇄ ",
        }
    }
}

impl NavRow {
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::SectionHeader { .. })
    }
}

const NAV_CACHE_CAPACITY: usize = 16;

struct NavRowsCache {
    entries: Vec<((u64, u64), Arc<[NavRow]>)>,
    max_entries: usize,
}

impl NavRowsCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries: max_entries.max(1),
        }
    }

    fn get(&mut self, key: (u64, u64)) -> Option<Arc<[NavRow]>> {
        let pos = self.entries.iter().position(|(k, _)| *k == key)?;
        if pos + 1 < self.entries.len() {
            let entry = self.entries.remove(pos);
            self.entries.push(entry);
        }
        self.entries.last().map(|(_, val)| Arc::clone(val))
    }

    fn insert(&mut self, key: (u64, u64), val: Arc<[NavRow]>) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push((key, val));
    }
}

thread_local! {
    static NAV_ROWS_CACHE: RefCell<NavRowsCache> = RefCell::new(NavRowsCache::new(NAV_CACHE_CAPACITY));
}

#[inline]
fn write_opt_str(h: &mut DualHasher, s: Option<&str>) {
    match s {
        Some(v) => {
            h.write_hash(&1u8);
            h.write_str(v);
        }
        None => h.write_hash(&0u8),
    }
}

/// Computes a 128-bit signature of all inputs that affect `NavRow` output for the active tab.
fn compute_nav_sig(app: &AppState, ui: &UiState) -> (u64, u64) {
    let mut h = DualHasher::new(0x4e41_565f_524f_5753);
    h.write_hash(&ui.active_nav_tab);
    h.write_hash(&ui.nav_unread_only);
    match ui.active_nav_tab {
        TAB_CHATS => {
            let mut pinned_keys: Vec<&str> = ui.pinned_convs.iter().map(String::as_str).collect();
            pinned_keys.sort_unstable();
            h.write_hash(&pinned_keys.len());
            for key in pinned_keys {
                h.write_str(key);
            }
            let mut count = 0usize;
            for c in &app.conversations {
                if c.kind != ConversationKind::Channel {
                    h.write_str(&c.conv_key);
                    h.write_str(&c.title);
                    h.write_hash(&c.kind);
                    write_opt_str(&mut h, c.peer_user_id.as_deref());
                    h.write_hash(&c.unread_count);
                    h.write_hash(&c.pinned);
                    count += 1;
                }
            }
            h.write_hash(&count);
        }
        TAB_TEAMS => {
            h.write_hash(&app.teams.len());
            for team in &app.teams {
                h.write_str(&team.id);
                h.write_str(&team.display_name);
                let collapsed = ui.collapsed_team_ids.contains(&team.id);
                h.write_hash(&collapsed);
                h.write_hash(&team.channels.len());
                for ch in &team.channels {
                    h.write_str(&ch.team_id);
                    h.write_str(&ch.id);
                    h.write_str(&ch.display_name);
                    h.write_hash(&ChannelMembership::from_membership_type(&ch.membership_type));
                }
            }
            let mut chan_conv_count = 0usize;
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel || c.conv_key.starts_with("chan:") {
                    h.write_str(&c.conv_key);
                    h.write_hash(&c.unread_count);
                    chan_conv_count += 1;
                }
            }
            h.write_hash(&chan_conv_count);
        }
        TAB_ACTIVITY => {
            h.write_hash(&app.activity_mentions.len());
            for m in &app.activity_mentions {
                h.write_str(&m.conv_key);
                h.write_str(&m.msg_id);
                write_opt_str(&mut h, m.parent_msg_id.as_deref());
                h.write_str(&m.author_name);
                h.write_str(&m.body_plain);
                h.write_str(&m.created_at);
            }
        }
        _ => {}
    }
    h.finish()
}

/// Returns the cached `Arc<[NavRow]>` slice for the active tab, rebuilding only on signature miss.
pub fn nav_rows_cached(app: &AppState, ui: &UiState) -> Arc<[NavRow]> {
    let sig = compute_nav_sig(app, ui);
    if let Some(hit) = NAV_ROWS_CACHE.with(|cache| cache.borrow_mut().get(sig)) {
        return hit;
    }
    let rows: Arc<[NavRow]> = build_nav_rows_uncached(app, ui).into();
    NAV_ROWS_CACHE.with(|cache| cache.borrow_mut().insert(sig, Arc::clone(&rows)));
    rows
}

#[inline]
fn is_conv_pinned(conv_key: &str, conv_pinned: bool, ui: &UiState) -> bool {
    conv_pinned || ui.pinned_convs.contains(conv_key)
}

/// Returns the number of visible rows in the active nav tab without allocating rows.
pub fn nav_row_count(app: &AppState, ui: &UiState) -> usize {
    match ui.active_nav_tab {
        TAB_CHATS => {
            let mut pinned = 0usize;
            let mut unread = 0usize;
            let mut recent = 0usize;
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel {
                    continue;
                }
                if ui.nav_unread_only && c.unread_count <= 0 {
                    continue;
                }
                if is_conv_pinned(&c.conv_key, c.pinned, ui) {
                    pinned += 1;
                } else if c.unread_count > 0 {
                    unread += 1;
                } else {
                    recent += 1;
                }
            }
            if pinned == 0 && unread == 0 {
                recent
            } else {
                (if pinned > 0 { 1 + pinned } else { 0 })
                    + (if unread > 0 { 1 + unread } else { 0 })
                    + (if recent > 0 { 1 + recent } else { 0 })
            }
        }
        TAB_TEAMS => {
            if !ui.nav_unread_only {
                return app
                    .teams
                    .iter()
                    .map(|team| {
                        1 + if ui.collapsed_team_ids.contains(&team.id) {
                            0
                        } else {
                            team.channels.len()
                        }
                    })
                    .sum();
            }
            let mut channel_unread: HashMap<&str, i32> =
                HashMap::with_capacity(app.conversations.len());
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel || c.conv_key.starts_with("chan:") {
                    channel_unread
                        .entry(c.conv_key.as_str())
                        .or_insert(c.unread_count);
                }
            }
            let mut total = 0usize;
            for team in &app.teams {
                let unread_chans = team
                    .channels
                    .iter()
                    .filter(|ch| channel_unread.get(ch.conv_key().as_str()).copied().unwrap_or(0) > 0)
                    .count();
                if unread_chans > 0 {
                    total += 1
                        + if ui.collapsed_team_ids.contains(&team.id) {
                            0
                        } else {
                            unread_chans
                        };
                }
            }
            total
        }
        TAB_ACTIVITY => app.activity_mentions.len(),
        _ => 0,
    }
}

pub fn nav_row_at(app: &AppState, ui: &UiState, index: usize) -> Option<NavRow> {
    nav_rows_cached(app, ui).get(index).cloned()
}

pub fn build_nav_rows(app: &AppState, ui: &UiState) -> Vec<NavRow> {
    nav_rows_cached(app, ui).to_vec()
}

fn build_nav_rows_uncached(app: &AppState, ui: &UiState) -> Vec<NavRow> {
    match ui.active_nav_tab {
        TAB_CHATS => {
            let mut pinned = Vec::new();
            let mut unread = Vec::new();
            let mut recent = Vec::new();
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel {
                    continue;
                }
                if ui.nav_unread_only && c.unread_count <= 0 {
                    continue;
                }
                let row = NavRow::Chat {
                    conv_key: c.conv_key.clone(),
                    title: c.title.clone(),
                    kind: c.kind,
                    peer_user_id: c.peer_user_id.clone(),
                    unread: c.unread_count,
                };
                if is_conv_pinned(&c.conv_key, c.pinned, ui) {
                    pinned.push(row);
                } else if c.unread_count > 0 {
                    unread.push(row);
                } else {
                    recent.push(row);
                }
            }
            if pinned.is_empty() && unread.is_empty() {
                return recent;
            }
            let mut rows = Vec::with_capacity(pinned.len() + unread.len() + recent.len() + 3);
            if !pinned.is_empty() {
                rows.push(NavRow::SectionHeader { label: "PINNED" });
                rows.extend(pinned);
            }
            if !unread.is_empty() {
                rows.push(NavRow::SectionHeader { label: "UNREAD" });
                rows.extend(unread);
            }
            if !recent.is_empty() {
                rows.push(NavRow::SectionHeader { label: "RECENT" });
                rows.extend(recent);
            }
            rows
        }
        TAB_TEAMS => {
            let mut channel_unread: HashMap<&str, i32> =
                HashMap::with_capacity(app.conversations.len());
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel || c.conv_key.starts_with("chan:") {
                    channel_unread
                        .entry(c.conv_key.as_str())
                        .or_insert(c.unread_count);
                }
            }
            let mut rows = Vec::new();
            for team in &app.teams {
                let collapsed = ui.collapsed_team_ids.contains(&team.id);
                let team_unread: i32 = team
                    .channels
                    .iter()
                    .map(|ch| {
                        channel_unread
                            .get(ch.conv_key().as_str())
                            .copied()
                            .unwrap_or(0)
                            .max(0)
                    })
                    .sum();
                if ui.nav_unread_only && team_unread <= 0 {
                    continue;
                }
                rows.push(NavRow::TeamHeader {
                    team_id: team.id.clone(),
                    display_name: team.display_name.clone(),
                    collapsed,
                    channel_count: team.channels.len(),
                    unread: team_unread,
                });
                if !collapsed {
                    for ch in &team.channels {
                        let conv_key = ch.conv_key();
                        let unread = channel_unread.get(conv_key.as_str()).copied().unwrap_or(0);
                        if ui.nav_unread_only && unread <= 0 {
                            continue;
                        }
                        rows.push(NavRow::TeamChannel {
                            conv_key,
                            display_name: ch.display_name.clone(),
                            membership: ChannelMembership::from_membership_type(
                                &ch.membership_type,
                            ),
                            unread,
                        });
                    }
                }
            }
            rows
        }
        TAB_ACTIVITY => app
            .activity_mentions
            .iter()
            .map(|m| NavRow::ActivityMention {
                conv_key: m.conv_key.clone(),
                msg_id: m.msg_id.clone(),
                parent_msg_id: m.parent_msg_id.clone(),
                author_name: m.author_name.clone(),
                preview: m.body_plain.replace(['\n', '\r'], " "),
                created_at: m.created_at.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn tab_counts(app: &AppState) -> [usize; TAB_COUNT] {
    let chat_count = app
        .conversations
        .iter()
        .filter(|c| c.kind != ConversationKind::Channel)
        .count();
    let team_channel_count: usize = app.teams.iter().map(|t| t.channels.len()).sum();
    [chat_count, team_channel_count, app.activity_mentions.len()]
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    if area.width == 0 || area.height == 0 {
        ui.nav_area = Rect::default();
        return;
    }

    let theme = &ui.theme;
    let sidebar_style = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(sidebar_style), area);

    draw_tab_strip(frame, area, app, ui, hitmap);

    if area.height < 2 {
        ui.nav_area = Rect::default();
        return;
    }

    draw_filter_strip(frame, area, ui, hitmap);

    let show_loading =
        ui.active_nav_tab == TAB_CHATS && app.loading_older_chats && area.height > 3;
    let list_y = area.y.saturating_add(2);
    let list_h = area
        .height
        .saturating_sub(2 + if show_loading { 1 } else { 0 });
    let list_area = Rect::new(area.x, list_y, area.width, list_h);
    ui.nav_area = list_area;

    let rows = nav_rows_cached(app, ui);

    if list_area.width > 0 && list_area.height > 0 {
        if rows.is_empty() {
            let empty_msg = match ui.active_nav_tab {
                TAB_CHATS => {
                    if ui.nav_unread_only {
                        "No unread chats"
                    } else {
                        "No chats yet"
                    }
                }
                TAB_TEAMS => {
                    if ui.nav_unread_only {
                        "No unread channels"
                    } else {
                        "No teams joined"
                    }
                }
                TAB_ACTIVITY => "No @mentions yet",
                _ => "No items",
            };
            let empty_style = if theme.no_color {
                theme.muted_style()
            } else {
                theme.muted_style().bg(theme.bg_sidebar)
            };
            frame.render_widget(
                Paragraph::new(format!(" {}", truncate(empty_msg, (list_area.width as usize).saturating_sub(1))))
                    .style(empty_style),
                list_area,
            );
        } else {
            let selected = ui.nav_selected.min(rows.len().saturating_sub(1));
            let height = list_area.height as usize;
            let offset = selection_offset(selected, height, rows.len());
            let width = list_area.width as usize;

            let mut lines = Vec::with_capacity(height);
            for (idx, row) in rows.iter().enumerate().skip(offset).take(height) {
                let is_selected = idx == selected && row.is_selectable();
                let is_active = match row {
                    NavRow::Chat { conv_key, .. } | NavRow::TeamChannel { conv_key, .. } => {
                        app.active_conv_key.as_deref() == Some(conv_key.as_str())
                    }
                    _ => false,
                };
                lines.push(render_nav_row(row, width, is_selected, is_active, app, ui));
                if row.is_selectable() {
                    hitmap.add_line(
                        list_area.x,
                        list_area.y + lines.len() as u16 - 1,
                        list_area.width,
                        HitTarget::NavRow(idx),
                    );
                }
            }
            frame.render_widget(Paragraph::new(lines).style(sidebar_style), list_area);
        }
    }

    if show_loading {
        let status_y = area.bottom().saturating_sub(1);
        let status_style = if theme.no_color {
            theme.muted_style()
        } else {
            theme.muted_style().bg(theme.bg_sidebar)
        };
        frame.render_widget(
            Paragraph::new(truncate(" Loading older chats…", area.width as usize)).style(status_style),
            Rect::new(area.x, status_y, area.width, 1),
        );
    }
}

fn draw_tab_strip(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let header_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_secondary)
    };
    let row_rect = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(Paragraph::new("").style(header_bg), row_rect);

    let unread_chats = app.unread_chat_count();
    let unread_chans = app.unread_channel_count();
    let mentions = app.activity_mentions.len();

    let labels = [
        if unread_chats > 0 {
            format!(" 1 Chats ({unread_chats}) ")
        } else {
            " 1 Chats ".to_string()
        },
        if unread_chans > 0 {
            format!(" 2 Teams ({unread_chans}) ")
        } else {
            " 2 Teams ".to_string()
        },
        if mentions > 0 {
            format!(" 3 Act ({mentions}) ")
        } else {
            " 3 Act ".to_string()
        },
    ];

    let mut x = area.x;
    for (idx, label) in labels.iter().enumerate() {
        let avail = area.right().saturating_sub(x) as usize;
        if avail == 0 {
            break;
        }
        let clipped = truncate(label, avail);
        let w = UnicodeWidthStr::width(clipped.as_str()) as u16;
        if w == 0 {
            break;
        }
        let active = ui.active_nav_tab == idx;
        let style = if active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_header)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            }
        } else {
            header_bg
        };
        frame.render_widget(
            Paragraph::new(Span::styled(clipped, style)),
            Rect::new(x, area.y, w, 1),
        );
        hitmap.add_line(x, area.y, w, HitTarget::NavTab(idx));
        x = x.saturating_add(w);
    }
}

fn draw_filter_strip(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let y = area.y.saturating_add(1);
    let row_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_sidebar)
    };
    frame.render_widget(
        Paragraph::new("").style(row_bg),
        Rect::new(area.x, y, area.width, 1),
    );

    let filter_label = if ui.nav_unread_only {
        "[Unread]"
    } else {
        "[All]"
    };
    let new_label = "[+New]";
    let filter_w = UnicodeWidthStr::width(filter_label) as u16;
    let new_w = UnicodeWidthStr::width(new_label) as u16;
    let right_w = filter_w + 1 + new_w;

    let left_budget = area.width.saturating_sub(right_w + 1) as usize;
    let jump_label = if left_budget >= 14 {
        " Ctrl+K Jump… "
    } else if left_budget >= 8 {
        " Ctrl+K "
    } else {
        ""
    };

    if !jump_label.is_empty() {
        let jw = UnicodeWidthStr::width(jump_label) as u16;
        let jump_style = if theme.no_color {
            theme.muted_style()
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_sidebar)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(jump_label, jump_style)),
            Rect::new(area.x, y, jw, 1),
        );
        hitmap.add_line(area.x, y, jw, HitTarget::Button(ButtonId::CommandPalette));
    }

    if area.width >= right_w {
        let filter_x = area.right().saturating_sub(right_w);
        let new_x = area.right().saturating_sub(new_w);

        let filter_style = if ui.nav_unread_only {
            if theme.no_color {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_selected)
                    .add_modifier(Modifier::BOLD)
            }
        } else if theme.no_color {
            theme.muted_style()
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_sidebar)
        };

        let new_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.accent)
                .bg(theme.bg_sidebar)
                .add_modifier(Modifier::BOLD)
        };

        frame.render_widget(
            Paragraph::new(Span::styled(filter_label, filter_style)),
            Rect::new(filter_x, y, filter_w, 1),
        );
        hitmap.add_line(filter_x, y, filter_w, HitTarget::Button(ButtonId::FilterToggle));

        frame.render_widget(
            Paragraph::new(Span::styled(new_label, new_style)),
            Rect::new(new_x, y, new_w, 1),
        );
        hitmap.add_line(new_x, y, new_w, HitTarget::Button(ButtonId::NewChat));
    }
}

const MARKER_WIDTH: usize = 2;

const SPACES: &str = "                                                                                                                                ";

#[inline]
fn spaces_cow(n: usize) -> Cow<'static, str> {
    if n <= SPACES.len() {
        Cow::Borrowed(&SPACES[..n])
    } else {
        Cow::Owned(" ".repeat(n))
    }
}

fn render_nav_row(
    row: &NavRow,
    width: usize,
    selected: bool,
    active: bool,
    app: &AppState,
    ui: &UiState,
) -> Line<'static> {
    let theme = &ui.theme;
    let sidebar_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)
    };

    if let NavRow::SectionHeader { label } = row {
        let hdr_text = truncate(label, width.saturating_sub(2));
        let pad = width.saturating_sub(2 + UnicodeWidthStr::width(hdr_text.as_str()));
        let hdr_style = if theme.no_color {
            theme.muted_style().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.fg_muted)
                .bg(theme.bg_sidebar)
                .add_modifier(Modifier::BOLD)
        };
        let mut spans = vec![
            Span::styled("  ", sidebar_bg),
            Span::styled(hdr_text, hdr_style),
        ];
        if pad > 0 {
            spans.push(Span::styled(spaces_cow(pad), sidebar_bg));
        }
        return Line::from(spans);
    }

    let marker: &'static str = if selected {
        "▎ "
    } else if active {
        "• "
    } else {
        "  "
    };

    let base_style = if selected {
        theme.nav_selection()
    } else if active {
        if theme.no_color {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.accent)
                .bg(theme.bg_sidebar)
                .add_modifier(Modifier::BOLD)
        }
    } else {
        sidebar_bg
    };

    let marker_style = if selected {
        if theme.no_color || theme.ansi16_only {
            base_style
        } else {
            Style::default()
                .fg(theme.accent)
                .bg(theme.bg_selected)
                .add_modifier(Modifier::BOLD)
        }
    } else if active {
        base_style
    } else {
        sidebar_bg
    };

    match row {
        NavRow::SectionHeader { .. } => unreachable!(),
        NavRow::Chat {
            title,
            kind,
            peer_user_id,
            unread,
            ..
        } => {
            let (prefix_glyph, glyph_color): (Cow<'static, str>, Color) = match kind {
                ConversationKind::OneOnOne => {
                    let avail = peer_user_id
                        .as_deref()
                        .and_then(|id| app.presences.get(id))
                        .map(|p| p.availability.as_str());
                    let (dot, color) = theme.presence_indicator(avail);
                    (Cow::Owned(format!("{dot} ")), color)
                }
                ConversationKind::Group => (Cow::Borrowed("[Group] "), theme.accent),
                ConversationKind::Meeting => (Cow::Borrowed("[Meeting] "), theme.reply),
                ConversationKind::Channel => (Cow::Borrowed("# "), theme.accent),
            };
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let used_fixed = MARKER_WIDTH
                + UnicodeWidthStr::width(prefix_glyph.as_ref())
                + badge.len();
            let name_w = width.saturating_sub(used_fixed).max(4);
            let (name, name_used) = truncate_with_width(title, name_w);
            let pad = width.saturating_sub(used_fixed + name_used);

            let glyph_style = if selected {
                base_style
            } else if theme.no_color {
                Style::default().fg(glyph_color)
            } else {
                Style::default().fg(glyph_color).bg(theme.bg_sidebar)
            };
            let name_style = if !selected && *unread > 0 {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let mut spans = Vec::with_capacity(5);
            spans.push(Span::styled(marker, marker_style));
            spans.push(Span::styled(prefix_glyph, glyph_style));
            spans.push(Span::styled(name, name_style));
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), base_style));
            }
            if !badge.is_empty() {
                let badge_style = if selected {
                    base_style
                } else if theme.no_color {
                    Style::default()
                        .fg(theme.unread)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.unread)
                        .bg(theme.bg_sidebar)
                        .add_modifier(Modifier::BOLD)
                };
                spans.push(Span::styled(badge, badge_style));
            }
            Line::from(spans)
        }
        NavRow::TeamHeader {
            display_name,
            collapsed,
            channel_count,
            unread,
            ..
        } => {
            let arrow = if *collapsed { "▸ " } else { "▾ " };
            let suffix = if *unread > 0 {
                format!(" ({unread})")
            } else {
                format!(" ({channel_count})")
            };
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(arrow) + suffix.len();
            let (name, name_used) =
                truncate_with_width(display_name, width.saturating_sub(fixed).max(4));
            let pad = width.saturating_sub(fixed + name_used);
            let style = if selected {
                base_style
            } else if theme.no_color {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_sidebar)
                    .add_modifier(Modifier::BOLD)
            };
            let suffix_style = if selected {
                base_style
            } else if *unread > 0 {
                if theme.no_color {
                    Style::default()
                        .fg(theme.unread)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.unread)
                        .bg(theme.bg_sidebar)
                        .add_modifier(Modifier::BOLD)
                }
            } else if theme.no_color {
                theme.muted_style()
            } else {
                theme.muted_style().bg(theme.bg_sidebar)
            };
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled(arrow, style),
                Span::styled(name, style),
            ];
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), base_style));
            }
            spans.push(Span::styled(suffix, suffix_style));
            Line::from(spans)
        }
        NavRow::TeamChannel {
            display_name,
            membership,
            unread,
            ..
        } => {
            let icon = membership.icon();
            let icon_w = UnicodeWidthStr::width(icon);
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let fixed = MARKER_WIDTH + icon_w + badge.len();
            let (name, name_used) =
                truncate_with_width(display_name, width.saturating_sub(fixed).max(4));
            let pad = width.saturating_sub(fixed + name_used);
            let name_style = if !selected && *unread > 0 {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let icon_style = if selected {
                base_style
            } else if theme.no_color {
                theme.muted_style()
            } else {
                theme.muted_style().bg(theme.bg_sidebar)
            };
            let mut spans = Vec::with_capacity(5);
            spans.push(Span::styled(marker, marker_style));
            spans.push(Span::styled(icon, icon_style));
            spans.push(Span::styled(name, name_style));
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), base_style));
            }
            if !badge.is_empty() {
                let badge_style = if selected {
                    base_style
                } else if theme.no_color {
                    Style::default()
                        .fg(theme.unread)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(theme.unread)
                        .bg(theme.bg_sidebar)
                        .add_modifier(Modifier::BOLD)
                };
                spans.push(Span::styled(badge, badge_style));
            }
            Line::from(spans)
        }
        NavRow::ActivityMention {
            author_name,
            preview,
            ..
        } => {
            let label = format!("@{author_name}: {preview}");
            let (body, body_used) =
                truncate_with_width(&label, width.saturating_sub(MARKER_WIDTH));
            let pad = width.saturating_sub(MARKER_WIDTH + body_used);
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled(body, base_style),
            ];
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), base_style));
            }
            Line::from(spans)
        }
    }
}

pub fn selection_offset(selected: usize, height: usize, total: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        0
    } else {
        (selected + 1)
            .saturating_sub(height)
            .min(total.saturating_sub(height))
    }
}

fn truncate_with_width(text: &str, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        if text.len() <= width {
            return (text.to_string(), text.len());
        }
        return (text[..width].to_string(), width);
    }
    let mut out = String::with_capacity(text.len().min(width.saturating_mul(3)));
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    (out, used)
}

fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.is_ascii() {
        if text.len() <= width {
            return text.to_string();
        }
        if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return text[..width].to_string();
        }
    }
    truncate_with_width(text, width).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_tabs_map_roundtrip() {
        assert_eq!(TAB_COUNT, 3);
        assert_eq!(TAB_LABELS.len(), TAB_COUNT);
        assert_eq!(TAB_LABELS[TAB_CHATS], "Chats");
        assert_eq!(TAB_LABELS[TAB_TEAMS], "Teams");
        assert_eq!(TAB_LABELS[TAB_ACTIVITY], "Activity");
        for i in 0..TAB_COUNT {
            let tab = NavTab::from_index(i);
            assert_eq!(tab.index(), i);
        }
        assert_eq!(UnicodeWidthStr::width("▎ "), MARKER_WIDTH);
    }

    #[test]
    fn truncate_and_spaces_helpers_handle_ascii_and_wide_chars() {
        assert_eq!(UnicodeWidthStr::width("▸ "), MARKER_WIDTH);
        assert_eq!(UnicodeWidthStr::width("• "), MARKER_WIDTH);
        assert_eq!(UnicodeWidthStr::width("  "), MARKER_WIDTH);

        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("hello", 0), "");

        let (s, w) = truncate_with_width("한글ABC", 4);
        assert_eq!(s, "한글");
        assert_eq!(w, 4);

        let (s2, w2) = truncate_with_width("한글ABC", 5);
        assert_eq!(s2, "한글A");
        assert_eq!(w2, 5);

        assert_eq!(spaces_cow(0).as_ref(), "");
        assert_eq!(spaces_cow(5).as_ref(), "     ");
        assert!(matches!(spaces_cow(128), Cow::Borrowed(_)));
        assert_eq!(spaces_cow(140).len(), 140);
    }

    fn make_populated_test_state() -> (AppState, UiState, crate::sync::SyncController) {
        use crate::auth::AuthManager;
        use crate::config::AppConfig;
        use crate::graph::GraphClient;
        use crate::models::{
            ChannelSummary, ConversationSummary, ConversationTarget, MessageItem, TeamSummary,
        };
        use crate::storage::Storage;
        use crate::sync;

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();

        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl.clone(), ui_tx);
        let ui = UiState::new();

        app.conversations = vec![
            ConversationSummary {
                conv_key: "chat:c1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c1".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Alice".to_string(),
                topic: None,
                unread_count: 2,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: Some("u1".to_string()),
                web_url: None,
            },
            ConversationSummary {
                conv_key: "chat:c2".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c2".to_string(),
                },
                kind: ConversationKind::Group,
                team_id: None,
                team_name: None,
                title: "Design Sync".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            },
            ConversationSummary {
                conv_key: "chan:t1:ch1".to_string(),
                target: ConversationTarget::Channel {
                    team_id: "t1".to_string(),
                    channel_id: "ch1".to_string(),
                },
                kind: ConversationKind::Channel,
                team_id: Some("t1".to_string()),
                team_name: Some("Core".to_string()),
                title: "General".to_string(),
                topic: None,
                unread_count: 5,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            },
        ];

        app.teams = vec![
            TeamSummary {
                id: "t1".to_string(),
                display_name: "Core".to_string(),
                description: None,
                channels: vec![
                    ChannelSummary {
                        id: "ch1".to_string(),
                        team_id: "t1".to_string(),
                        team_name: "Core".to_string(),
                        display_name: "General".to_string(),
                        description: None,
                        membership_type: "standard".to_string(),
                        web_url: None,
                    },
                    ChannelSummary {
                        id: "ch2".to_string(),
                        team_id: "t1".to_string(),
                        team_name: "Core".to_string(),
                        display_name: "Secret".to_string(),
                        description: None,
                        membership_type: "private".to_string(),
                        web_url: None,
                    },
                ],
            },
            TeamSummary {
                id: "t2".to_string(),
                display_name: "Infra".to_string(),
                description: None,
                channels: vec![ChannelSummary {
                    id: "ch3".to_string(),
                    team_id: "t2".to_string(),
                    team_name: "Infra".to_string(),
                    display_name: "Alerts".to_string(),
                    description: None,
                    membership_type: "standard".to_string(),
                    web_url: None,
                }],
            },
        ];

        app.activity_mentions = vec![MessageItem {
            msg_key: "chan:t1:ch1|m1".to_string(),
            conv_key: "chan:t1:ch1".to_string(),
            msg_id: "m1".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Bob".to_string(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: String::new(),
            body_plain: "ping\nline2".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }];

        (app, ui, sync_ctrl)
    }

    #[tokio::test]
    async fn nav_row_count_and_nav_row_at_match_build_nav_rows_across_all_tabs() {
        let (app, mut ui, sync_ctrl) = make_populated_test_state();

        for tab in 0..TAB_COUNT {
            ui.active_nav_tab = tab;
            let expected = build_nav_rows(&app, &ui);
            assert_eq!(
                nav_row_count(&app, &ui),
                expected.len(),
                "row count mismatch on tab {tab}"
            );
            for (idx, exp_row) in expected.iter().enumerate() {
                assert_eq!(
                    nav_row_at(&app, &ui, idx).as_ref(),
                    Some(exp_row),
                    "nav_row_at mismatch on tab {tab} idx {idx}"
                );
            }
            assert_eq!(nav_row_at(&app, &ui, expected.len()), None);
        }

        // TAB_TEAMS with collapsed team
        ui.active_nav_tab = TAB_TEAMS;
        ui.collapsed_team_ids.insert("t1".to_string());
        let teams_collapsed = build_nav_rows(&app, &ui);
        assert_eq!(nav_row_count(&app, &ui), teams_collapsed.len());
        assert_eq!(teams_collapsed.len(), 3); // t1 header + t2 header + alerts

        sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn nav_rows_cached_reuses_arc_and_invalidates_on_relevant_mutations() {
        let (mut app, mut ui, sync_ctrl) = make_populated_test_state();

        // TAB_CHATS: unread_count mutation invalidates
        ui.active_nav_tab = TAB_CHATS;
        let c1 = nav_rows_cached(&app, &ui);
        let c2 = nav_rows_cached(&app, &ui);
        assert!(Arc::ptr_eq(&c1, &c2));
        app.conversations[0].unread_count = 9;
        let c3 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&c1, &c3));
        assert!(matches!(
            c3.first(),
            Some(NavRow::SectionHeader { label: "UNREAD" })
        ));
        assert!(matches!(c3.get(1), Some(NavRow::Chat { unread: 9, .. })));

        // Pinning and unread filter invalidate cache and match nav_row_count
        ui.pinned_convs.insert("chat:c2".to_string());
        let c4 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&c3, &c4));
        assert!(matches!(
            c4.first(),
            Some(NavRow::SectionHeader { label: "PINNED" })
        ));
        assert_eq!(nav_row_count(&app, &ui), c4.len());

        ui.nav_unread_only = true;
        let c5 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&c4, &c5));
        assert_eq!(nav_row_count(&app, &ui), c5.len());
        assert_eq!(c5.len(), 2); // UNREAD header + Alice
        ui.nav_unread_only = false;
        ui.pinned_convs.clear();

        // TAB_TEAMS: channel unread_count & collapsed_team_ids invalidate
        ui.active_nav_tab = TAB_TEAMS;
        let t1 = nav_rows_cached(&app, &ui);
        let t2 = nav_rows_cached(&app, &ui);
        assert!(Arc::ptr_eq(&t1, &t2));

        app.conversations[2].unread_count = 12;
        let t3 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&t1, &t3));
        assert!(matches!(
            t3.first(),
            Some(NavRow::TeamHeader { unread: 12, .. })
        ));
        assert!(matches!(
            t3.get(1),
            Some(NavRow::TeamChannel { unread: 12, .. })
        ));

        ui.nav_unread_only = true;
        let t_unread = nav_rows_cached(&app, &ui);
        assert_eq!(nav_row_count(&app, &ui), t_unread.len());
        assert_eq!(t_unread.len(), 2); // Core header + General
        ui.nav_unread_only = false;

        ui.collapsed_team_ids.insert("t1".to_string());
        let t4 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&t3, &t4));
        assert_eq!(t4.len(), 3);

        sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    // Track 5: shared channels (including ones shared into the team via /allChannels) get
    // their own icon instead of the private lock.
    #[test]
    fn channel_membership_picks_shared_icon_apart_from_private_lock() {
        assert_eq!(
            ChannelMembership::from_membership_type("standard"),
            ChannelMembership::Standard
        );
        assert_eq!(
            ChannelMembership::from_membership_type("private"),
            ChannelMembership::Private
        );
        assert_eq!(
            ChannelMembership::from_membership_type(" Shared "),
            ChannelMembership::Shared
        );
        assert_eq!(
            ChannelMembership::from_membership_type("unknownFutureValue"),
            ChannelMembership::Private
        );
        assert_eq!(ChannelMembership::Standard.icon(), "  # ");
        assert_eq!(ChannelMembership::Private.icon(), "  🔒 ");
        assert_eq!(ChannelMembership::Shared.icon(), "  ⇄ ");
        assert_ne!(ChannelMembership::Shared.icon(), ChannelMembership::Private.icon());
    }

    #[tokio::test]
    async fn teams_tab_rows_mark_shared_channels() {
        let (mut app, mut ui, sync_ctrl) = make_populated_test_state();
        app.teams[1].channels.push(crate::models::ChannelSummary {
            id: "ch-shared".to_string(),
            team_id: "t2".to_string(),
            team_name: "Infra".to_string(),
            display_name: "Partner Room".to_string(),
            description: None,
            membership_type: "shared".to_string(),
            web_url: None,
        });
        ui.active_nav_tab = TAB_TEAMS;
        let rows = build_nav_rows(&app, &ui);
        let membership_of = |name: &str| {
            rows.iter().find_map(|r| match r {
                NavRow::TeamChannel {
                    display_name,
                    membership,
                    ..
                } if display_name == name => Some(*membership),
                _ => None,
            })
        };
        assert_eq!(membership_of("General"), Some(ChannelMembership::Standard));
        assert_eq!(membership_of("Secret"), Some(ChannelMembership::Private));
        assert_eq!(membership_of("Partner Room"), Some(ChannelMembership::Shared));

        // The cached rows are rebuilt when only the membership type changes.
        let before = nav_rows_cached(&app, &ui);
        app.teams[1].channels[1].membership_type = "private".to_string();
        let after = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&before, &after));

        sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }
}
