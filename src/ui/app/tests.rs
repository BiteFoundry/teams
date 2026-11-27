use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;

use crate::img_preview::{self, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::{AttachmentInfo, CoreEvent, NotificationCandidate, SyncCursor};
use crate::sync;

use super::super::composer::ReplyPreview;
use super::super::overlays::image_preview::PreviewTarget;
use super::super::overlays::Overlay;
use super::helpers::merge_optimistic_messages;
use super::*;
use crate::sync::SyncCommand;

#[test]
fn image_cache_key_formats_stably() {
    assert_eq!(
        AppState::image_cache_key("msg-1", "att-9", 40, 14, (10, 20), (30, 34, 50)),
        "msg-1:att-9:40x14:10x20:1e2232"
    );
}

#[test]
fn test_preview_cache_key_includes_cell_px() {
    let k1 = AppState::preview_image_cache_key(
        "msg-1",
        "att-9",
        100,
        30,
        100,
        (10, 20),
        TerminalImageProtocol::Sixel,
    );
    let k2 = AppState::preview_image_cache_key(
        "msg-1",
        "att-9",
        100,
        30,
        100,
        (16, 32),
        TerminalImageProtocol::Sixel,
    );
    let k3 = AppState::preview_image_cache_key(
        "msg-1",
        "att-9",
        100,
        30,
        150,
        (10, 20),
        TerminalImageProtocol::Sixel,
    );
    assert_ne!(k1, k2);
    assert_ne!(k1, k3);
    assert!(k1.contains("10x20"));
    assert!(k2.contains("16x32"));
    assert!(k3.contains("150%"));
}

fn make_test_msg(conv_key: &str, msg_id: &str, created_at: &str, body: &str) -> MessageItem {
    MessageItem {
        msg_key: MessageItem::make_msg_key(conv_key, msg_id),
        conv_key: conv_key.to_string(),
        msg_id: msg_id.to_string(),
        parent_msg_id: None,
        reply_to_id: None,
        reply_to_author: None,
        reply_to_preview: None,
        subject: None,
        author_id: None,
        author_name: "Alice".to_string(),
        is_me: false,
        created_at: created_at.to_string(),
        last_modified_at: None,
        deleted: false,
        importance: "normal".to_string(),
        message_type: "message".to_string(),
        body_html: format!("<p>{body}</p>"),
        body_plain: body.to_string(),
        attachments: Vec::new(),
        mentions: Vec::new(),
        reactions: Vec::new(),
        reply_count: 0,
        web_url: None,
    }
}

pub(in crate::ui) fn make_test_app() -> (AppState, UiState) {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let ui = UiState::from_config(&cfg);
    (app, ui)
}

#[tokio::test]
async fn stash_draft_noop_does_not_mark_dirty_and_ignores_stale_loads() {
    let (mut app, mut ui) = make_test_app();
    app.active_conv_key = Some("chat:room-a".to_string());
    app.drafts.clear();

    // Empty composer -> no-op (must not mark dirty or trigger disk write)
    assert!(!app.stash_draft_in_memory(&ui));
    app.stash_draft(&ui);

    // Non-empty draft -> marks dirty; repeating unchanged text -> no-op
    ui.composer.load_text("hello world");
    assert!(app.stash_draft_in_memory(&ui));
    assert_eq!(app.drafts.get("chat:room-a").map(String::as_str), Some("hello world"));
    assert!(!app.stash_draft_in_memory(&ui));

    // Clearing composer -> marks dirty, removes draft; repeating -> no-op
    ui.composer.load_text("");
    assert!(app.stash_draft_in_memory(&ui));
    assert!(!app.drafts.contains_key("chat:room-a"));
    assert!(!app.stash_draft_in_memory(&ui));

    // Stale MessagesLoaded for another room must NOT overwrite active room's messages
    app.loading_messages = true;
    let stale_msg = make_test_msg("chat:room-b", "m1", "2026-11-18T00:00:00Z", "stale");
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessagesLoaded {
            req_seq: app.conv_req_seq,
            conv_key: "chat:room-b".to_string(),
            messages: vec![stale_msg.clone()],
            members: Vec::new(),
        },
    );
    assert!(app.messages.is_empty());
    assert!(app.loading_messages);

    // Matching MessagesLoaded updates state
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessagesLoaded {
            req_seq: app.conv_req_seq,
            conv_key: "chat:room-a".to_string(),
            messages: vec![stale_msg],
            members: Vec::new(),
        },
    );
    assert_eq!(app.messages.len(), 1);
    assert!(!app.loading_messages);

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn optimistic_message_send_with_rollback() {
    let (mut app, mut ui) = make_test_app();

    // Optimistic message send -> replacement -> failure restore
    app.active_conv_key = Some("chat:c1".to_string());
    app.active_target = Some(ConversationTarget::Chat {
        chat_id: "c1".to_string(),
    });
    ui.composer.load_text("hello optimistic");
    app.send_composer(&mut ui);
    assert_eq!(app.messages.len(), 1);
    let opt_id = app.messages[0].msg_id.clone();
    assert!(opt_id.starts_with("optimistic-"));
    assert!(ui.composer.text().is_empty());

    let mut real_msg = app.messages[0].clone();
    real_msg.msg_id = "server-1".to_string();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageSent {
            conv_key: "chat:c1".to_string(),
            parent_msg_id: None,
            optimistic_id: Some(opt_id),
            sent: real_msg,
        },
    );
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].msg_id, "server-1");

    // Failure rollback restores composer text when empty
    ui.composer.load_text("retry me");
    app.send_composer(&mut ui);
    assert_eq!(app.messages.len(), 2);
    let failed_opt_id = app.messages[1].msg_id.clone();
    assert!(ui.composer.text().is_empty());
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageSendFailed {
            conv_key: "chat:c1".to_string(),
            parent_msg_id: None,
            optimistic_id: Some(failed_opt_id),
            text: "retry me".to_string(),
        },
    );
    assert_eq!(app.messages.len(), 1);
    assert_eq!(ui.composer.text(), "retry me");

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

fn make_sized_test_image(label_bytes: usize) -> RenderedTerminalImage {
    RenderedTerminalImage {
        width_px: 10,
        height_px: 10,
        cols: 20,
        rows: 10,
        format_label: "X".repeat(label_bytes),
        lines: vec![],
        protocol: img_preview::TerminalImageProtocol::Halfblock,
        pixel_payload: None,
        scroll_backing: None,
    }
}

