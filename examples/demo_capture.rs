//! Captures real `ratatui::TestBackend` frames (120x34) from `teams_cli::ui::draw`
//! using 100% fictional Acme Engineering demo data, then invokes the shared GIF renderer.
//!
//! Run with:
//!   cargo run --example demo_capture
//!
//! The renderer is not part of this repository. Point `DEMO_RENDERER_SCRIPT` at
//! `render_tui_gif.py` (default: `scripts/render_tui_gif.py`) and, optionally,
//! `DEMO_PREVIEW_DIR` at a directory for preview PNGs (default: `target/demo-previews/teams`).

use std::path::PathBuf;
use std::process::Command;

use chrono::Local;
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use teams_cli::{
    auth::AuthManager,
    config::{AppConfig, DensityMode, ThemePreset},
    graph::GraphClient,
    models::{
        AttachmentInfo, ChannelSummary, ConversationKind, ConversationSummary, ConversationTarget,
        MemberInfo, MentionInfo, MessageItem, PresenceStatus, ReactionSummary, TeamSummary,
        ThreadView, UserProfile,
    },
    storage::Storage,
    sync::{self, SyncCommand},
    ui::{
        self,
        app::{AppState, Focus, UiState},
        overlays::{
            command_palette::CommandPalette,
            file_picker::{FilePickerEntry, FilePickerOverlay, PickerMode},
            links::{extract_links_from_messages, LinksOverlay},
            settings::SettingsState,
            Overlay,
        },
        theme::Theme,
    },
};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;

/// A non-empty environment variable as a path.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The GIF renderer script: `DEMO_RENDERER_SCRIPT`, else `scripts/render_tui_gif.py`
/// relative to the working directory (`cargo run --example` runs from the repo root).
fn renderer_script() -> PathBuf {
    env_path("DEMO_RENDERER_SCRIPT")
        .unwrap_or_else(|| PathBuf::from("scripts").join("render_tui_gif.py"))
}

