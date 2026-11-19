//! Unified Omni-Switcher & Command Palette (`Ctrl+K`).
//!
//! Supports prefix modes:
//! - (none): conversations, channels, and commands
//! - `>`: commands only
//! - `#`: channels only (with slash-path fuzzy matching such as `eng/gen`)
//! - `@`: 1:1 / group chats + `Create new chat…`
//! - `/`: hands off query to `SearchOverlay` on Enter

use std::collections::{HashMap, HashSet};

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::ConversationKind,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayKind, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Chat,
    Channel,
    Command,
    Search,
}

#[derive(Debug, Clone)]
pub struct PaletteItem {
    kind: ItemKind,
    pub label: String,
    search_alias: String,
    pub shortcut: String,
    pub action: Action,
    base_priority: i32,
}

fn palette_commands() -> Vec<PaletteItem> {
    let cmds = [
        ("Switch to Chats Tab", "1 / Alt+1", Action::SelectNavTab(0)),
        ("Switch to Teams Tab", "2 / Alt+2", Action::SelectNavTab(1)),
        (
            "Switch to Activity Tab",
            "3 / Alt+3",
            Action::SelectNavTab(2),
        ),
        ("New Chat", "Ctrl+N", Action::OpenOverlay(OverlayKind::NewChat)),
        ("Search Messages", "/", Action::OpenSearch),
        ("Browse & Attach File (File Picker)", "Ctrl+F", Action::OpenFilePicker),
        ("Extract Links (URLs)", "u", Action::OpenLinks),
        ("Toggle Inline Link URLs (href)", "h", Action::ToggleShowHrefs),
        ("Jump to Next Unread", "u (Nav) / Alt+u", Action::JumpNextUnread),
        ("Jump to Previous Unread", "U", Action::JumpPrevUnread),
        (
            "Toggle Last Conversation",
            "`",
            Action::ToggleLastConversation,
        ),
        (
            "Toggle Message Density (Cozy / Compact)",
            "Ctrl+D",
            Action::ToggleDensity,
        ),
        ("Cycle Color Theme", "", Action::CycleTheme),
        (
            "Change Presence",
            "Ctrl+P",
            Action::OpenOverlay(OverlayKind::Presence),
        ),
        ("Preview Image Attachment", "v", Action::OpenImagePreview),
        ("Send File", "Ctrl+O", Action::OpenSendFile),
        (
            "View Attachments",
            "Ctrl+T",
            Action::OpenOverlay(OverlayKind::Attachments),
        ),
        (
            "Room Info & Members",
            "Ctrl+U",
            Action::OpenOverlay(OverlayKind::RoomInfo),
        ),
        (
            "Add Reaction to Selected",
            "+",
            Action::OpenReactionsForSelected,
        ),
        (
            "Open Thread for Selected",
            "t",
            Action::OpenThreadForSelected,
        ),
        (
            "Toggle Composer Target (Thread / Channel)",
            "Alt+T",
            Action::ToggleComposerTarget,
        ),
        ("Toggle Channel Subject", "Ctrl+S", Action::ToggleSubject),
        (
            "Clear Pending Images",
            "Ctrl+Shift+X",
            Action::ClearPendingImages,
        ),
        ("Refresh Conversations", "F5 / R", Action::RefreshAll),
        ("Sync Deep History (150)", "H / s", Action::SyncHistory),
        ("Load Older Messages", "o", Action::LoadOlder),
        ("Download Latest Attachment", "d", Action::DownloadLatest),
        ("Focus Composer", "i", Action::OpenComposer),
        ("Mark Conversation as Read", "", Action::MarkActiveRead),
        ("Toggle Left Navigation Rail", "Ctrl+B", Action::ToggleNavRail),
        ("Close Thread Drawer", "Esc", Action::CloseThreadDrawer),
        ("Toggle Notifications", "", Action::ToggleNotify),
        (
            "Toggle Inline Image Previews",
            "",
            Action::ToggleInlineImages,
        ),
        ("Check for Updates", "", Action::CheckForUpdates),
        ("Install Self-Update", "", Action::ApplySelfUpdate),
        (
            "Settings",
            "Ctrl+,",
            Action::OpenOverlay(OverlayKind::Settings),
        ),
        (
            "Keyboard Shortcuts Help",
            "?",
            Action::OpenOverlay(OverlayKind::Help),
        ),
        ("Quit Teams", "Ctrl+Q", Action::QuitConfirm),
    ];

    cmds.into_iter()
        .map(|(label, shortcut, action)| PaletteItem {
            kind: ItemKind::Command,
            label: label.to_string(),
            search_alias: String::new(),
            shortcut: shortcut.to_string(),
            action,
            base_priority: 0,
        })
        .collect()
}