#[tokio::test]
async fn image_cache_byte_budget_evicts_and_clear_resets_bytes() {
    let (mut app, _ui) = make_test_app();
    let two_mib = 2 * 1024 * 1024 - 128;

    for i in 0..5 {
        app.insert_image_cache(format!("msg:{i}:20x10"), make_sized_test_image(two_mib));
    }
    assert!(app.image_cache_bytes <= img_preview::MAX_IMAGE_CACHE_BYTES);
    assert_eq!(app.image_cache.len(), 4);

    let expected_bytes: usize = app
        .image_cache
        .values()
        .map(AppState::rendered_image_bytes)
        .sum();
    assert_eq!(app.image_cache_bytes, expected_bytes);

    // Single image larger than MAX_IMAGE_CACHE_BYTES still stays cached as the sole entry.
    let ten_mib = 10 * 1024 * 1024;
    app.insert_image_cache("oversized:0:20x10".to_string(), make_sized_test_image(ten_mib));
    assert_eq!(app.image_cache.len(), 1);
    assert!(app.image_cache.contains_key("oversized:0:20x10"));

    // Clearing resets map, order, and byte counter to zero.
    app.clear_image_caches();
    assert!(app.image_cache.is_empty());
    assert!(app.image_cache_order.is_empty());
    assert_eq!(app.image_cache_bytes, 0);

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

fn seed_root_messages(app: &AppState, conv: &str, count: usize) {
    let base = chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z").unwrap();
    let msgs: Vec<MessageItem> = (0..count)
        .map(|i| {
            let ts = (base + chrono::Duration::minutes(i as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            make_test_msg(conv, &format!("m{i:04}"), &ts, "body")
        })
        .collect();
    app.storage.upsert_messages(&msgs).unwrap();
}

fn save_history_eof(app: &AppState, conv: &str) {
    app.storage
        .save_sync_cursor(&SyncCursor {
            scope_key: SyncCursor::history_scope(conv),
            delta_link: None,
            oldest_msg_ts: Some(SyncCursor::HISTORY_EOF.to_string()),
            synced_at: "2026-11-20T00:00:00Z".to_string(),
        })
        .unwrap();
}

// TEAMS-04: an EOF cursor means Graph has no older page, not that the 80-row window
// shows everything that is cached.
#[tokio::test]
async fn regression_teams_04_oldest_reached_needs_exhausted_cache() {
    let (mut app, mut ui) = make_test_app();
    let chat = |id: &str| ConversationTarget::Chat {
        chat_id: id.to_string(),
    };
    seed_root_messages(&app, "chat:big", 200);
    save_history_eof(&app, "chat:big");
    seed_root_messages(&app, "chat:small", 50);
    save_history_eof(&app, "chat:small");
    seed_root_messages(&app, "chat:open", 200);

    app.select_conversation(&mut ui, chat("big"));
    assert_eq!(app.messages.len(), 80);
    assert!(!app.oldest_reached, "120 older rows are still in the cache");

    app.select_conversation(&mut ui, chat("small"));
    assert_eq!(app.messages.len(), 50);
    assert!(app.oldest_reached);

    app.select_conversation(&mut ui, chat("open"));
    assert_eq!(app.messages.len(), 80);
    assert!(!app.oldest_reached, "no EOF cursor: unchanged");

    // An empty window with an EOF cursor stays "reached".
    save_history_eof(&app, "chat:empty");
    app.messages.clear();
    assert!(app.cached_history_exhausted("chat:empty"));
    assert!(!app.cached_history_exhausted("chat:never-synced"));

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

// TEAMS-11: the drawer must leave "loading" whether the fetch answers or fails, also
// when the root was not cached when it opened.
#[tokio::test]
async fn regression_teams_11_thread_drawer_leaves_loading_state() {
    let (mut app, mut ui) = make_test_app();
    let conv = "chan:t1:ch1";
    app.active_conv_key = Some(conv.to_string());
    app.active_target = Some(ConversationTarget::Channel {
        team_id: "t1".to_string(),
        channel_id: "ch1".to_string(),
    });
    let thread = |root_id: &str| {
        let mut reply = make_test_msg(conv, &format!("{root_id}-r1"), "2026-11-18T01:01:00Z", "reply");
        reply.parent_msg_id = Some(root_id.to_string());
        ThreadView {
            conv_key: conv.to_string(),
            root: make_test_msg(conv, root_id, "2026-11-18T01:00:00Z", "root"),
            replies: vec![reply],
        }
    };
    let loaded = |req_seq: u64, root_id: &str| UiMsg::ThreadLoaded {
        req_seq,
        conv_key: conv.to_string(),
        root_msg_id: root_id.to_string(),
        thread: thread(root_id),
    };
    let failed = |req_seq: u64, root_id: &str| UiMsg::ThreadLoadFailed {
        req_seq,
        conv_key: conv.to_string(),
        root_msg_id: root_id.to_string(),
        error: "503 Service Unavailable".to_string(),
    };
    let shown = |app: &AppState| app.active_thread.as_ref().map(|t| t.root.msg_id.clone());

    // 1) Uncached root, then the answer arrives.
    app.open_thread_by_id(&mut ui, "root-a".to_string());
    let seq_a = app.thread_req_seq;
    assert!(app.loading_thread);
    assert!(app.active_thread.is_none());
    assert_eq!(app.pending_thread, Some((conv.to_string(), "root-a".to_string())));
    app.handle_ui_msg(&mut ui, loaded(seq_a, "root-a"));
    assert!(!app.loading_thread);
    assert!(app.pending_thread.is_none());
    assert_eq!(shown(&app).as_deref(), Some("root-a"));

    // 2) Another uncached root: the previous thread is not shown under it, and a late
    //    answer for the thread the user left is dropped.
    app.open_thread_by_id(&mut ui, "root-b".to_string());
    let seq_b = app.thread_req_seq;
    assert!(app.active_thread.is_none());
    app.handle_ui_msg(&mut ui, loaded(seq_a, "root-a"));
    assert!(app.active_thread.is_none());
    assert!(app.loading_thread);

    // 3) The fetch fails with nothing to show: loading ends, drawer closes, error shown.
    app.handle_ui_msg(&mut ui, failed(seq_b, "root-b"));
    assert!(!app.loading_thread);
    assert!(app.pending_thread.is_none());
    assert!(app.active_thread.is_none());
    assert!(!ui.thread_drawer_open);
    assert!(ui
        .toasts
        .current()
        .is_some_and(|t| t.message.contains("Thread sync failed")));

    // 4) The fetch fails while a cached thread is on screen: it stays.
    let cached = thread("root-c");
    let mut rows = vec![cached.root.clone()];
    rows.extend(cached.replies.clone());
    app.storage.upsert_messages(&rows).unwrap();
    app.open_thread_by_id(&mut ui, "root-c".to_string());
    let seq_c = app.thread_req_seq;
    assert_eq!(shown(&app).as_deref(), Some("root-c"));
    app.handle_ui_msg(&mut ui, failed(seq_c, "root-c"));
    assert!(!app.loading_thread);
    assert!(ui.thread_drawer_open);
    assert_eq!(shown(&app).as_deref(), Some("root-c"));

    // 5) Switching conversation drops the answer of the fetch in flight.
    app.close_thread(&mut ui);
    app.open_thread_by_id(&mut ui, "root-d".to_string());
    let seq_d = app.thread_req_seq;
    seed_root_messages(&app, "chat:other", 3);
    app.select_conversation(
        &mut ui,
        ConversationTarget::Chat {
            chat_id: "other".to_string(),
        },
    );
    assert!(app.pending_thread.is_none());
    assert!(!app.loading_thread);
    app.handle_ui_msg(&mut ui, loaded(seq_d, "root-d"));
    assert!(app.active_thread.is_none());

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn older_messages_loaded_deduplicates_prepends_shifts_selection_and_sets_anchor() {
    let (mut app, mut ui) = make_test_app();
    let conv = "chat:c1";
    app.active_conv_key = Some(conv.to_string());
    app.loading_older = true;
    app.messages = vec![
        make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three"),
        make_test_msg(conv, "m4", "2026-11-18T04:00:00Z", "four"),
    ];
    ui.chat_scroll.follow_tail = false;
    ui.chat_scroll.total_lines = 12;
    ui.chat_scroll.offset = 0;
    ui.chat_scroll.selected_idx = Some(0);

    app.handle_ui_msg(
        &mut ui,
        UiMsg::OlderMessagesLoaded {
            req_seq: app.older_req_seq,
            conv_key: conv.to_string(),
            messages: vec![
                make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two"),
                make_test_msg(conv, "m1", "2026-11-18T01:00:00Z", "one"),
                make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three"),
            ],
            reached_eof: false,
        },
    );

    assert!(!app.loading_older);
    assert!(!app.oldest_reached);
    assert_eq!(app.history_error, None);
    let ids: Vec<&str> = app.messages.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(ids, vec!["m1", "m2", "m3", "m4"]);
    assert_eq!(ui.chat_scroll.selected_idx, Some(2));
    assert_eq!(ui.pending_prepend_anchor, Some((12, 0)));

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn older_messages_failed_preserves_oldest_reached_false_and_sets_error_banner() {
    let (mut app, mut ui) = make_test_app();
    let conv = "chat:c1";
    app.active_conv_key = Some(conv.to_string());
    app.loading_older = true;
    app.oldest_reached = false;
    app.messages = vec![make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three")];

    let err_msg = format!("{} missing Chat.Read", crate::auth::INSUFFICIENT_PERMISSIONS_PREFIX);
    app.handle_ui_msg(
        &mut ui,
        UiMsg::OlderMessagesFailed {
            req_seq: app.older_req_seq,
            conv_key: conv.to_string(),
            local_messages: vec![make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two")],
            error: err_msg.clone(),
        },
    );

    assert!(!app.loading_older);
    assert!(!app.oldest_reached);
    assert_eq!(app.history_error.as_deref(), Some(err_msg.as_str()));
    let ids: Vec<&str> = app.messages.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(ids, vec!["m2", "m3"]);

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[test]
fn merge_optimistic_messages_preserves_loaded_older_history() {
    let conv = "chat:c1";
    let mut target = vec![
        make_test_msg(conv, "m1", "2026-11-18T01:00:00Z", "one"),
        make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two"),
        make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three"),
        make_test_msg(conv, "optimistic-1", "2026-11-18T05:00:00Z", "pending"),
    ];
    let incoming = vec![
        make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three-updated"),
        make_test_msg(conv, "m4", "2026-11-18T04:00:00Z", "four"),
    ];

    merge_optimistic_messages(&mut target, incoming);
    let ids: Vec<&str> = target.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(ids, vec!["m1", "m2", "m3", "m4", "optimistic-1"]);
    assert_eq!(target[2].body_plain, "three-updated");
}

#[tokio::test]
async fn daemon_attached_skips_duplicate_bootstrap_and_preserves_focus() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    app.my_profile = Some(UserProfile {
        id: "user-1".to_string(),
        display_name: "Alice".to_string(),
        mail: Some("alice@example.com".to_string()),
        user_principal_name: Some("alice@example.com".to_string()),
    });
    app.my_user_id = "user-1".to_string();
    app.active_target = Some(ConversationTarget::Chat {
        chat_id: "chat-42".to_string(),
    });

    app.handle_ui_msg(
        &mut ui,
        UiMsg::DaemonAttached {
            pid: Some(4242),
            sync_busy: false,
        },
    );

    assert!(app.daemon_attached);
    assert_eq!(app.daemon_pid, Some(4242));
    assert!(!app.syncing);
    assert_eq!(app.sync_summary, "Daemon connected");
    assert!(ui.tasks.first_running().is_none());

    assert!(nudge_rx.try_recv().is_err(), "unexpected extra bootstrap command");
    assert_eq!(
        focus_rx.borrow().target,
        Some(ConversationTarget::Chat {
            chat_id: "chat-42".to_string(),
        })
    );

    app.handle_ui_msg(&mut ui, UiMsg::DaemonDetached);
    assert!(!app.daemon_attached);
    assert_eq!(app.daemon_pid, None);
    assert_eq!(app.sync_summary, "Local sync");
}

#[tokio::test]
async fn select_conversation_focuses_composer_snapshots_unread_and_resets_transient_state() {
    let (mut app, mut ui) = make_test_app();
    app.conversations = vec![
        ConversationSummary {
            conv_key: "chat:r1".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "r1".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Room 1".to_string(),
            topic: None,
            unread_count: 3,
            last_read_at: Some("2026-11-18T09:00:00Z".to_string()),
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: None,
            pinned: false,
            peer_user_id: None,
            web_url: None,
        },
        ConversationSummary {
            conv_key: "chat:r2".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "r2".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Room 2".to_string(),
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
    ];

    assert_eq!(ui.focus(), Focus::Nav);
    app.select_conversation(
        &mut ui,
        ConversationTarget::Chat {
            chat_id: "r1".to_string(),
        },
    );
    assert_eq!(ui.focus(), Focus::Composer);
    assert_eq!(ui.narrow_pane, NarrowPane::Chat);
    assert_eq!(
        ui.unread_anchor,
        Some(("chat:r1".to_string(), "2026-11-18T09:00:00Z".to_string()))
    );
    assert_eq!(app.conversations[0].unread_count, 0);

    ui.composer.load_text("draft one");
    ui.composer.set_reply(ReplyPreview {
        msg_id: "m1".to_string(),
        author: "Alice".to_string(),
        text: "hi".to_string(),
    });
    ui.composer.subject = "Sub".to_string();
    ui.composer.subject_cursor = 3;
    ui.composer.subject_focused = true;
    ui.composer
        .stage_inline_image(vec![1, 2, 3], "image/png", Some("a.png".to_string()));

    app.select_conversation(
        &mut ui,
        ConversationTarget::Chat {
            chat_id: "r2".to_string(),
        },
    );
    assert_eq!(ui.focus(), Focus::Composer);
    assert!(ui.composer.reply.is_none());
    assert!(ui.composer.subject.is_empty());
    assert_eq!(ui.composer.subject_cursor, 0);
    assert!(!ui.composer.subject_focused);
    assert!(ui.composer.pending_images.is_empty());
    assert_eq!(ui.last_conv_key.as_deref(), Some("chat:r1"));
    assert_eq!(ui.conv_history, vec!["chat:r1", "chat:r2"]);
    assert_eq!(ui.conv_history_pos, 1);

    app.history_back(&mut ui);
    assert_eq!(app.active_conv_key.as_deref(), Some("chat:r1"));
    assert_eq!(ui.composer.text(), "draft one");
    assert_eq!(ui.focus(), Focus::Composer);

    app.history_forward(&mut ui);
    assert_eq!(app.active_conv_key.as_deref(), Some("chat:r2"));
    assert_eq!(ui.focus(), Focus::Composer);

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn poll_retry_spinner_and_image_error_cap_and_append_anchor() {
    let (mut app, mut ui) = make_test_app();
    app.sync_summary = "Synced".to_string();

    // Transient poll retry sets poll_retrying without clobbering sync_summary or setting syncing.
    app.handle_core_event(
        &mut ui,
        CoreEvent::SyncStateChanged {
            busy: false,
            summary: crate::models::POLL_RETRY_SUMMARY.to_string(),
        },
    );
    assert!(ui.poll_retrying);
    assert!(!app.syncing);
    assert_eq!(app.sync_summary, "Synced");

    // Synced event clears poll_retrying.
    app.handle_core_event(
        &mut ui,
        CoreEvent::SyncStateChanged {
            busy: false,
            summary: "Synced".to_string(),
        },
    );
    assert!(!ui.poll_retrying);

    // ConversationsUpdated clears poll_retrying.
    ui.poll_retrying = true;
    app.handle_core_event(&mut ui, CoreEvent::ConversationsUpdated(vec![]));
    assert!(!ui.poll_retrying);

    // image_errors is capped at 64 entries and cleared by clear_image_cache.
    for i in 0..70 {
        app.insert_image_error(format!("k{i}"), format!("err{i}"));
    }
    assert_eq!(app.image_errors.len(), 64);
    assert!(!app.image_errors.contains_key("k0"));
    assert!(app.image_errors.contains_key("k69"));

    let gen_before = app.image_cache_gen;
    app.clear_image_cache();
    assert!(app.image_errors.is_empty());
    assert_eq!(app.image_cache_gen, gen_before + 1);

    // Appending messages while scrolled up preserves viewport offset.
    let conv = "chat:c1";
    app.active_conv_key = Some(conv.to_string());
    let m1 = make_test_msg(conv, "m1", "2026-11-18T01:00:00Z", "one");
    let m2 = make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two");
    app.storage.upsert_messages(&[m1.clone(), m2]).unwrap();
    app.messages = vec![m1];
    ui.chat_scroll.follow_tail = false;
    ui.chat_scroll.scroll_up = 10;
    ui.chat_scroll.total_lines = 50;
    ui.chat_scroll.offset = 20;

    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![],
            notify_candidate: None,
            changed_msg_ids: None,
        },
    );
    assert_eq!(ui.pending_append_anchor, Some(20));

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn raw_image_cache_hit_skips_fetch_respects_budget_and_clears() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let sync_ctrl =
        sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    // 1. Entry count cap (8 entries) and 16 MiB byte budget enforcement.
    for i in 0..10 {
        let key = AppState::raw_image_cache_key("m", &format!("a{i}"));
        app.insert_raw_image_cache(key, Arc::from(vec![i as u8; 1024]));
    }
    assert_eq!(app.image_raw_cache.len(), MAX_RAW_IMAGE_CACHE_ENTRIES);
    assert_eq!(app.image_raw_bytes, MAX_RAW_IMAGE_CACHE_ENTRIES * 1024);
    assert!(!app.image_raw_cache.contains_key("m:a0"));
    assert!(!app.image_raw_cache.contains_key("m:a1"));
    assert!(app.image_raw_cache.contains_key("m:a9"));

    // Inserting 6 MiB chunks must evict older entries so total stays <= 16 MiB.
    let six_mib = 6 * 1024 * 1024;
    for i in 0..3 {
        let key = AppState::raw_image_cache_key("big", &format!("b{i}"));
        app.insert_raw_image_cache(key, Arc::from(vec![1u8; six_mib]));
    }
    assert!(app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);
    assert_eq!(app.image_raw_cache.len(), 2);
    assert!(!app.image_raw_cache.contains_key("big:b0"));
    assert!(app.image_raw_cache.contains_key("big:b1"));
    assert!(app.image_raw_cache.contains_key("big:b2"));

    // Oversized single entry (>16 MiB) is not cached.
    let seventeen_mib = 17 * 1024 * 1024;
    app.insert_raw_image_cache(
        AppState::raw_image_cache_key("huge", "h0"),
        Arc::from(vec![0u8; seventeen_mib]),
    );
    assert!(app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);
    assert!(!app.image_raw_cache.contains_key("huge:h0"));

    // clear_image_caches resets raw cache, order, and byte counter.
    app.clear_image_caches();
    assert!(app.image_raw_cache.is_empty());
    assert!(app.image_raw_order.is_empty());
    assert_eq!(app.image_raw_bytes, 0);

    // 2. Raw cache hit skips network fetch across zoom & protocol changes.
    let png = super::super::composer::encode_rgba_to_png(4, 4, vec![200u8; 4 * 4 * 4]).unwrap();
    let target = PreviewTarget::Teams {
        msg_id: "msg-offline".to_string(),
        attachment: AttachmentInfo {
            id: "att-1".to_string(),
            content_type: "image/png".to_string(),
            content_url: None,
            name: Some("pic.png".to_string()),
            content_json: None,
            thumbnail_url: None,
        },
    };
    app.insert_raw_image_cache(
        AppState::raw_image_cache_key(target.owner_id(), target.att_id()),
        Arc::from(png),
    );

    let caps_half = TerminalGraphicsCaps {
        protocol: TerminalImageProtocol::Halfblock,
        cell_px: (8, 16),
        cell_px_exact: false,
    };
    let fit_half = img_preview::FitBox::new(40, 20, 100, true);
    app.request_preview_image(&target, fit_half, caps_half, (37, 42, 61));
    let msg1 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("decode timed out")
        .expect("channel closed");
    match msg1 {
        UiMsg::ImageDecoded { cache_key, rendered } => {
            assert_eq!(
                cache_key,
                AppState::preview_image_cache_key(
                    "msg-offline",
                    "att-1",
                    40,
                    20,
                    100,
                    (8, 16),
                    TerminalImageProtocol::Halfblock
                )
            );
            assert_eq!(rendered.protocol, TerminalImageProtocol::Halfblock);
            app.handle_ui_msg(&mut ui, UiMsg::ImageDecoded { cache_key, rendered });
        }
        _ => panic!("expected ImageDecoded on raw cache hit"),
    }

    // Switch protocol to Sixel and zoom to 80x40 offline without any network token!
    let caps_sixel = TerminalGraphicsCaps {
        protocol: TerminalImageProtocol::Sixel,
        cell_px: (8, 16),
        cell_px_exact: false,
    };
    let fit_sixel = img_preview::FitBox::new(80, 40, 125, true);
    app.request_preview_image(&target, fit_sixel, caps_sixel, (37, 42, 61));
    let msg2 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("sixel decode timed out")
        .expect("channel closed");
    match msg2 {
        UiMsg::ImageDecoded { cache_key, rendered } => {
            assert_eq!(
                cache_key,
                AppState::preview_image_cache_key(
                    "msg-offline",
                    "att-1",
                    80,
                    40,
                    125,
                    (8, 16),
                    TerminalImageProtocol::Sixel
                )
            );
            assert_eq!(rendered.protocol, TerminalImageProtocol::Sixel);
            assert!(rendered.pixel_payload.is_some());
        }
        _ => panic!("expected Sixel ImageDecoded on raw cache hit"),
    }

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn incremental_chat_updates_read_only_changed_rows_for_80_and_830_windows() {
    use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

    for initial_count in [80usize, 830usize] {
        let (mut app, mut ui) = make_test_app();
        let conv = "chat:perf-room";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "perf-room".to_string(),
        });

        let mut seed = Vec::with_capacity(initial_count);
        for i in 0..initial_count {
            let ts = format!(
                "2026-11-20T{:02}:{:02}:{:02}Z",
                i / 3600,
                (i / 60) % 60,
                i % 60
            );
            seed.push(make_test_msg(
                conv,
                &format!("m{:04}", i + 1),
                &ts,
                &format!("msg {}", i + 1),
            ));
        }
        app.storage.upsert_messages(&seed).unwrap();
        app.messages = seed;

        // 1) 0 changed rows -> 0 SQLite row reads.
        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: Vec::new(),
                notify_candidate: None,
                changed_msg_ids: Some(Vec::new()),
            },
        );
        assert_eq!(mapped_message_rows(), 0, "0 changes must read 0 rows");
        assert_eq!(app.messages.len(), initial_count);

        // 2) 1 new root message while scrolled up -> reads exactly 1 row & sets append anchor.
        let new_msg = make_test_msg(
            conv,
            &format!("m{:04}", initial_count + 1),
            "2026-11-20T23:00:00Z",
            "newest arrival",
        );
        app.storage
            .upsert_messages(std::slice::from_ref(&new_msg))
            .unwrap();
        ui.chat_scroll.follow_tail = false;
        ui.chat_scroll.scroll_up = 12;
        ui.chat_scroll.total_lines = 400;
        ui.chat_scroll.offset = 100;
        ui.pending_append_anchor = None;

        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![new_msg.clone()],
                notify_candidate: None,
                changed_msg_ids: Some(vec![new_msg.msg_id.clone()]),
            },
        );
        assert_eq!(
            mapped_message_rows(),
            1,
            "1 new message in {initial_count}-row chat must read exactly 1 row"
        );
        assert_eq!(app.messages.len(), initial_count + 1);
        assert_eq!(app.messages.last().unwrap().msg_id, new_msg.msg_id);
        assert_eq!(ui.pending_append_anchor, Some(100));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}

#[tokio::test]
async fn incremental_chat_updates_preserve_order_edits_deletes_optimistic_and_snapshots() {
    use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

    let (mut app, mut ui) = make_test_app();
    let conv = "chat:group-1";
    app.active_conv_key = Some(conv.to_string());
    app.active_target = Some(ConversationTarget::Chat {
        chat_id: "group-1".to_string(),
    });
    app.conversations.push(ConversationSummary {
        conv_key: conv.to_string(),
        target: ConversationTarget::Chat {
            chat_id: "group-1".to_string(),
        },
        kind: ConversationKind::Group,
        team_id: None,
        team_name: None,
        title: "Group Chat".to_string(),
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
    });

    // 1) Empty window uses snapshot fallback even if changed_msg_ids is Some([]).
    let m1 = make_test_msg(conv, "m1", "2026-11-20T01:00:00Z", "one");
    let m3 = make_test_msg(conv, "m3", "2026-11-20T02:00:00Z", "three");
    app.storage
        .upsert_messages(&[m1.clone(), m3.clone()])
        .unwrap();
    assert!(app.messages.is_empty());
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: Vec::new(),
            notify_candidate: None,
            changed_msg_ids: Some(Vec::new()),
        },
    );
    assert_eq!(
        app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["m1", "m3"]
    );

    // 2) Same-timestamp tie-break (m2 < m3 at identical timestamp) + selection shift.
    let m2 = make_test_msg(conv, "m2", "2026-11-20T02:00:00Z", "two");
    app.storage.upsert_messages(std::slice::from_ref(&m2)).unwrap();
    ui.chat_scroll.selected_idx = Some(1); // pointing at m3
    reset_mapped_message_rows();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![m2.clone()],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["m2".to_string()]),
        },
    );
    assert_eq!(mapped_message_rows(), 1);
    assert_eq!(
        app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["m1", "m2", "m3"]
    );
    assert_eq!(ui.chat_scroll.selected_idx, Some(2)); // still pointing at m3

    // 3) Edit, soft-delete, and duplicate delivery update in-place without duplicating.
    let mut m2_edited = m2.clone();
    m2_edited.body_plain = "two edited".to_string();
    m2_edited.body_html = "<p>two edited</p>".to_string();
    m2_edited.last_modified_at = Some("2026-11-20T02:05:00Z".to_string());
    let mut m3_deleted = m3.clone();
    m3_deleted.deleted = true;
    app.storage
        .upsert_messages(&[m2_edited.clone(), m3_deleted.clone()])
        .unwrap();
    reset_mapped_message_rows();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![m2_edited, m3_deleted],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["m2".to_string(), "m3".to_string(), "m2".to_string()]),
        },
    );
    assert_eq!(mapped_message_rows(), 2);
    assert_eq!(app.messages.len(), 3);
    assert_eq!(app.messages[1].body_plain, "two edited");
    assert!(app.messages[2].deleted);

    // 4) Optimistic reconciliation:
    //    (a) Poll arrives before MessageSent -> server row inserted before optimistic, then MessageSent removes optimistic.
    let opt = make_test_msg(conv, "optimistic-1", "2026-11-20T03:00:00Z", "sending");
    app.messages.push(opt);
    let m4 = make_test_msg(conv, "m4", "2026-11-20T02:59:59Z", "sending");
    app.storage.upsert_messages(std::slice::from_ref(&m4)).unwrap();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![m4.clone()],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["m4".to_string()]),
        },
    );
    assert_eq!(
        app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["m1", "m2", "m3", "m4", "optimistic-1"]
    );
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageSent {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            optimistic_id: Some("optimistic-1".to_string()),
            sent: m4.clone(),
        },
    );
    assert_eq!(
        app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["m1", "m2", "m3", "m4"]
    );

    //    (b) MessageSent arrives before duplicate poll -> poll replaces in-place without duplicating.
    let opt2 = make_test_msg(conv, "optimistic-2", "2026-11-20T04:00:00Z", "second");
    app.messages.push(opt2);
    let m5 = make_test_msg(conv, "m5", "2026-11-20T04:00:01Z", "second");
    app.storage.upsert_messages(std::slice::from_ref(&m5)).unwrap();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageSent {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            optimistic_id: Some("optimistic-2".to_string()),
            sent: m5.clone(),
        },
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![m5.clone()],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["m5".to_string()]),
        },
    );
    assert_eq!(
        app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
        vec!["m1", "m2", "m3", "m4", "m5"]
    );

    // 5) Out-of-window old message is NOT injected into a full (>=80) window when !oldest_reached.
    let mut full_window = Vec::with_capacity(80);
    for i in 100..180 {
        full_window.push(make_test_msg(
            conv,
            &format!("w{i:03}"),
            &format!("2026-11-20T10:{:02}:{:02}Z", (i - 100) / 60, (i - 100) % 60),
            "win",
        ));
    }
    let very_old = make_test_msg(conv, "w001", "2026-11-19T00:00:00Z", "ancient");
    app.storage.upsert_messages(&full_window).unwrap();
    app.storage
        .upsert_messages(std::slice::from_ref(&very_old))
        .unwrap();
    app.messages = full_window;
    app.oldest_reached = false;
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![very_old],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["w001".to_string()]),
        },
    );
    assert_eq!(app.messages.len(), 80);
    assert_eq!(app.messages[0].msg_id, "w100");

    // 6) Legacy IPC event (changed_msg_ids: None) falls back to snapshot list_root_messages.
    let m_snap = make_test_msg(conv, "w999", "2026-11-20T23:59:59Z", "snapshot fallback");
    app.storage
        .upsert_messages(std::slice::from_ref(&m_snap))
        .unwrap();
    reset_mapped_message_rows();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: Vec::new(),
            notify_candidate: None,
            changed_msg_ids: None,
        },
    );
    assert!(mapped_message_rows() >= 80);
    assert_eq!(app.messages.last().unwrap().msg_id, "w999");

    // 7) Mentions refresh on changed mention row, skipped on 0-change event.
    app.my_user_id = "me-mention".to_string();
    reset_mapped_message_rows();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: Vec::new(),
            notify_candidate: None,
            changed_msg_ids: Some(Vec::new()),
        },
    );
    assert_eq!(mapped_message_rows(), 0);
    assert!(app.activity_mentions.is_empty());

    let mut mention_msg = make_test_msg(conv, "w1000", "2026-11-20T23:59:59.5Z", "@Me check");
    mention_msg.mentions.push(crate::models::MentionInfo {
        id: 0,
        user_id: "me-mention".to_string(),
        display_name: "Me".to_string(),
    });
    app.storage
        .upsert_messages(std::slice::from_ref(&mention_msg))
        .unwrap();
    ui.notify = false;
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![mention_msg.clone()],
            notify_candidate: Some(NotificationCandidate {
                conv_key: conv.to_string(),
                conv_title: "Group Chat".to_string(),
                sender: "Alice".to_string(),
                preview: "@Me check".to_string(),
            }),
            changed_msg_ids: Some(vec!["w1000".to_string()]),
        },
    );
    assert_eq!(app.activity_mentions.len(), 1);
    assert_eq!(app.activity_mentions[0].msg_id, "w1000");

    // 8) Channel conversation still uses snapshot path + refreshes open thread.
    let chan_key = "chan:t1:ch1";
    let root = make_test_msg(chan_key, "r1", "2026-11-20T01:00:00Z", "root");
    let mut reply = make_test_msg(chan_key, "r1-rep1", "2026-11-20T01:05:00Z", "reply");
    reply.parent_msg_id = Some("r1".to_string());
    app.storage
        .upsert_messages(&[root.clone(), reply])
        .unwrap();
    app.active_conv_key = Some(chan_key.to_string());
    app.active_target = Some(ConversationTarget::Channel {
        team_id: "t1".to_string(),
        channel_id: "ch1".to_string(),
    });
    app.messages = vec![root.clone()];
    app.active_thread = Some(ThreadView {
        conv_key: chan_key.to_string(),
        root,
        replies: Vec::new(),
    });
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: chan_key.to_string(),
            parent_msg_id: Some("r1".to_string()),
            messages: Vec::new(),
            notify_candidate: None,
            changed_msg_ids: Some(vec!["r1-rep1".to_string()]),
        },
    );
    assert_eq!(app.active_thread.as_ref().unwrap().replies.len(), 1);
    assert_eq!(app.active_thread.as_ref().unwrap().replies[0].msg_id, "r1-rep1");

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn channel_root_reaction_maps_one_row_and_reply_deletion_refreshes_parent() {
    use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

    let (mut app, mut ui) = make_test_app();
    let conv_key = "chan:t1:ch1";
    app.active_conv_key = Some(conv_key.into());
    app.active_target = ConversationTarget::from_conv_key(conv_key);
    let roots: Vec<_> = (1..=1580)
        .map(|i| {
            make_test_msg(
                conv_key,
                &format!("root-{i:04}"),
                "2026-11-20T01:00:00Z",
                "root",
            )
        })
        .collect();
    let root_index = roots.len() - 1;
    let mut changed_root = roots[root_index].clone();
    let mut reply = make_test_msg(conv_key, "reply", "2026-11-20T01:05:00Z", "reply");
    reply.parent_msg_id = Some(changed_root.msg_id.clone());
    app.storage.upsert_messages(&roots).unwrap();
    app.storage.upsert_messages(&[reply.clone()]).unwrap();
    app.messages = app.storage.list_root_messages(conv_key, 1580, None).unwrap();
    ui.chat_scroll.selected_idx = Some(root_index);
    assert_eq!(app.messages[root_index].reply_count, 1);

    let event = |message: MessageItem| CoreEvent::MessagesUpdated {
        conv_key: conv_key.into(),
        parent_msg_id: None,
        changed_msg_ids: Some(vec![message.msg_id.clone()]),
        messages: vec![message],
        notify_candidate: None,
    };
    changed_root.reactions.push(crate::models::ReactionSummary {
        reaction_type: "like".into(),
        count: 1,
        me_reacted: false,
        users: vec!["someone".into()],
    });
    app.storage.upsert_messages(&[changed_root.clone()]).unwrap();
    reset_mapped_message_rows();
    app.handle_core_event(&mut ui, event(changed_root));
    assert_eq!(mapped_message_rows(), 1);
    assert_eq!(app.messages[root_index].reactions[0].count, 1);
    assert_eq!(app.messages[root_index].reply_count, 1);
    assert_eq!(ui.chat_scroll.selected_idx, Some(root_index));
    assert!(app.tail_message_updated);

    reply.deleted = true;
    reply.parent_msg_id = None;
    reply.created_at.clear();
    reply.body_html.clear();
    reply.body_plain.clear();
    app.storage.upsert_messages(&[reply.clone()]).unwrap();
    reset_mapped_message_rows();
    app.handle_core_event(&mut ui, event(reply));
    assert_eq!(mapped_message_rows(), 1580);
    assert_eq!(app.messages[root_index].reply_count, 0);
    assert_eq!(app.messages[root_index].reactions[0].count, 1);
    assert!(!app.tail_message_updated);
    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn tui_preview_mention_removal_delete_and_active_read_guard() {
    use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

    let (mut app, mut ui) = make_test_app();
    let conv = "chat:c-tui";
    app.my_user_id = "me-1".to_string();
    app.active_conv_key = Some(conv.to_string());
    app.active_target = Some(ConversationTarget::Chat {
        chat_id: "c-tui".to_string(),
    });
    let summary = ConversationSummary {
        conv_key: conv.to_string(),
        target: ConversationTarget::Chat {
            chat_id: "c-tui".to_string(),
        },
        kind: ConversationKind::OneOnOne,
        team_id: None,
        team_name: None,
        title: "Alice".to_string(),
        topic: None,
        unread_count: 0,
        last_read_at: Some("2026-11-20T05:00:00Z".to_string()),
        last_msg_id: Some("m1".to_string()),
        last_msg_author: Some("Alice".to_string()),
        last_msg_preview: Some("@Me original".to_string()),
        last_updated_at: Some("2026-11-20T05:00:00Z".to_string()),
        pinned: false,
        peer_user_id: Some("u2".to_string()),
        web_url: None,
    };
    app.storage
        .upsert_conversations(std::slice::from_ref(&summary))
        .unwrap();
    app.conversations = vec![summary];

    // 1) Message with @me mention -> edited to remove @me -> activity_mentions must clear!
    let mut m1 = make_test_msg(conv, "m1", "2026-11-20T05:00:00Z", "@Me original");
    m1.mentions.push(crate::models::MentionInfo {
        id: 0,
        user_id: "me-1".to_string(),
        display_name: "Me".to_string(),
    });
    app.storage.upsert_messages(std::slice::from_ref(&m1)).unwrap();
    app.messages = vec![m1.clone()];
    app.activity_mentions = vec![m1.clone()];

    let mut m1_no_mention = m1.clone();
    m1_no_mention.mentions.clear();
    m1_no_mention.body_plain = "edited without mention".to_string();
    m1_no_mention.body_html = "<p>edited without mention</p>".to_string();
    m1_no_mention.last_modified_at = Some("2026-11-20T05:01:00Z".to_string());
    app.storage
        .upsert_messages(std::slice::from_ref(&m1_no_mention))
        .unwrap();

    reset_mapped_message_rows();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: conv.to_string(),
            parent_msg_id: None,
            messages: vec![m1_no_mention],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["m1".to_string()]),
        },
    );
    assert_eq!(mapped_message_rows(), 1, "single edited row must read 1 message row");
    assert!(
        app.activity_mentions.is_empty(),
        "removing @me via edit must clear activity_mentions"
    );
    assert_eq!(app.messages[0].body_plain, "edited without mention");

    // 2) Active read guard: stale ConversationsUpdated with unread_count=1 must not resurrect UNREAD.
    app.active_marked_read_at = Some((conv.to_string(), "2026-11-20T05:05:00Z".to_string()));
    let mut stale_conv = app.conversations[0].clone();
    stale_conv.unread_count = 1;
    stale_conv.last_updated_at = Some("2026-11-20T05:00:00Z".to_string());
    app.handle_core_event(&mut ui, CoreEvent::ConversationsUpdated(vec![stale_conv]));
    assert_eq!(app.conversations[0].unread_count, 0);

    // 3) UiMsg::MessageDeleted updates active message, thread root, and conversation preview.
    app.active_thread = Some(ThreadView {
        conv_key: conv.to_string(),
        root: app.messages[0].clone(),
        replies: Vec::new(),
    });
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageDeleted {
            conv_key: conv.to_string(),
            msg_id: "m1".to_string(),
            parent_msg_id: None,
        },
    );
    assert!(app.messages[0].deleted);
    assert_eq!(app.messages[0].body_plain, "(deleted message)");
    assert!(app.active_thread.as_ref().unwrap().root.deleted);
    assert_eq!(
        app.conversations[0].last_msg_preview.as_deref(),
        Some("(deleted message)")
    );

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn background_channel_events_update_sidebar_without_disturbing_open_conv_thread_or_draft() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, _core_rx) = mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), core_tx);

    let team = TeamSummary {
        id: "t1".to_string(),
        display_name: "Eng".to_string(),
        description: None,
        channels: vec![ChannelSummary {
            id: "ch1".to_string(),
            team_id: "t1".to_string(),
            team_name: "Eng".to_string(),
            display_name: "General".to_string(),
            description: None,
            membership_type: "standard".to_string(),
            web_url: None,
        }],
    };
    storage.upsert_teams(std::slice::from_ref(&team)).unwrap();
    let active_chat_target = ConversationTarget::Chat {
        chat_id: "c-open".to_string(),
    };
    let active_key = active_chat_target.conv_key();
    storage
        .upsert_conversations(&[ConversationSummary {
            conv_key: active_key.clone(),
            target: active_chat_target.clone(),
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Alice".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: Some("a1".to_string()),
            last_msg_author: Some("Alice".to_string()),
            last_msg_preview: Some("hello".to_string()),
            last_updated_at: Some("2026-11-20T05:00:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u2".to_string()),
            web_url: None,
        }])
        .unwrap();
    let active_msg = make_test_msg(&active_key, "a1", "2026-11-20T05:00:00Z", "hello");
    storage
        .upsert_messages(std::slice::from_ref(&active_msg))
        .unwrap();

    let mut app = AppState::new(cfg.clone(), auth, graph, storage.clone(), sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    app.teams = vec![team];
    app.conversations = storage.list_conversations().unwrap();
    app.active_conv_key = Some(active_key.clone());
    app.active_target = Some(active_chat_target);
    app.messages = vec![active_msg.clone()];
    app.active_thread = Some(ThreadView {
        conv_key: active_key.clone(),
        root: active_msg,
        replies: Vec::new(),
    });
    ui.chat_scroll.selected_idx = Some(0);
    ui.composer.load_text("draft in progress");

    let bg_chan_key = "chan:t1:ch1".to_string();
    let bg_msg = make_test_msg(&bg_chan_key, "cm-1", "2026-11-20T05:10:00Z", "Channel news");
    storage
        .upsert_messages(std::slice::from_ref(&bg_msg))
        .unwrap();
    storage.increment_conversation_unread(&bg_chan_key, 1).unwrap();
    let updated_convs = storage.list_conversations().unwrap();

    app.handle_core_event(
        &mut ui,
        CoreEvent::MessagesUpdated {
            conv_key: bg_chan_key.clone(),
            parent_msg_id: None,
            messages: vec![bg_msg],
            notify_candidate: None,
            changed_msg_ids: Some(vec!["cm-1".to_string()]),
        },
    );
    app.handle_core_event(&mut ui, CoreEvent::ConversationsUpdated(updated_convs));

    assert_eq!(app.active_conv_key.as_deref(), Some(active_key.as_str()));
    assert_eq!(ui.chat_scroll.selected_idx, Some(0));
    assert_eq!(ui.composer.text(), "draft in progress");
    assert_eq!(
        app.active_thread.as_ref().map(|t| t.root.msg_id.as_str()),
        Some("a1")
    );
    assert_eq!(app.unread_channel_count(), 1);
    ui.active_nav_tab = crate::ui::nav_pane::TAB_TEAMS;
    let rows = crate::ui::nav_pane::nav_rows_cached(&app, &ui);
    assert!(rows.iter().any(|r| matches!(
        r,
        crate::ui::nav_pane::NavRow::TeamChannel { conv_key, unread: 1, .. }
        if conv_key == &bg_chan_key
    )));

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn async_loader_seq_guards_and_thread_optimistic_merge() {
    let (mut app, mut ui) = make_test_app();
    let conv = "chan:t1:ch1";
    app.active_conv_key = Some(conv.to_string());
    app.active_target = Some(ConversationTarget::Channel {
        team_id: "t1".into(),
        channel_id: "ch1".into(),
    });

    // 1) UserSearchResults: stale seq or mismatched NewChat query must be ignored.
    app.user_search_seq = 5;
    app.user_searching = true;
    let mut nc = super::super::overlays::new_chat::NewChatState::new();
    nc.peers_input = "bob".into();
    ui.push_overlay(Overlay::NewChat(nc));
    let alice = UserProfile {
        id: "u1".into(),
        display_name: "Alice".into(),
        mail: None,
        user_principal_name: None,
    };
    let bob = UserProfile {
        id: "u2".into(),
        display_name: "Bob".into(),
        mail: None,
        user_principal_name: None,
    };
    app.handle_ui_msg(
        &mut ui,
        UiMsg::UserSearchResults {
            req_seq: 4,
            query: "bob".into(),
            users: vec![alice.clone()],
        },
    );
    assert!(app.user_searching);
    assert!(app.user_search_results.is_empty());

    app.handle_ui_msg(
        &mut ui,
        UiMsg::UserSearchResults {
            req_seq: 5,
            query: "alice".into(),
            users: vec![alice],
        },
    );
    assert!(app.user_searching);
    assert!(app.user_search_results.is_empty());

    app.handle_ui_msg(
        &mut ui,
        UiMsg::UserSearchResults {
            req_seq: 5,
            query: "bob".into(),
            users: vec![bob],
        },
    );
    assert!(!app.user_searching);
    assert_eq!(app.user_search_results.len(), 1);
    assert_eq!(app.user_search_results[0].id, "u2");
    ui.overlays.clear();

    // 2) ThreadLoaded: stale req_seq ignored; current req_seq merges optimistic replies & reveals target reply_id.
    let root = make_test_msg(conv, "root-1", "2026-11-20T00:00:00Z", "root");
    let mut opt_reply = make_test_msg(conv, "optimistic-1", "2026-11-20T00:05:00Z", "pending");
    opt_reply.parent_msg_id = Some("root-1".into());
    app.messages = vec![root.clone()];
    app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), Some("rep-2".into()));
    let seq = app.thread_req_seq;
    app.active_thread.as_mut().unwrap().replies.push(opt_reply);

    let mut rep1 = make_test_msg(conv, "rep-1", "2026-11-20T00:01:00Z", "first");
    rep1.parent_msg_id = Some("root-1".into());
    let mut rep2 = make_test_msg(conv, "rep-2", "2026-11-20T00:02:00Z", "target");
    rep2.parent_msg_id = Some("root-1".into());
    let fetched = ThreadView {
        conv_key: conv.to_string(),
        root: root.clone(),
        replies: vec![rep1, rep2],
    };

    // Stale seq must be rejected.
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ThreadLoaded {
            req_seq: seq - 1,
            conv_key: conv.to_string(),
            root_msg_id: "root-1".into(),
            thread: fetched.clone(),
        },
    );
    assert!(app.loading_thread);

    // Current seq merges optimistic reply AND selects/reveals rep-2 (index 1 + 1 = 2).
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ThreadLoaded {
            req_seq: seq,
            conv_key: conv.to_string(),
            root_msg_id: "root-1".into(),
            thread: fetched,
        },
    );
    assert!(!app.loading_thread);
    let reply_ids: Vec<&str> = app
        .active_thread
        .as_ref()
        .unwrap()
        .replies
        .iter()
        .map(|r| r.msg_id.as_str())
        .collect();
    assert_eq!(reply_ids, vec!["rep-1", "rep-2", "optimistic-1"]);
    assert_eq!(ui.thread_scroll.selected_idx, Some(2));
    assert!(ui.pending_thread_reveal);

    // 3) ClipboardImageEncoded & HtmlAttachmentLoaded with stale seq/conv_key are ignored.
    app.clipboard_paste_seq = 10;
    app.html_fetch_seq = 20;
    ui.set_focus(Focus::Chat);
    let target_thread = ui.composer_target_thread;
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ClipboardImageEncoded {
            req_seq: 9,
            conv_key: Some(conv.to_string()),
            composer_target_thread: target_thread,
            width: 10,
            height: 10,
            file_name: "stale.png".into(),
            png_bytes: vec![1, 2, 3],
            preview: None,
        },
    );
    assert!(ui.composer.pending_images.is_empty());
    assert_eq!(ui.focus(), Focus::Chat);

    app.handle_ui_msg(
        &mut ui,
        UiMsg::HtmlAttachmentLoaded {
            req_seq: 20,
            conv_key: Some("chat:other".into()),
            title: "stale".into(),
            html: "<p>stale</p>".into(),
            kind: bitefoundry_html::ContentKind::Html,
        },
    );
    assert!(ui.overlays.is_empty());

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn message_deleted_clears_metadata_and_failed_send_restores_original_target() {
    let (mut app, mut ui) = make_test_app();
    let conv = "chan:t1:ch1";
    app.active_conv_key = Some(conv.to_string());
    app.active_target = Some(ConversationTarget::Channel {
        team_id: "t1".into(),
        channel_id: "ch1".into(),
    });
    let mut msg = make_test_msg(conv, "m1", "2026-11-20T00:00:00Z", "with att");
    msg.subject = Some("Sub".into());
    msg.attachments.push(AttachmentInfo {
        id: "a1".into(),
        content_type: "application/pdf".into(),
        content_url: None,
        name: Some("doc.pdf".into()),
        content_json: None,
        thumbnail_url: None,
    });
    app.messages = vec![msg];

    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageDeleted {
            conv_key: conv.to_string(),
            msg_id: "m1".into(),
            parent_msg_id: None,
        },
    );
    assert!(app.messages[0].deleted);
    assert!(app.messages[0].attachments.is_empty());
    assert!(app.messages[0].reactions.is_empty());
    assert!(app.messages[0].mentions.is_empty());
    assert!(app.messages[0].subject.is_none());
    assert!(
        super::super::overlays::attachments::rows_from(&app.messages, MessageScope::Chat)
            .is_empty()
    );

    // Failed thread reply while user is now in main composer must NOT clobber main composer;
    // it must restore into the thread draft key instead.
    ui.composer_target_thread = false;
    ui.composer.load_text("");
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MessageSendFailed {
            conv_key: conv.to_string(),
            parent_msg_id: Some("root-9".into()),
            optimistic_id: None,
            text: "caption for failed thread image".into(),
        },
    );
    assert!(ui.composer.text().is_empty());
    assert_eq!(
        app.drafts.get(&format!("{conv}#thread:root-9")).map(String::as_str),
        Some("caption for failed thread image")
    );

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn bootstrap_emits_before_peer_presence_and_delivers_peer_presences_async() {
    use axum::{routing::{get, post}, Json, Router};
    use tokio::sync::Notify;

    let release_presence = Arc::new(Notify::new());
    let rel_clone = Arc::clone(&release_presence);

    let router = Router::new()
        .route(
            "/v1.0/me",
            get(|| async {
                Json(serde_json::json!({
                    "id": "me-1",
                    "displayName": "Me",
                    "mail": "me@example.com"
                }))
            }),
        )
        .route(
            "/v1.0/me/presence",
            get(|| async {
                Json(serde_json::json!({
                    "id": "me-1",
                    "availability": "Available",
                    "activity": "Available"
                }))
            }),
        )
        .route(
            "/v1.0/me/chats",
            get(|| async {
                Json(serde_json::json!({
                    "value": [{
                        "id": "c-dm",
                        "chatType": "oneOnOne",
                        "members": [
                            { "userId": "me-1", "displayName": "Me" },
                            { "userId": "peer-1", "displayName": "Alice" }
                        ]
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async { Json(serde_json::json!({ "value": [] })) }),
        )
        .route(
            "/v1.0/communications/getPresencesByUserId",
            post(move || {
                let release = Arc::clone(&rel_clone);
                async move {
                    release.notified().await;
                    Json(serde_json::json!({
                        "value": [{
                            "id": "peer-1",
                            "availability": "Busy",
                            "activity": "InACall"
                        }]
                    }))
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..Default::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("me-1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone())
        .unwrap()
        .with_my_user_id("me-1");
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

    let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);
    assert_eq!(app.my_user_id, "me-1");
    let mut ui = UiState::new();
    app.spawn_teams_bootstrap(None);

    // BootstrapDone must arrive while getPresencesByUserId is still blocked on Notify.
    let first_msg = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("BootstrapDone must not block behind peer presence RTT")
        .unwrap();
    assert!(matches!(first_msg, UiMsg::BootstrapDone { .. }));
    app.handle_ui_msg(&mut ui, first_msg);
    assert_eq!(app.conversations.len(), 1);
    assert!(!app.presences.contains_key("peer-1"));

    // Releasing peer presence delivers PeerPresencesLoaded asynchronously (ignoring any
    // MembersLoaded/MessagesLoaded emitted by auto-selecting the first conversation).
    release_presence.notify_one();
    let peer_msg = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let msg = ui_rx.recv().await.unwrap();
            if matches!(msg, UiMsg::PeerPresencesLoaded(_)) {
                break msg;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
    })
    .await
    .expect("PeerPresencesLoaded should arrive once presence finishes");
    app.handle_ui_msg(&mut ui, peer_msg);
    assert_eq!(
        app.presences.get("peer-1").map(|p| p.availability.as_str()),
        Some("Busy")
    );

    server.abort();
}

#[tokio::test]
async fn select_conversation_fetches_single_50_page_and_interactive_prefetch_warms_uncached_neighbors() {
    use axum::{routing::{get, post}, Json, Router};
    use std::sync::atomic::AtomicUsize;

    let c1_msg_hits = Arc::new(AtomicUsize::new(0));
    let batch_hits = Arc::new(AtomicUsize::new(0));
    let batch_urls = Arc::new(Mutex::new(Vec::<String>::new()));

    let c1_hits_cl = Arc::clone(&c1_msg_hits);
    let batch_hits_cl = Arc::clone(&batch_hits);
    let batch_urls_cl = Arc::clone(&batch_urls);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}/v1.0");
    let c1_next = format!("{base}/chats/c1/messages?page=2");

    let router = Router::new()
        .route(
            "/v1.0/chats/c1/messages",
            get(move || {
                let hits = Arc::clone(&c1_hits_cl);
                let next = c1_next.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let vals: Vec<serde_json::Value> = (0..50)
                        .map(|i| {
                            serde_json::json!({
                                "id": format!("c1-m{i:02}"),
                                "messageType": "message",
                                "createdDateTime": format!("2026-11-20T01:{:02}:00Z", i),
                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                "body": { "contentType": "text", "content": format!("msg {i}") }
                            })
                        })
                        .collect();
                    Json(serde_json::json!({ "value": vals, "@odata.nextLink": next }))
                }
            }),
        )
        .route(
            "/v1.0/chats/{id}/members",
            get(|| async { Json(serde_json::json!({ "value": [] })) }),
        )
        .route(
            "/v1.0/chats/{id}/markChatReadForUser",
            post(|| async { axum::http::StatusCode::NO_CONTENT }),
        )
        .route(
            "/v1.0/$batch",
            post(move |Json(payload): Json<serde_json::Value>| {
                let hits = Arc::clone(&batch_hits_cl);
                let urls = Arc::clone(&batch_urls_cl);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                    let mut responses = Vec::new();
                    for r in reqs {
                        let id = r["id"].as_str().unwrap_or("").to_string();
                        let url = r["url"].as_str().unwrap_or("").to_string();
                        urls.lock().unwrap().push(url.clone());
                        let chat_id = url
                            .strip_prefix("/chats/")
                            .and_then(|s| s.split('/').next())
                            .unwrap_or("unknown");
                        responses.push(serde_json::json!({
                            "id": id,
                            "status": 200,
                            "body": {
                                "value": [{
                                    "id": format!("{chat_id}-warm-1"),
                                    "messageType": "message",
                                    "createdDateTime": "2026-11-20T02:00:00Z",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "text", "content": "prefetched" }
                                }]
                            }
                        }));
                    }
                    Json(serde_json::json!({ "responses": responses }))
                }
            }),
        );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig {
        graph_base_url: base,
        ..Default::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("me-1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone())
        .unwrap()
        .with_my_user_id("me-1");
    let storage = Storage::open_in_memory().unwrap();
    let mk_chat = |id: &str, title: &str, ts: &str| ConversationSummary {
        conv_key: format!("chat:{id}"),
        target: ConversationTarget::Chat {
            chat_id: id.into(),
        },
        kind: ConversationKind::OneOnOne,
        team_id: None,
        team_name: None,
        title: title.into(),
        topic: None,
        unread_count: 0,
        last_read_at: None,
        last_msg_id: None,
        last_msg_author: None,
        last_msg_preview: None,
        last_updated_at: Some(ts.into()),
        pinned: false,
        peer_user_id: Some(format!("peer-{id}")),
        web_url: None,
    };
    storage
        .upsert_conversations(&[
            mk_chat("c0", "Zero", "2026-11-20T05:00:00Z"),
            mk_chat("c1", "One", "2026-11-20T04:00:00Z"),
            mk_chat("c2", "Two", "2026-11-20T03:00:00Z"),
            mk_chat("c3", "Three", "2026-11-20T02:00:00Z"),
        ])
        .unwrap();
    // Pre-cache c0 and c2 so auto-select renders c0 immediately and prefetch skips cached c2.
    storage
        .upsert_messages(&[
            make_test_msg("chat:c0", "c0-m1", "2026-11-20T05:00:00Z", "cached c0"),
            make_test_msg("chat:c2", "c2-m1", "2026-11-20T03:00:00Z", "cached c2"),
        ])
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

    let mut app = AppState::new(cfg, auth, graph, storage.clone(), sync_ctrl, ui_tx);
    let mut ui = UiState::new();
    app.load_cached_teams();

    // 1) Initial cached auto-selection renders c0 immediately without prefetch.
    app.select_initial_cached_conversation(&mut ui);
    assert_eq!(app.active_conv_key.as_deref(), Some("chat:c0"));
    assert_eq!(app.messages.len(), 1);
    assert!(!app.loading_messages);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        batch_hits.load(Ordering::SeqCst),
        0,
        "startup auto-selection must not trigger speculative prefetch"
    );

    // 2) Interactive switch to cold c1 fetches exactly 1 Graph page ($top=50) and saves cursor,
    //    while prefetching uncached neighbor c3 (skipping already-cached c0 and c2).
    app.select_conversation(
        &mut ui,
        ConversationTarget::Chat {
            chat_id: "c1".into(),
        },
    );
    let loaded = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let msg = ui_rx.recv().await.unwrap();
            if matches!(&msg, UiMsg::MessagesLoaded { conv_key, .. } if conv_key == "chat:c1") {
                break msg;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
    })
    .await
    .expect("MessagesLoaded for chat:c1 should arrive");
    app.handle_ui_msg(&mut ui, loaded);
    assert_eq!(app.messages.len(), 50);
    assert_eq!(
        c1_msg_hits.load(Ordering::SeqCst),
        1,
        "cold conversation open must issue 1 single $top=50 page, not 2 sequential pages"
    );
    let cursor = storage
        .get_sync_cursor(&SyncCursor::history_scope("chat:c1"))
        .unwrap()
        .expect("initial history cursor must be saved on cold open");
    assert!(cursor.delta_link.as_deref().is_some_and(|l| l.contains("page=2")));

    // Wait briefly for background neighbor prefetch to populate c3 in SQLite.
    let mut warmed = false;
    for _ in 0..40 {
        if !storage.list_root_messages("chat:c3", 10, None).unwrap().is_empty() {
            warmed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(warmed, "uncached adjacent chat c3 should be warmed in SQLite");
    assert_eq!(batch_hits.load(Ordering::SeqCst), 1);
    let fetched_urls = batch_urls.lock().unwrap().clone();
    assert_eq!(fetched_urls.len(), 1, "already-cached neighbors c0/c2 must not be fetched");
    assert!(fetched_urls[0].contains("/chats/c3/messages"));

    // 3) Switching to c3 now shows cached messages immediately with loading_messages == false,
    //    and respects the 3s prefetch cooldown (still 1 batch hit total).
    app.select_conversation(
        &mut ui,
        ConversationTarget::Chat {
            chat_id: "c3".into(),
        },
    );
    assert!(!app.loading_messages);
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].msg_id, "c3-warm-1");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(batch_hits.load(Ordering::SeqCst), 1);

    server.abort();
}

fn mock_graph_auth(cfg: &AppConfig) -> AuthManager {
    let tok = crate::auth::StoredTokens {
        access_token: "tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("me-1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    AuthManager::from_tokens(cfg.clone(), tok)
}

/// An app wired to a mock Graph at base_url, keeping the UiMsg receiver for the test.
fn make_mock_graph_app(
    base_url: &str,
) -> (AppState, UiState, mpsc::UnboundedReceiver<UiMsg>) {
    let cfg = AppConfig {
        graph_base_url: base_url.to_string(),
        ..Default::default()
    };
    let auth = mock_graph_auth(&cfg);
    let graph = GraphClient::new(cfg.clone(), auth.clone())
        .unwrap()
        .with_my_user_id("me-1");
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) =
        tokio::sync::watch::channel(crate::sync::FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let ui = UiState::from_config(&cfg);
    (app, ui, ui_rx)
}

// Track 5: delta sync leaves channel roots at reply_count 0, so a loaded thread must carry
// its count back to the root the channel view draws the reply pill from.
#[tokio::test]
async fn thread_loaded_backfills_reply_count_into_channel_root() {
    let (mut app, mut ui) = make_test_app();
    let conv = "chan:t1:ch1";
    app.active_conv_key = Some(conv.to_string());
    app.active_target = Some(ConversationTarget::Channel {
        team_id: "t1".into(),
        channel_id: "ch1".into(),
    });
    let root = make_test_msg(conv, "root-1", "2026-11-20T00:00:00Z", "root");
    let other = make_test_msg(conv, "root-2", "2026-11-20T00:10:00Z", "other");
    app.messages = vec![root.clone(), other];
    let reply = |id: &str, minute: u32| {
        let mut r = make_test_msg(conv, id, &format!("2026-11-20T00:0{minute}:00Z"), id);
        r.parent_msg_id = Some("root-1".into());
        r
    };

    // 1) Graph's count on the fetched root covers replies beyond the loaded window.
    app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), None);
    let mut fetched_root = root.clone();
    fetched_root.reply_count = 5;
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ThreadLoaded {
            req_seq: app.thread_req_seq,
            conv_key: conv.to_string(),
            root_msg_id: "root-1".into(),
            thread: ThreadView {
                conv_key: conv.to_string(),
                root: fetched_root,
                replies: vec![reply("rep-1", 1), reply("rep-2", 2)],
            },
        },
    );
    assert_eq!(app.messages[0].reply_count, 5);
    assert_eq!(app.messages[1].reply_count, 0, "other roots keep their count");

    // 2) A root still at 0 (delta-sourced) takes the number of live replies loaded.
    app.messages[0].reply_count = 0;
    app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), None);
    let mut deleted = reply("rep-3", 3);
    deleted.deleted = true;
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ThreadLoaded {
            req_seq: app.thread_req_seq,
            conv_key: conv.to_string(),
            root_msg_id: "root-1".into(),
            thread: ThreadView {
                conv_key: conv.to_string(),
                root: root.clone(),
                replies: vec![reply("rep-1", 1), reply("rep-2", 2), deleted],
            },
        },
    );
    assert_eq!(app.messages[0].reply_count, 2);
    assert_eq!(app.messages[1].reply_count, 0);

    // 3) A stale load for an older request leaves the count alone.
    app.messages[0].reply_count = 0;
    app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), None);
    app.handle_ui_msg(
        &mut ui,
        UiMsg::ThreadLoaded {
            req_seq: app.thread_req_seq.wrapping_sub(1),
            conv_key: conv.to_string(),
            root_msg_id: "root-1".into(),
            thread: ThreadView {
                conv_key: conv.to_string(),
                root: root.clone(),
                replies: vec![reply("rep-1", 1)],
            },
        },
    );
    assert_eq!(app.messages[0].reply_count, 0);

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