/// Where preview PNGs are written: `DEMO_PREVIEW_DIR`, else `target/demo-previews/teams`.
fn preview_dir() -> PathBuf {
    env_path("DEMO_PREVIEW_DIR")
        .unwrap_or_else(|| PathBuf::from("target").join("demo-previews").join("teams"))
}

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = indexed_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn indexed_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (24, 26, 33),
        1 => (247, 118, 142),
        2 => (158, 206, 106),
        3 => (224, 175, 104),
        4 => (122, 162, 247),
        5 => (187, 154, 247),
        6 => (125, 207, 255),
        7 => (169, 177, 214),
        8 => (86, 95, 137),
        9 => (255, 140, 160),
        10 => (175, 224, 128),
        11 => (240, 200, 130),
        12 => (148, 184, 255),
        13 => (205, 176, 255),
        14 => (155, 222, 255),
        15 => (240, 244, 252),
        16..=231 => {
            let n = idx - 16;
            let b = n % 6;
            let g = (n / 6) % 6;
            let r = n / 36;
            let map = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (map(r), map(g), map(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

fn modifier_to_flags(m: Modifier) -> u8 {
    let mut f = 0u8;
    if m.contains(Modifier::BOLD) {
        f |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        f |= 2;
    }
    if m.contains(Modifier::DIM) {
        f |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        f |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        f |= 16;
    }
    f
}

fn normalize_cell_char(symbol: &str) -> char {
    let ch = symbol
        .chars()
        .find(|c| !matches!(*c, '\u{FE0E}' | '\u{FE0F}' | '\u{200D}'))
        .unwrap_or(' ');
    match ch {
        '▎' | '▏' => '▌',
        '↳' | '↗' => '›',
        '⊘' | '⛔' | '◐' => '●',
        '◆' | '◈' | '⚑' => '♦',
        '✕' => '×',
        '🔒' | '📌' | '✎' | '☰' => '▪',
        '▣' | '☑' => '■',
        '👍' | '📎' => '+',
        '❤' => '♥',
        '★' | '🔥' => '*',
        '❗' => '!',
        '🚀' | '📥' => '▸',
        '🎉' => '♦',
        '⌫' => '<',
        _ => ch,
    }
}

fn capture_buffer(buf: &Buffer, duration_ms: u32, caption: &str, key_badge: &str) -> Value {
    let area = buf.area;
    let mut rows = Vec::with_capacity(area.height as usize);
    for y in area.y..area.bottom() {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_fg: Option<Color> = None;
        let mut cur_bg: Option<Color> = None;
        let mut cur_flags: u8 = 0;

        for x in area.x..area.right() {
            let cell = &buf[(x, y)];
            let ch = if cell.skip {
                ' '
            } else {
                normalize_cell_char(cell.symbol())
            };
            let fg = cell.fg;
            let bg = cell.bg;
            let mut flags = modifier_to_flags(cell.modifier);
            if ch == '✓' {
                flags &= !1;
            }

            if cur_fg == Some(fg) && cur_bg == Some(bg) && cur_flags == flags {
                cur_text.push(ch);
            } else {
                if !cur_text.is_empty() {
                    runs.push(json!([
                        cur_text,
                        color_to_json(cur_fg.unwrap_or(Color::Reset)),
                        color_to_json(cur_bg.unwrap_or(Color::Reset)),
                        cur_flags
                    ]));
                    cur_text.clear();
                }
                cur_text.push(ch);
                cur_fg = Some(fg);
                cur_bg = Some(bg);
                cur_flags = flags;
            }
        }
        if !cur_text.is_empty() {
            runs.push(json!([
                cur_text,
                color_to_json(cur_fg.unwrap_or(Color::Reset)),
                color_to_json(cur_bg.unwrap_or(Color::Reset)),
                cur_flags
            ]));
        }
        rows.push(Value::Array(runs));
    }

    json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    })
}

fn render_and_capture(
    app: &mut AppState,
    ui: &mut UiState,
    duration_ms: u32,
    caption: &str,
    key_badge: &str,
) -> Value {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal init");
    terminal
        .draw(|f| {
            let _ = ui::draw(f, app, ui);
        })
        .expect("ratatui draw failed");
    capture_buffer(terminal.backend().buffer(), duration_ms, caption, key_badge)
}

fn make_presence(user_id: &str, availability: &str, activity: &str) -> PresenceStatus {
    PresenceStatus {
        user_id: user_id.to_string(),
        availability: availability.to_string(),
        activity: activity.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_msg(
    conv_key: &str,
    msg_id: &str,
    parent_msg_id: Option<&str>,
    subject: Option<&str>,
    author_id: &str,
    author_name: &str,
    is_me: bool,
    created_at: &str,
    body_html: &str,
    body_plain: &str,
    attachments: Vec<AttachmentInfo>,
    mentions: Vec<MentionInfo>,
    reactions: Vec<ReactionSummary>,
    reply_count: usize,
) -> MessageItem {
    MessageItem {
        msg_key: MessageItem::make_msg_key(conv_key, msg_id),
        conv_key: conv_key.to_string(),
        msg_id: msg_id.to_string(),
        parent_msg_id: parent_msg_id.map(ToString::to_string),
        reply_to_id: None,
        reply_to_author: None,
        reply_to_preview: None,
        subject: subject.map(ToString::to_string),
        author_id: Some(author_id.to_string()),
        author_name: author_name.to_string(),
        is_me,
        created_at: created_at.to_string(),
        last_modified_at: None,
        deleted: false,
        importance: "normal".to_string(),
        message_type: "message".to_string(),
        body_html: body_html.to_string(),
        body_plain: body_plain.to_string(),
        attachments,
        mentions,
        reactions,
        reply_count,
        web_url: None,
    }
}

fn build_channel_messages(conv_key: &str, today: &str) -> Vec<MessageItem> {
    vec![
        make_msg(
            conv_key,
            "m-101",
            None,
            Some("v2.4.0 Canary Rollout - US-East & EU-West"),
            "u-elena",
            "Elena Park",
            false,
            &format!("{today}T09:12:00Z"),
            "<p>Canary deployed to 25% of traffic. P99 gateway latency dropped from <b>48ms</b> to <b>19ms</b> after enabling connection pooling.</p>\
             <p>Track live metrics on <a href=\"https://metrics.acme.dev/d/gw-v24\">Grafana Latency Board</a> and review <a href=\"https://github.com/acme/platform/pull/482\">PR #482</a>.</p>",
            "Canary deployed to 25% of traffic. P99 gateway latency dropped from 48ms to 19ms after enabling connection pooling.\nTrack live metrics on Grafana Latency Board and review PR #482.",
            vec![],
            vec![],
            vec![
                ReactionSummary {
                    reaction_type: "like".to_string(),
                    count: 4,
                    me_reacted: true,
                    users: vec!["Alex Rivera".into(), "Marcus Chen".into()],
                },
                ReactionSummary {
                    reaction_type: "heart".to_string(),
                    count: 2,
                    me_reacted: false,
                    users: vec!["Priya Patel".into()],
                },
            ],
            3,
        ),
        make_msg(
            conv_key,
            "m-102",
            None,
            None,
            "u-marcus",
            "Marcus Chen",
            false,
            &format!("{today}T09:18:00Z"),
            "<p>Zero error-budget burn across both regions. <at id=\"0\">Alex Rivera</at> can you double-check the <a href=\"https://docs.acme.dev/runbooks/v24-rollout\">v2.4 Rollout Runbook</a> before we bump to 100%?</p>",
            "Zero error-budget burn across both regions. @Alex Rivera can you double-check the v2.4 Rollout Runbook before we bump to 100%?",
            vec![],
            vec![MentionInfo {
                id: 0,
                user_id: "u-alex".to_string(),
                display_name: "Alex Rivera".to_string(),
            }],
            vec![ReactionSummary {
                reaction_type: "like".to_string(),
                count: 2,
                me_reacted: false,
                users: vec!["Elena Park".into()],
            }],
            0,
        ),
        make_msg(
            conv_key,
            "m-103",
            None,
            None,
            "u-alex",
            "Alex Rivera",
            true,
            &format!("{today}T09:24:00Z"),
            "<p>Verified all smoke tests and rollback triggers. Ready to promote to 100% at 10:00 UTC.</p>",
            "Verified all smoke tests and rollback triggers. Ready to promote to 100% at 10:00 UTC.",
            vec![],
            vec![],
            vec![ReactionSummary {
                reaction_type: "heart".to_string(),
                count: 3,
                me_reacted: true,
                users: vec!["Elena Park".into(), "Priya Patel".into()],
            }],
            0,
        ),
        make_msg(
            conv_key,
            "m-104",
            None,
            None,
            "u-priya",
            "Priya Patel",
            false,
            &format!("{today}T09:31:00Z"),
            "<p>Attached the final Q4 load-test summary and updated <a href=\"https://status.acme.dev/releases/v2.4\">Release Dashboard</a> for stakeholders.</p>",
            "Attached the final Q4 load-test summary and updated Release Dashboard for stakeholders.",
            vec![AttachmentInfo {
                id: "att-bench-1".to_string(),
                content_type: "reference".to_string(),
                content_url: Some("https://acme.sharepoint.com/docs/v24-load-test.pdf".into()),
                name: Some("v24-load-test.pdf".to_string()),
                content_json: None,
                thumbnail_url: None,
            }],
            vec![],
            vec![],
            0,
        ),
    ]
}

fn build_elena_dm_messages(conv_key: &str, today: &str) -> Vec<MessageItem> {
    vec![
        make_msg(
            conv_key,
            "dm-201",
            None,
            None,
            "u-elena",
            "Elena Park",
            false,
            &format!("{today}T08:45:00Z"),
            "<p>Morning Alex! Quick heads-up before standup: the SQLite FTS5 cache cut cold-start query time down to <b>3.2ms</b>.</p>",
            "Morning Alex! Quick heads-up before standup: the SQLite FTS5 cache cut cold-start query time down to 3.2ms.",
            vec![],
            vec![],
            vec![ReactionSummary {
                reaction_type: "like".to_string(),
                count: 1,
                me_reacted: true,
                users: vec!["Alex Rivera".into()],
            }],
            0,
        ),
        make_msg(
            conv_key,
            "dm-202",
            None,
            None,
            "u-alex",
            "Alex Rivera",
            true,
            &format!("{today}T08:49:00Z"),
            "<p>Nice! Did we keep WAL mode enabled for concurrent daemon reads?</p>",
            "Nice! Did we keep WAL mode enabled for concurrent daemon reads?",
            vec![],
            vec![],
            vec![],
            0,
        ),
        make_msg(
            conv_key,
            "dm-203",
            None,
            None,
            "u-elena",
            "Elena Park",
            false,
            &format!("{today}T08:52:00Z"),
            "<p>Yes, WAL + busy_timeout=5000. Here is the benchmark chart from the staging cluster:</p>",
            "Yes, WAL + busy_timeout=5000. Here is the benchmark chart from the staging cluster:",
            vec![AttachmentInfo {
                id: "att-img-1".to_string(),
                content_type: "image/png".to_string(),
                content_url: Some("https://acme.sharepoint.com/charts/latency-p99.png".into()),
                name: Some("latency-p99.png".to_string()),
                content_json: None,
                thumbnail_url: None,
            }],
            vec![],
            vec![ReactionSummary {
                reaction_type: "heart".to_string(),
                count: 1,
                me_reacted: true,
                users: vec!["Alex Rivera".into()],
            }],
            0,
        ),
    ]
}

fn build_thread(conv_key: &str, root: MessageItem, today: &str) -> ThreadView {
    let replies = vec![
        make_msg(
            conv_key,
            "r-301",
            Some(&root.msg_id),
            None,
            "u-marcus",
            "Marcus Chen",
            false,
            &format!("{today}T09:14:00Z"),
            "<p>CPU utilization on edge nodes also dropped 18% after the pool tweak.</p>",
            "CPU utilization on edge nodes also dropped 18% after the pool tweak.",
            vec![],
            vec![],
            vec![ReactionSummary {
                reaction_type: "like".to_string(),
                count: 2,
                me_reacted: true,
                users: vec!["Alex Rivera".into()],
            }],
            0,
        ),
        make_msg(
            conv_key,
            "r-302",
            Some(&root.msg_id),
            None,
            "u-priya",
            "Priya Patel",
            false,
            &format!("{today}T09:16:00Z"),
            "<p>Confirmed EU-West TLS handshake latency is flat. Green light from SRE.</p>",
            "Confirmed EU-West TLS handshake latency is flat. Green light from SRE.",
            vec![],
            vec![],
            vec![],
            0,
        ),
        make_msg(
            conv_key,
            "r-303",
            Some(&root.msg_id),
            None,
            "u-alex",
            "Alex Rivera",
            true,
            &format!("{today}T09:19:00Z"),
            "<p>Awesome, promoting canary to 100% after the 10:00 UTC checkpoint.</p>",
            "Awesome, promoting canary to 100% after the 10:00 UTC checkpoint.",
            vec![],
            vec![],
            vec![],
            0,
        ),
    ];
    ThreadView {
        conv_key: conv_key.to_string(),
        root,
        replies,
    }
}

#[tokio::main]
async fn main() {
    // Isolate config dir so real local drafts/config paths never leak into the demo GIF.
    std::env::set_var("TEAMS_CONFIG_DIR", "~/.config/teams-cli");

    let mut cfg = AppConfig::default();
    cfg.theme = ThemePreset::Dark;
    cfg.density = DensityMode::Cozy;
    cfg.show_nav_rail = true;
    cfg.auto_thread_pane = true;
    cfg.inline_images = true;

    let auth = AuthManager::new(cfg.clone()).expect("auth init");
    let graph = GraphClient::new(cfg.clone(), auth.clone()).expect("graph init");
    let storage = Storage::open_in_memory().expect("in-memory sqlite");
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    app.download_dir = PathBuf::from("~/Downloads/teams");
    app.sync_summary = "Synced".to_string();
    app.daemon_attached = true;
    app.daemon_pid = Some(4821);
    app.my_user_id = "u-alex".to_string();
    app.my_profile = Some(UserProfile {
        id: "u-alex".to_string(),
        display_name: "Alex Rivera".to_string(),
        mail: Some("alex.rivera@acme.dev".to_string()),
        user_principal_name: Some("alex.rivera@acme.dev".to_string()),
    });
    app.my_presence = Some(make_presence("u-alex", "Available", "Available"));

    app.presences.insert("u-elena".into(), make_presence("u-elena", "Available", "Available"));
    app.presences.insert("u-marcus".into(), make_presence("u-marcus", "Busy", "InAMeeting"));
    app.presences.insert("u-priya".into(), make_presence("u-priya", "Available", "Available"));
    app.presences.insert("u-david".into(), make_presence("u-david", "Away", "Away"));
    app.presences.insert("u-sofia".into(), make_presence("u-sofia", "DoNotDisturb", "Presenting"));

    let chan_target = ConversationTarget::Channel {
        team_id: "team-eng".to_string(),
        channel_id: "ch-release".to_string(),
    };
    let chan_key = chan_target.conv_key();
    let dm_elena_target = ConversationTarget::Chat {
        chat_id: "chat-elena".to_string(),
    };
    let dm_elena_key = dm_elena_target.conv_key();

    app.teams = vec![
        TeamSummary {
            id: "team-eng".to_string(),
            display_name: "Acme Engineering".to_string(),
            description: Some("Core platform & infrastructure".to_string()),
            channels: vec![
                ChannelSummary {
                    id: "ch-release".to_string(),
                    team_id: "team-eng".to_string(),
                    team_name: "Acme Engineering".to_string(),
                    display_name: "platform-release".to_string(),
                    description: Some("Release coordination & canary telemetry".to_string()),
                    membership_type: "standard".to_string(),
                    web_url: None,
                },
                ChannelSummary {
                    id: "ch-general".to_string(),
                    team_id: "team-eng".to_string(),
                    team_name: "Acme Engineering".to_string(),
                    display_name: "general".to_string(),
                    description: Some("Engineering announcements".to_string()),
                    membership_type: "standard".to_string(),
                    web_url: None,
                },
                ChannelSummary {
                    id: "ch-incidents".to_string(),
                    team_id: "team-eng".to_string(),
                    team_name: "Acme Engineering".to_string(),
                    display_name: "incidents".to_string(),
                    description: Some("On-call triage".to_string()),
                    membership_type: "private".to_string(),
                    web_url: None,
                },
            ],
        },
        TeamSummary {
            id: "team-des".to_string(),
            display_name: "Product Design".to_string(),
            description: None,
            channels: vec![ChannelSummary {
                id: "ch-ds".to_string(),
                team_id: "team-des".to_string(),
                team_name: "Product Design".to_string(),
                display_name: "design-systems".to_string(),
                description: None,
                membership_type: "standard".to_string(),
                web_url: None,
            }],
        },
    ];

    app.conversations = vec![
        ConversationSummary {
            conv_key: "chat:chat-release-squad".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "chat-release-squad".to_string(),
            },
            kind: ConversationKind::Group,
            team_id: None,
            team_name: None,
            title: "Release Squad v2.4".to_string(),
            topic: Some("Release Squad v2.4".to_string()),
            unread_count: 3,
            last_read_at: Some("2026-11-19T09:20:00Z".to_string()),
            last_msg_id: Some("m-104".to_string()),
            last_msg_author: Some("Priya Patel".to_string()),
            last_msg_preview: Some("Attached the final Q4 load-test summary".to_string()),
            last_updated_at: Some("2026-11-19T09:31:00Z".to_string()),
            pinned: true,
            peer_user_id: None,
            web_url: None,
        },
        ConversationSummary {
            conv_key: dm_elena_key.clone(),
            target: dm_elena_target.clone(),
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Elena Park".to_string(),
            topic: None,
            unread_count: 1,
            last_read_at: Some("2026-11-19T08:40:00Z".to_string()),
            last_msg_id: Some("dm-203".to_string()),
            last_msg_author: Some("Elena Park".to_string()),
            last_msg_preview: Some("Here is the benchmark chart".to_string()),
            last_updated_at: Some("2026-11-19T08:52:00Z".to_string()),
            pinned: true,
            peer_user_id: Some("u-elena".to_string()),
            web_url: None,
        },
        ConversationSummary {
            conv_key: "chat:chat-marcus".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "chat-marcus".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Marcus Chen".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: Some("Marcus Chen".to_string()),
            last_msg_preview: Some("Zero error-budget burn".to_string()),
            last_updated_at: Some("2026-11-19T08:30:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u-marcus".to_string()),
            web_url: None,
        },
        ConversationSummary {
            conv_key: "chat:chat-priya".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "chat-priya".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Priya Patel".to_string(),
            topic: None,
            unread_count: 2,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: Some("Priya Patel".to_string()),
            last_msg_preview: Some("EU-West telemetry looks green".to_string()),
            last_updated_at: Some("2026-11-19T08:15:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u-priya".to_string()),
            web_url: None,
        },
        ConversationSummary {
            conv_key: "chat:chat-standup".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "chat-standup".to_string(),
            },
            kind: ConversationKind::Meeting,
            team_id: None,
            team_name: None,
            title: "Platform Daily Standup".to_string(),
            topic: Some("Platform Daily Standup".to_string()),
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: Some("David Kim".to_string()),
            last_msg_preview: Some("Notes posted to #general".to_string()),
            last_updated_at: Some("2026-11-19T07:50:00Z".to_string()),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        },
        ConversationSummary {
            conv_key: "chat:chat-david".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "chat-david".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "David Kim".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: Some("David Kim".to_string()),
            last_msg_preview: Some("Sounds good, thanks!".to_string()),
            last_updated_at: Some("2026-11-18T18:10:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u-david".to_string()),
            web_url: None,
        },
        ConversationSummary {
            conv_key: "chat:chat-sofia".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "chat-sofia".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Sofia Morales".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: Some("Sofia Morales".to_string()),
            last_msg_preview: Some("Design tokens updated".to_string()),
            last_updated_at: Some("2026-11-18T16:40:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u-sofia".to_string()),
            web_url: None,
        },
        ConversationSummary {
            conv_key: chan_key.clone(),
            target: chan_target.clone(),
            kind: ConversationKind::Channel,
            team_id: Some("team-eng".to_string()),
            team_name: Some("Acme Engineering".to_string()),
            title: "#platform-release".to_string(),
            topic: None,
            unread_count: 1,
            last_read_at: None,
            last_msg_id: Some("m-104".to_string()),
            last_msg_author: Some("Priya Patel".to_string()),
            last_msg_preview: Some("Attached the final Q4 load-test summary".to_string()),
            last_updated_at: None,
            pinned: false,
            peer_user_id: None,
            web_url: None,
        },
    ];

    app.members = vec![
        MemberInfo {
            user_id: "u-elena".into(),
            member_id: None,
            display_name: "Elena Park".into(),
            email: Some("elena.park@acme.dev".into()),
            roles: vec!["owner".into()],
            presence: Some(make_presence("u-elena", "Available", "Available")),
        },
        MemberInfo {
            user_id: "u-marcus".into(),
            member_id: None,
            display_name: "Marcus Chen".into(),
            email: Some("marcus.chen@acme.dev".into()),
            roles: vec!["member".into()],
            presence: Some(make_presence("u-marcus", "Busy", "InAMeeting")),
        },
        MemberInfo {
            user_id: "u-priya".into(),
            member_id: None,
            display_name: "Priya Patel".into(),
            email: Some("priya.patel@acme.dev".into()),
            roles: vec!["member".into()],
            presence: Some(make_presence("u-priya", "Available", "Available")),
        },
    ];

    let today_str = Local::now().date_naive().format("%Y-%m-%d").to_string();
    let channel_msgs = build_channel_messages(&chan_key, &today_str);
    let elena_msgs = build_elena_dm_messages(&dm_elena_key, &today_str);
    let thread = build_thread(&chan_key, channel_msgs[0].clone(), &today_str);

    let mut ui = UiState::from_config(&cfg);
    ui.theme = Theme::midnight_dark();
    ui.theme_preset = ThemePreset::Dark;
    ui.density = DensityMode::Cozy;
    ui.show_nav_rail = true;
    ui.active_nav_tab = 0;
    ui.nav_selected = 1; // first selectable chat under Pinned

    let mut frames = Vec::new();

    // Frame 1: Chat list + active engineering channel conversation
    app.active_conv_key = Some(chan_key.clone());
    app.active_target = Some(chan_target.clone());
    app.messages = channel_msgs.clone();
    ui.focus = Focus::Chat;
    ui.chat_scroll.selected_idx = Some(0);
    ui.unread_anchor = Some((chan_key.clone(), format!("{today_str}T09:26:00Z")));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "Minimal-Chrome TUI · Chats, unread badges, presence & channel stream",
        "teams",
    ));

    // Frame 2: Navigate down to another chat (Elena Park 1:1 DM)
    ui.focus = Focus::Nav;
    ui.nav_selected = 2; // Elena Park
    app.active_conv_key = Some(dm_elena_key.clone());
    app.active_target = Some(dm_elena_target.clone());
    app.messages = elena_msgs.clone();
    ui.unread_anchor = None;
    ui.chat_scroll.selected_idx = Some(2);
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        1800,
        "Navigate conversations smoothly with j / k · inline attachment chips",
        "j / k",
    ));

    // Frame 3: Open Thread panel on engineering channel post
    ui.active_nav_tab = 1; // Teams tab
    ui.nav_selected = 1; // #platform-release
    app.active_conv_key = Some(chan_key.clone());
    app.active_target = Some(chan_target.clone());
    app.messages = channel_msgs.clone();
    app.active_thread = Some(thread.clone());
    ui.thread_drawer_open = true;
    ui.focus = Focus::Thread;
    ui.chat_scroll.selected_idx = Some(0);
    ui.thread_scroll.selected_idx = Some(2);
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "Side-by-side Thread Drawer · read & reply in channel threads",
        "t",
    ));

    // Frame 4: Link Extractor overlay (`u`)
    let msg_refs: Vec<&MessageItem> = app.messages.iter().collect();
    let extracted = extract_links_from_messages(&msg_refs);
    let mut links_overlay = LinksOverlay::new(extracted, "Acme Engineering › #platform-release");
    links_overlay.selected = 0;
    ui.push_overlay(Overlay::links(links_overlay));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Link Extractor · jump to or copy any URL in the conversation",
        "u",
    ));
    ui.close_top_overlay();

    // Frame 5: Composer with staged inline image pill + draft text (`i`)
    ui.thread_drawer_open = false;
    app.active_thread = None;
    ui.active_nav_tab = 0;
    ui.nav_selected = 1;
    ui.focus = Focus::Composer;
    ui.composer.load_text("Promoting v2.4.0 to 100% now — attaching the final P99 latency snapshot.");
    ui.composer.stage_inline_image(
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        "image/png",
        Some("latency-p99.png".to_string()),
    );
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Composer-First workflow · Markdown drafts & clipboard screenshot staging",
        "i / Ctrl+V",
    ));

    // Frame 6: File Picker overlay (`Ctrl+F`)
    let picker = FilePickerOverlay {
        current_dir: PathBuf::from("/workspace/acme-platform"),
        entries: vec![
            FilePickerEntry {
                name: "charts".into(),
                path: PathBuf::from("/workspace/acme-platform/charts"),
                is_dir: true,
                size: None,
            },
            FilePickerEntry {
                name: "docs".into(),
                path: PathBuf::from("/workspace/acme-platform/docs"),
                is_dir: true,
                size: None,
            },
            FilePickerEntry {
                name: "latency-p99.png".into(),
                path: PathBuf::from("/workspace/acme-platform/latency-p99.png"),
                is_dir: false,
                size: Some(148_200),
            },
            FilePickerEntry {
                name: "release-notes-v2.4.md".into(),
                path: PathBuf::from("/workspace/acme-platform/release-notes-v2.4.md"),
                is_dir: false,
                size: Some(6_420),
            },
            FilePickerEntry {
                name: "v24-load-test.pdf".into(),
                path: PathBuf::from("/workspace/acme-platform/v24-load-test.pdf"),
                is_dir: false,
                size: Some(1_240_000),
            },
            FilePickerEntry {
                name: "rollout-plan.toml".into(),
                path: PathBuf::from("/workspace/acme-platform/rollout-plan.toml"),
                is_dir: false,
                size: Some(2_890),
            },
        ],
        filter: String::new(),
        selected: 2,
        mode: PickerMode::Browse,
        zoxide_dirs: Vec::new(),
        zoxide_filter: String::new(),
        zoxide_selected: 0,
        status: None,
    };
    ui.push_overlay(Overlay::FilePicker(picker));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Built-in File Picker with zoxide quick-jump · attach & upload directly",
        "Ctrl+F",
    ));
    ui.close_top_overlay();

    // Frame 7: Command Palette / Omni-Switcher (`Ctrl+K`)
    let mut palette = CommandPalette::new();
    palette.query = ">".to_string();
    palette.selected = 4;
    ui.push_overlay(Overlay::CommandPalette(palette));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Unified Omni-Switcher & Command Palette · chats, #channels, >commands",
        "Ctrl+K",
    ));
    ui.close_top_overlay();

    // Frame 8: Settings overlay (`Ctrl+,`)
    ui.composer.clear();
    let settings = SettingsState { selected: 5 };
    ui.push_overlay(Overlay::Settings(settings));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "Settings · themes (Midnight Dark / Daylight Light), density & image previews",
        "Ctrl+,",
    ));
    ui.close_top_overlay();

    app.sync_ctrl.request(SyncCommand::Shutdown);

    let spec = json!({
        "title": "teams — Acme Engineering",
        "badge": "@bitefoundry/teams v0.1.0",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [20, 22, 32],
        "fg": [226, 230, 243],
        "frames": frames,
    });

    let tmp_json = std::env::temp_dir().join("teams_demo_frames.json");
    std::fs::write(
        &tmp_json,
        serde_json::to_string(&spec).expect("serialize spec"),
    )
    .expect("write frames json");

    let out_gif = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("demo.gif");

    let renderer = renderer_script();
    assert!(
        renderer.is_file(),
        "renderer script not found at {}; set DEMO_RENDERER_SCRIPT",
        renderer.display()
    );
    let previews = preview_dir();
    std::fs::create_dir_all(&previews).expect("create preview dir");

    let status = Command::new("python")
        .arg(&renderer)
        .arg(&tmp_json)
        .arg(&out_gif)
        .arg("--preview-png")
        .arg(&previews)
        .status()
        .expect("failed to invoke python render_tui_gif.py");
    assert!(status.success(), "render_tui_gif.py exited with {status}");
}