fn conversation_items(app: &AppState, ui: &UiState) -> Vec<PaletteItem> {
    let mut team_channel_names: HashMap<String, (String, String)> = HashMap::new();
    for team in &app.teams {
        for ch in &team.channels {
            team_channel_names.insert(
                ch.conv_key(),
                (team.display_name.clone(), ch.display_name.clone()),
            );
        }
    }

    let mut seen = HashSet::new();
    let mut items = Vec::new();

    for (idx, conv) in app.conversations.iter().enumerate() {
        seen.insert(conv.conv_key.clone());
        let is_pinned = conv.pinned || ui.pinned_convs.contains(&conv.conv_key);
        let unread_bonus = if conv.unread_count > 0 { 250 } else { 0 };
        let pinned_bonus = if is_pinned { 150 } else { 0 };
        let recent_bonus = 100i32.saturating_sub(idx.min(100) as i32);
        let base_priority = unread_bonus + pinned_bonus + recent_bonus;

        let is_channel = conv.kind == ConversationKind::Channel;
        let (label, search_alias) = if is_channel {
            if let Some((team_name, ch_name)) = team_channel_names.get(&conv.conv_key) {
                (
                    format!("{team_name} › #{ch_name}"),
                    format!("{team_name}/{ch_name}"),
                )
            } else if let Some(team_name) = conv.team_name.as_deref() {
                (
                    format!("{team_name} › #{}", conv.title),
                    format!("{team_name}/{}", conv.title),
                )
            } else {
                (format!("#{}", conv.title), conv.title.clone())
            }
        } else {
            (conv.title.clone(), conv.title.clone())
        };

        let mut tags = Vec::new();
        if conv.unread_count > 0 {
            tags.push(format!("{} unread", conv.unread_count));
        }
        if is_pinned {
            tags.push("Pinned".to_string());
        }
        if tags.is_empty() {
            tags.push(AppState::conversation_kind_label(conv.kind).to_string());
        }

        items.push(PaletteItem {
            kind: if is_channel {
                ItemKind::Channel
            } else {
                ItemKind::Chat
            },
            label,
            search_alias,
            shortcut: tags.join(" · "),
            action: Action::OpenConversationKey {
                conv_key: conv.conv_key.clone(),
                focus_composer: true,
            },
            base_priority,
        });
    }

    for team in &app.teams {
        for ch in &team.channels {
            let conv_key = ch.conv_key();
            if !seen.insert(conv_key.clone()) {
                continue;
            }
            let is_pinned = ui.pinned_convs.contains(&conv_key);
            let pinned_bonus = if is_pinned { 150 } else { 0 };
            let mut tags = Vec::new();
            if is_pinned {
                tags.push("Pinned".to_string());
            } else {
                tags.push("Channel".to_string());
            }
            items.push(PaletteItem {
                kind: ItemKind::Channel,
                label: format!("{} › #{}", team.display_name, ch.display_name),
                search_alias: format!("{}/{}", team.display_name, ch.display_name),
                shortcut: tags.join(" · "),
                action: Action::OpenConversationKey {
                    conv_key,
                    focus_composer: true,
                },
                base_priority: 10 + pinned_bonus,
            });
        }
    }

    items
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_app(_app: &AppState, _ui: &UiState) -> Self {
        Self::default()
    }

    pub fn filtered(&self, app: &AppState, ui: &UiState) -> Vec<PaletteItem> {
        let raw = self.query.trim();
        let (mode, needle) = if let Some(rest) = raw.strip_prefix('>') {
            ('>', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('#') {
            ('#', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('@') {
            ('@', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('/') {
            let search_q = rest.trim().to_string();
            let label = if search_q.is_empty() {
                "Search all messages…".to_string()
            } else {
                format!("Search all messages for \"{search_q}\"")
            };
            return vec![PaletteItem {
                kind: ItemKind::Search,
                label,
                search_alias: String::new(),
                shortcut: "Enter".to_string(),
                action: Action::OpenSearchWithQuery(search_q),
                base_priority: 500,
            }];
        } else {
            (' ', raw.to_lowercase())
        };

        let mut pool = Vec::new();
        match mode {
            '>' => pool.extend(palette_commands()),
            '#' => {
                pool.extend(
                    conversation_items(app, ui)
                        .into_iter()
                        .filter(|i| i.kind == ItemKind::Channel),
                );
            }
            '@' => {
                pool.extend(
                    conversation_items(app, ui)
                        .into_iter()
                        .filter(|i| i.kind == ItemKind::Chat),
                );
                pool.push(PaletteItem {
                    kind: ItemKind::Command,
                    label: "Create new chat…".to_string(),
                    search_alias: "new chat".to_string(),
                    shortcut: "Ctrl+N".to_string(),
                    action: Action::OpenOverlay(OverlayKind::NewChat),
                    base_priority: 5,
                });
            }
            _ => {
                pool.extend(conversation_items(app, ui));
                pool.extend(palette_commands());
            }
        }

        let mut scored: Vec<(i32, PaletteItem)> = pool
            .into_iter()
            .filter_map(|item| {
                if needle.is_empty() {
                    return Some((item.base_priority, item));
                }
                let label_l = item.label.to_lowercase();
                let alias_l = item.search_alias.to_lowercase();
                let short_l = item.shortcut.to_lowercase();
                let match_score = if label_l.starts_with(&needle) || alias_l.starts_with(&needle) {
                    400
                } else if label_l.contains(&needle)
                    || alias_l.contains(&needle)
                    || short_l.contains(&needle)
                {
                    200
                } else if fuzzy_matches(&label_l, &needle)
                    || (!alias_l.is_empty() && fuzzy_matches(&alias_l, &needle))
                    || fuzzy_matches(&short_l, &needle)
                {
                    80
                } else {
                    return None;
                };
                Some((match_score + item.base_priority, item))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.query.push_str(text);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::MoveUp => {
                let len = self.filtered(app, ui).len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered(app, ui).len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let items = self.filtered(app, ui);
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
                    None => OverlayOutcome::Handled,
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(72),
            (area.height.saturating_sub(4)).min(20),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Omni-Switcher · > cmd  # chan  @ chat  / search · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled("> ", theme.title(true)),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let items = self.filtered(app, ui);
        let active_sel = if items.is_empty() {
            0
        } else {
            self.selected.min(items.len() - 1)
        };
        let list_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " No matching conversations or commands",
                    theme.muted_style(),
                )),
                Rect::new(inner.x, list_y, inner.width, 1),
            );
            return;
        }

        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
        for (row, (index, item)) in items
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
            let y = list_y + row as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let is_selected = index == active_sel;
            let style = if is_selected {
                theme.selection()
            } else {
                Style::default()
            };
            let right_w = unicode_width::UnicodeWidthStr::width(item.shortcut.as_str())
                .min(18)
                .max(10);
            let label_w = (inner.width as usize).saturating_sub(right_w + 3).max(10);
            let truncated_label = crate::ui::truncate_width(&item.label, label_w);
            let used_w = unicode_width::UnicodeWidthStr::width(truncated_label.as_str());
            let pad = label_w.saturating_sub(used_w);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {truncated_label}{}", " ".repeat(pad)),
                    style,
                ),
                Span::styled(
                    crate::ui::truncate_width(&item.shortcut, right_w),
                    theme.muted_style(),
                ),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }
}

fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
    let mut chars = needle.chars();
    let Some(mut target) = chars.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == target {
            match chars.next() {
                Some(next) => target = next,
                None => return true,
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        graph::GraphClient,
        models::{ChannelSummary, ConversationSummary, ConversationTarget, TeamSummary},
        storage::Storage,
        sync::{self, SyncCommand},
    };
    use tokio::sync::mpsc;

    fn make_palette_app() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        app.conversations = vec![
            ConversationSummary {
                conv_key: "chat:c1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c1".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Alice Cooper".to_string(),
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
                team_name: Some("Engineering".to_string()),
                title: "General".to_string(),
                topic: None,
                unread_count: 4,
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
        app.teams = vec![TeamSummary {
            id: "t1".to_string(),
            display_name: "Engineering".to_string(),
            description: None,
            channels: vec![ChannelSummary {
                id: "ch1".to_string(),
                team_id: "t1".to_string(),
                team_name: "Engineering".to_string(),
                display_name: "General".to_string(),
                description: None,
                membership_type: "standard".to_string(),
                web_url: None,
            }],
        }];
        ui.pinned_convs.insert("chat:c1".to_string());
        (app, ui)
    }

    #[tokio::test]
    async fn filters_and_fuzzy_matches() {
        let (app, ui) = make_palette_app();
        let mut palette = CommandPalette::new();

        palette.query = "search".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res.iter().any(|i| i.label.contains("Search Messages")));

        palette.query = "new chat".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res.iter().any(|i| i.label.contains("New Chat")));

        palette.query = "eng/gen".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res
            .iter()
            .any(|i| matches!(i.action, Action::OpenConversationKey { ref conv_key, .. } if conv_key == "chan:t1:ch1")));

        palette.query = ">dens".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res.iter().all(|i| i.kind == ItemKind::Command));
        assert!(res.iter().any(|i| i.action == Action::ToggleDensity));

        palette.query = "#gen".to_string();
        let res = palette.filtered(&app, &ui);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].kind, ItemKind::Channel);

        palette.query = "@".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res.iter().any(|i| i.label == "Alice Cooper"));
        assert!(res.iter().any(|i| i.label == "Create new chat…"));

        palette.query = "/deploy".to_string();
        let outcome = palette.handle(&Action::Activate, &app, &ui);
        assert_eq!(
            outcome,
            OverlayOutcome::CloseAndDispatch(Action::OpenSearchWithQuery("deploy".to_string()))
        );

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn activate_dispatches_quit() {
        let (app, ui) = make_palette_app();
        let mut palette = CommandPalette::new();
        palette.query = "quit".to_string();
        let outcome = palette.handle(&Action::Activate, &app, &ui);
        assert!(matches!(
            outcome,
            OverlayOutcome::CloseAndDispatch(Action::QuitConfirm)
        ));
        app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}