// Track 5: a channel cached only from delta sync shows no reply pills; the first open this
// session refreshes one $expand=replies page in the background, and later opens do not.
#[tokio::test]
async fn channel_open_hydrates_delta_cached_reply_counts_once_per_session() {
    use axum::{routing::get, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let page_hits = Arc::new(AtomicUsize::new(0));
    let page_hits_c = Arc::clone(&page_hits);
    let router = Router::new().route(
        "/v1.0/teams/t1/channels/ch1/messages",
        get(move || {
            let hits = Arc::clone(&page_hits_c);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let reply = |id: &str, ts: &str| {
                    serde_json::json!({
                        "id": id,
                        "messageType": "message",
                        "createdDateTime": ts,
                        "from": { "user": { "id": "u2", "displayName": "Bob" } },
                        "body": { "contentType": "text", "content": id }
                    })
                };
                Json(serde_json::json!({
                    "value": [{
                        "id": "root-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-20T00:00:00Z",
                        "from": { "user": { "id": "u1", "displayName": "Alice" } },
                        "body": { "contentType": "text", "content": "root" },
                        "replies": [
                            reply("rep-1", "2026-11-20T00:01:00Z"),
                            reply("rep-2", "2026-11-20T00:02:00Z")
                        ]
                    }]
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let (mut app, mut ui, mut ui_rx) = make_mock_graph_app(&format!("http://{addr}/v1.0"));
    let conv = "chan:t1:ch1";
    // The delta-synced root: no replies cached, so its count is 0.
    app.storage
        .upsert_messages(&[make_test_msg(conv, "root-1", "2026-11-20T00:00:00Z", "root")])
        .unwrap();
    let channel = ConversationTarget::Channel {
        team_id: "t1".into(),
        channel_id: "ch1".into(),
    };

    app.select_conversation(&mut ui, channel.clone());
    assert_eq!(app.messages.len(), 1);
    assert_eq!(app.messages[0].reply_count, 0);
    assert!(!app.loading_messages, "cached roots render without a loading state");
    let loaded = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = ui_rx.recv().await.unwrap();
            if matches!(&msg, UiMsg::MessagesLoaded { conv_key, .. } if conv_key == conv) {
                break msg;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
    })
    .await
    .expect("the reply hydration page should load");
    app.handle_ui_msg(&mut ui, loaded);
    assert_eq!(page_hits.load(Ordering::SeqCst), 1);
    let root = app
        .messages
        .iter()
        .find(|m| m.msg_id == "root-1")
        .expect("root stays in the channel view");
    assert_eq!(root.reply_count, 2);
    assert_eq!(
        app.storage.get_message(conv, "root-1").unwrap().unwrap().reply_count,
        2
    );

    // Reopening the hydrated channel in the same session does not fetch the page again.
    app.select_conversation(
        &mut ui,
        ConversationTarget::Chat {
            chat_id: "c-other".into(),
        },
    );
    app.select_conversation(&mut ui, channel);
    assert_eq!(app.messages[0].reply_count, 2);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(page_hits.load(Ordering::SeqCst), 1);

    server.abort();
}

// Track 5: the stored CHATS_LIST_SCOPE cursor decides paging, not the cached flag, and an
// expired $skiptoken is dropped instead of being retried forever.
#[tokio::test]
async fn load_older_chats_recovers_from_stale_eof_and_expired_skiptoken() {
    use axum::{extract::RawQuery, http::StatusCode, routing::get, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dead_hits = Arc::new(AtomicUsize::new(0));
    let list_hits = Arc::new(AtomicUsize::new(0));
    let dead_hits_c = Arc::clone(&dead_hits);
    let list_hits_c = Arc::clone(&list_hits);
    let router = Router::new().route(
        "/v1.0/me/chats",
        get(move |RawQuery(q): RawQuery| {
            let dead_hits = Arc::clone(&dead_hits_c);
            let list_hits = Arc::clone(&list_hits_c);
            async move {
                if q.unwrap_or_default().contains("skiptoken=dead") {
                    dead_hits.fetch_add(1, Ordering::SeqCst);
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": { "code": "BadRequest", "message": "Invalid skip token." }
                        })),
                    );
                }
                list_hits.fetch_add(1, Ordering::SeqCst);
                // A full page of 50 chats with no nextLink.
                let chats: Vec<serde_json::Value> = (0..50)
                    .map(|i| {
                        serde_json::json!({
                            "id": format!("c-{i:02}"),
                            "chatType": "group",
                            "topic": format!("Chat {i:02}"),
                            "members": []
                        })
                    })
                    .collect();
                (StatusCode::OK, Json(serde_json::json!({ "value": chats })))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://{addr}/v1.0");
    let (mut app, mut ui, mut ui_rx) = make_mock_graph_app(&base);
    let chats_cursor =
        |app: &AppState| app.storage.get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE).unwrap();
    let save_cursor = |app: &AppState, delta_link: Option<String>, oldest: Option<&str>| {
        app.storage
            .save_sync_cursor(&SyncCursor {
                scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
                delta_link,
                oldest_msg_ts: oldest.map(str::to_string),
                synced_at: Utc::now().to_rfc3339(),
            })
            .unwrap();
    };
    async fn next_older_chats_msg(ui_rx: &mut mpsc::UnboundedReceiver<UiMsg>) -> UiMsg {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = ui_rx.recv().await.unwrap();
                if matches!(&msg, UiMsg::OlderChatsLoaded { .. } | UiMsg::OlderChatsFailed) {
                    break msg;
                }
            }
        })
        .await
        .expect("older chats load should finish")
    }
    let loaded_summary = |msg: &UiMsg| match msg {
        UiMsg::OlderChatsLoaded {
            conversations,
            reached_eof,
        } => (conversations.len(), *reached_eof),
        _ => panic!("expected OlderChatsLoaded"),
    };

    // 1) A stored EOF ends paging without a request.
    save_cursor(&app, None, Some(SyncCursor::HISTORY_EOF));
    app.load_older_chats();
    assert!(app.chats_oldest_reached);
    assert!(!app.loading_older_chats);
    assert_eq!(list_hits.load(Ordering::SeqCst), 0);

    // 2) A fresh page-1 sync replaced that EOF with a nextLink (the stored EOF timestamp
    //    stays beside it): the cached flag must not block the link. The link has expired
    //    (400), so the cursor is dropped and the count-based listing loads chats instead.
    let dead_link = format!("{base}/me/chats?$skiptoken=dead");
    save_cursor(&app, Some(dead_link.clone()), None);
    app.load_older_chats();
    assert!(app.loading_older_chats);
    let loaded = next_older_chats_msg(&mut ui_rx).await;
    assert_eq!(loaded_summary(&loaded), (50, false));
    assert_eq!(dead_hits.load(Ordering::SeqCst), 1);
    assert_eq!(list_hits.load(Ordering::SeqCst), 1);
    // Clearing only the link would have revived the old EOF and stopped paging for good.
    let cursor = chats_cursor(&app);
    assert!(
        cursor.as_ref().is_none_or(|c| !c.is_history_eof()
            && c.delta_link.as_deref() != Some(dead_link.as_str())),
        "the dead cursor must be gone, not turned back into EOF: {cursor:?}"
    );
    app.handle_ui_msg(&mut ui, loaded);
    assert!(!app.loading_older_chats);
    assert!(!app.chats_oldest_reached);
    assert_eq!(app.conversations.len(), 50);

    // 3) Paging continues without the dead link: no second request for it.
    app.load_older_chats();
    assert!(app.loading_older_chats);
    let again = next_older_chats_msg(&mut ui_rx).await;
    assert_eq!(loaded_summary(&again), (50, true));
    assert_eq!(dead_hits.load(Ordering::SeqCst), 1);
    assert_eq!(list_hits.load(Ordering::SeqCst), 2);
    assert!(chats_cursor(&app).is_some_and(|c| c.is_history_eof()));
    app.handle_ui_msg(&mut ui, again);
    assert!(app.chats_oldest_reached);

    // 4) A cursor whose link was replaced meanwhile is not deleted by a stale expiry.
    let fresh = format!("{base}/me/chats?$skiptoken=fresh");
    save_cursor(&app, Some(fresh.clone()), None);
    assert!(!app
        .storage
        .delete_sync_cursor_with_link(SyncCursor::CHATS_LIST_SCOPE, &dead_link)
        .unwrap());
    assert_eq!(
        chats_cursor(&app).and_then(|c| c.delta_link).as_deref(),
        Some(fresh.as_str())
    );

    server.abort();
}
