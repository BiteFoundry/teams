use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::json;

use crate::{
    auth::{AuthManager, StoredTokens},
    config,
    graph::GraphClient,
    img_preview,
    models::{
        AttachmentInfo, ConversationKind, ConversationSummary, ConversationTarget, MemberInfo,
        MentionInfo, MessageItem, ThreadCoverage, ThreadView,
    },
    resolver,
    storage::Storage,
};

use super::{
    args::has_candidate_hyphen_value,
    format::{error_code_for_message, format_message_line, DetailedError},
    history::{parse_mentions_cli, ReadMessagesResult},
    messages::delete_gate,
    preview::{resolve_image_protocol_preference, select_teams_attachment},
    runtime::{
        extract_user_id_from_jwt, is_cursor_fresh, parse_cli_cache_secs,
        resolve_storage_db_path_fast,
    },
    *,
};

#[test]
fn parses_default_tui_without_subcommand() {
    let cli = Cli::try_parse_from(["teams"]).unwrap();
    assert!(cli.command.is_none());
}

#[test]
fn parses_aliases_and_flags() {
    let cli = Cli::try_parse_from(["teams", "ls", "--limit", "200", "--refresh", "--json"])
        .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Chats {
            limit: 200,
            refresh: true,
            cached: false,
            json: true
        }
    ));

    // Default `teams read` limit is 30, supports `--before` and `--thread`
    let cli = Cli::try_parse_from(["teams", "read", "Engineering/General"]).unwrap();
    match cli.command.unwrap() {
        Command::Read {
            target,
            limit,
            before,
            thread,
            offline,
            cached,
            json,
            compact,
            full,
        } => {
            assert_eq!(target, "Engineering/General");
            assert_eq!(limit, 30);
            assert!(before.is_none());
            assert!(thread.is_none());
            assert!(!offline);
            assert!(!cached);
            assert!(!json);
            assert!(!compact);
            assert!(!full);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "teams",
        "read",
        "chat:1",
        "--before",
        "2026-11-01",
        "--limit",
        "100",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Read {
            target,
            limit,
            before,
            thread,
            ..
        } => {
            assert_eq!(target, "chat:1");
            assert_eq!(limit, 100);
            assert_eq!(before.as_deref(), Some("2026-11-01"));
            assert!(thread.is_none());
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "teams",
        "read",
        "chat:1",
        "--thread",
        "root-1",
        "--limit",
        "100",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Read {
            target,
            limit,
            before,
            thread,
            ..
        } => {
            assert_eq!(target, "chat:1");
            assert_eq!(limit, 100);
            assert!(before.is_none());
            assert_eq!(thread.as_deref(), Some("root-1"));
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let conflict = Cli::try_parse_from([
        "teams",
        "read",
        "chat:1",
        "--before",
        "2026-11-01",
        "--thread",
        "root-1",
    ])
    .unwrap_err();
    assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);

    // `teams sync --chats --depth --target`
    let cli = Cli::try_parse_from(["teams", "sync"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Sync {
            target: None,
            chats: 100,
            depth: 50,
            json: false
        }
    ));
    let cli = Cli::try_parse_from([
        "teams", "sync", "--target", "Engineering/General", "--chats", "250", "--depth", "500", "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Sync {
            target,
            chats,
            depth,
            json,
        } => {
            assert_eq!(target.as_deref(), Some("Engineering/General"));
            assert_eq!(chats, 250);
            assert_eq!(depth, 500);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "teams",
        "msg",
        "Engineering/General",
        "Hello",
        "--subject",
        "Notice",
        "--mention",
        "u1:Alice",
        "--important",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Send {
            target,
            markdown,
            subject,
            mention,
            important,
            json,
            full: false,
        } => {
            assert_eq!(target, "Engineering/General");
            assert_eq!(markdown, "Hello");
            assert_eq!(subject.as_deref(), Some("Notice"));
            assert_eq!(mention, vec!["u1:Alice"]);
            assert!(important);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "teams",
        "file",
        "Alice",
        "C:\\tmp\\shot.png",
        "--caption",
        "Screenshot",
        "--thread",
        "root1",
        "--inline",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::SendFile {
            target,
            path,
            caption,
            thread,
            inline,
            json,
            full: false,
        } => {
            assert_eq!(target, "Alice");
            assert_eq!(path, PathBuf::from("C:\\tmp\\shot.png"));
            assert_eq!(caption.as_deref(), Some("Screenshot"));
            assert_eq!(thread.as_deref(), Some("root1"));
            assert!(inline);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn parses_compact_reads_and_exclusive_edit_modes() {
    let edit = Cli::try_parse_from([
        "teams", "edit", "chan:t:c", "m1", "--find", "text", "--replace=",
    ])
    .unwrap();
    assert!(matches!(
        edit.command,
        Some(Command::Edit { new_markdown: None, find: Some(_), replace: Some(ref value), .. })
            if value.is_empty()
    ));
    assert!(Cli::try_parse_from(["teams", "edit", "chat:c", "m1", "Markdown"]).is_ok());
    assert!(Cli::try_parse_from([
        "teams", "edit", "chan:t:c", "m1", "--subject", "New title",
    ])
    .is_ok());
    assert!(Cli::try_parse_from([
        "teams", "edit", "chan:t:c", "m1", "--thread", "root", "--subject", "New title",
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "teams", "update-chat", "chat:c", "--topic", "New topic", "--json",
    ])
    .is_ok());
    assert!(Cli::try_parse_from([
        "teams", "edit", "chat:c", "m1", "Markdown", "--find", "old", "--replace", "new",
    ])
    .is_err());
    assert!(
        Cli::try_parse_from(["teams", "edit", "chat:c", "m1", "--find", "old"]).is_err()
    );
    assert!(Cli::try_parse_from(["teams", "read", "chat:c", "--compact"]).is_err());
    assert!(Cli::try_parse_from([
        "teams", "thread", "chan:t:c", "m1", "--json", "--compact",
    ])
    .is_ok());
    assert!(Cli::try_parse_from(["teams", "read", "chat:c", "--json", "--full"]).is_ok());
    assert!(Cli::try_parse_from([
        "teams", "read", "chat:c", "--json", "--compact", "--full",
    ])
    .is_err());
    assert!(Cli::try_parse_from(["teams", "read", "chat:c", "--full"]).is_err());
    assert!(Cli::try_parse_from([
        "teams", "thread", "chan:t:c", "m1", "--json", "--full",
    ])
    .is_ok());
    assert!(Cli::try_parse_from(["teams", "send", "chat:c", "hi", "--full"]).is_err());
    assert!(Cli::try_parse_from(["teams", "send", "chat:c", "hi", "--json", "--full"]).is_ok());
    assert!(Cli::try_parse_from(["teams", "reply", "chat:c", "m1", "hi", "--full"]).is_err());
    assert!(Cli::try_parse_from([
        "teams", "reply", "chat:c", "m1", "hi", "--json", "--full",
    ])
    .is_ok());
    assert!(Cli::try_parse_from([
        "teams", "send-file", "chat:c", "a.txt", "--full",
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "teams", "send-file", "chat:c", "a.txt", "--json", "--full",
    ])
    .is_ok());
    assert!(Cli::try_parse_from(["teams", "users", "Alice", "--limit", "5", "--json"]).is_ok());
}

#[test]
fn parses_serve_and_mcp_aliases() {
    let cli = Cli::try_parse_from([
        "teams",
        "api",
        "--bind",
        "0.0.0.0",
        "--port",
        "9000",
        "--token",
        "sec",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Serve {
            bind,
            port,
            token,
            allow_keyless,
            cors_origin,
        } => {
            assert_eq!(bind, "0.0.0.0");
            assert_eq!(port, 9000);
            assert_eq!(token.as_deref(), Some("sec"));
            assert!(!allow_keyless);
            assert!(cors_origin.is_empty());
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "teams",
        "serve",
        "--allow-keyless",
        "--cors-origin",
        "http://localhost:3000",
        "--cors-origin",
        "https://app.example",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Serve {
            token,
            allow_keyless,
            cors_origin,
            ..
        } => {
            assert!(token.is_none());
            assert!(allow_keyless);
            assert_eq!(cors_origin, ["http://localhost:3000", "https://app.example"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    assert!(
        Cli::try_parse_from(["teams", "serve", "--allow-keyless", "--token", "sec"]).is_err(),
        "--allow-keyless and --token are mutually exclusive"
    );

    let cli = Cli::try_parse_from(["teams", "mcp-server"]).unwrap();
    assert!(matches!(cli.command.unwrap(), Command::Mcp));
}

#[test]
fn parses_update_and_preview_commands() {
    let cli = Cli::try_parse_from([
        "teams",
        "update",
        "--check",
        "--force",
        "--repo",
        "org/repo",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Update {
            check,
            force,
            repo,
            json,
        } => {
            assert!(check);
            assert!(force);
            assert_eq!(repo.as_deref(), Some("org/repo"));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = Cli::try_parse_from(["teams", "preview-file", "shot.png", "--cols", "80"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::PreviewFile {
            cols: Some(80),
            protocol: None,
            ..
        }
    ));

    let cli = Cli::try_parse_from([
        "teams",
        "preview",
        "chat:1",
        "msg-1",
        "--protocol",
        "sixel",
        "--cols",
        "100",
        "--rows",
        "30",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Preview {
            target,
            msg_id,
            cols,
            rows,
            protocol,
            ..
        } => {
            assert_eq!(target.as_deref(), Some("chat:1"));
            assert_eq!(msg_id.as_deref(), Some("msg-1"));
            assert_eq!(cols, Some(100));
            assert_eq!(rows, Some(30));
            assert_eq!(protocol.as_deref(), Some("sixel"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "teams",
        "preview-file",
        "shot.png",
        "--protocol",
        "iterm2",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::PreviewFile { path, protocol, .. } => {
            assert_eq!(path, Path::new("shot.png"));
            assert_eq!(protocol.as_deref(), Some("iterm2"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    let cfg = AppConfig {
        image_protocol: config::ImageProtocolPreference::Kitty,
        ..Default::default()
    };
    assert_eq!(
        resolve_image_protocol_preference(&cfg, None).unwrap(),
        config::ImageProtocolPreference::Kitty
    );
    assert_eq!(
        resolve_image_protocol_preference(&cfg, Some("sixel")).unwrap(),
        config::ImageProtocolPreference::Sixel
    );
    assert_eq!(
        resolve_image_protocol_preference(&cfg, Some("iterm2")).unwrap(),
        config::ImageProtocolPreference::Iterm2
    );
    let invalid_err = resolve_image_protocol_preference(&cfg, Some("invalid-proto"))
        .unwrap_err()
        .to_string();
    assert!(
        invalid_err.contains("auto, kitty, iterm2, sixel, halfblock"),
        "unexpected error message: {invalid_err}"
    );
}

#[test]
fn parses_mentions_and_rejects_malformed() {
    let parsed =
        parse_mentions_cli(&["user-1:Alice".to_string(), " user-2 : Bob Kim ".to_string()])
            .unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].id, 0);
    assert_eq!(parsed[0].user_id, "user-1");
    assert_eq!(parsed[0].display_name, "Alice");
    assert_eq!(parsed[1].id, 1);
    assert_eq!(parsed[1].user_id, "user-2");
    assert_eq!(parsed[1].display_name, "Bob Kim");

    for bad in ["nocolon", ":noname", "nouid:", "  :  "] {
        assert!(parse_mention_spec(bad, 0).is_err(), "{bad}");
    }
}

#[test]
fn normalizes_reactions_and_presence() {
    assert_eq!(normalize_reaction("LIKE").unwrap(), "like");
    assert_eq!(normalize_reaction("Heart").unwrap(), "heart");
    assert_eq!(normalize_reaction("👍").unwrap(), "👍");
    assert!(normalize_reaction("  ").is_err());
    assert!(normalize_reaction("bogus").is_err());

    assert_eq!(
        normalize_presence_availability("donotdisturb").unwrap(),
        "DoNotDisturb"
    );
    assert_eq!(normalize_presence_availability("brb").unwrap(), "BeRightBack");
    assert_eq!(normalize_presence_availability("reset").unwrap(), "Reset");
    assert!(normalize_presence_availability("unknown").is_err());
}

#[test]
fn delete_gate_requires_yes_or_confirmation() {
    delete_gate(true, false, None).unwrap();
    delete_gate(true, true, None).unwrap();
    assert!(delete_gate(false, true, Some("yes")).is_err());
    delete_gate(false, false, Some("YES")).unwrap();
    assert!(delete_gate(false, false, Some("no")).is_err());
    assert!(delete_gate(false, false, None).is_err());
}

#[test]
fn resolves_before_cursor_and_offline_pagination() {
    let storage = Storage::open_in_memory().unwrap();
    let conv = ConversationSummary {
        conv_key: "chat:19:hist".to_string(),
        target: ConversationTarget::Chat {
            chat_id: "19:hist".to_string(),
        },
        kind: ConversationKind::OneOnOne,
        team_id: None,
        team_name: None,
        title: "History Test".to_string(),
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
    };
    storage.upsert_conversations(std::slice::from_ref(&conv)).unwrap();

    let mk_msg = |id: &str, ts: &str| MessageItem {
        msg_key: format!("chat:19:hist|{id}"),
        conv_key: "chat:19:hist".to_string(),
        msg_id: id.to_string(),
        parent_msg_id: None,
        reply_to_id: None,
        reply_to_author: None,
        reply_to_preview: None,
        subject: None,
        author_id: Some("u1".to_string()),
        author_name: "Bob".to_string(),
        is_me: false,
        created_at: ts.to_string(),
        last_modified_at: None,
        deleted: false,
        importance: "normal".to_string(),
        message_type: "message".to_string(),
        body_html: format!("<p>{id}</p>"),
        body_plain: id.to_string(),
        attachments: vec![],
        mentions: vec![],
        reactions: vec![],
        reply_count: 0,
        web_url: None,
    };

    storage
        .upsert_messages(&[
            mk_msg("m1", "2026-10-30T10:00:00Z"),
            mk_msg("m2", "2026-10-31T12:00:00Z"),
            mk_msg("m3", "2026-11-01T09:00:00Z"),
            mk_msg("m4", "2026-11-02T09:00:00Z"),
        ])
        .unwrap();

    // Date-only `YYYY-MM-DD` normalizes to fixed-width UTC 9-digit comparison key
    let (ts, id) = resolve_before_cursor(&storage, &conv.conv_key, "2026-11-01").unwrap();
    assert_eq!(ts, "2026-11-01T00:00:00.000000000Z");
    assert!(id.is_none());

    // Cached msg_id resolves to its normalized timestamp key and msg_id
    let (ts, id) = resolve_before_cursor(&storage, &conv.conv_key, "m3").unwrap();
    assert_eq!(ts, "2026-11-01T09:00:00.000000000Z");
    assert_eq!(id.as_deref(), Some("m3"));

    // Invalid input returns an error
    assert!(resolve_before_cursor(&storage, &conv.conv_key, "not-a-timestamp").is_err());

    // Offline pagination before `2026-11-01` yields m1, m2 in chronological order
    let res = read_conversation_messages_offline(&storage, &conv, 1, Some("2026-11-01"))
        .unwrap();
    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0].msg_id, "m2");
    assert!(res.has_more);
    assert_eq!(res.next_before.as_deref(), Some("m2"));

    let res2 = read_conversation_messages_offline(&storage, &conv, 10, Some("m2")).unwrap();
    assert_eq!(res2.messages.len(), 1);
    assert_eq!(res2.messages[0].msg_id, "m1");
    assert!(!res2.has_more);
}

#[test]
fn pure_teams_top_aliases_and_env_help() {
    assert!(matches!(
        Cli::try_parse_from(["teams", "chats", "--top", "7"]).unwrap().command.unwrap(),
        Command::Chats { limit: 7, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "read", "c1", "--top", "7"]).unwrap().command.unwrap(),
        Command::Read { limit: 7, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "thread", "c1", "m1", "--top", "7"]).unwrap().command.unwrap(),
        Command::Thread { limit: 7, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
        Command::Search { limit: 7, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "sync", "--top", "7"]).unwrap().command.unwrap(),
        Command::Sync { depth: 7, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "sync", "--limit", "77"]).unwrap().command.unwrap(),
        Command::Sync { depth: 77, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "export", "c1", "--top", "7"]).unwrap().command.unwrap(),
        Command::Export { limit: 7, .. }
    ));

    let required_envs = [
        "TEAMS_TOKEN_FILE",
        "TEAMS_TOKEN_REFRESH_CMD",
        "TEAMS_ACCESS_TOKEN",
        "TEAMS_DISABLE_KEYRING",
        "TEAMS_IMAGE_PROTOCOL",
    ];
    let teams_help = <Cli as clap::CommandFactory>::command().render_help().to_string();
    for env_name in required_envs {
        assert!(teams_help.contains(env_name), "teams help missing {env_name}");
    }
    assert!(!teams_help.contains("TEAMS_PERSONA"));
    assert!(Cli::try_parse_from(["teams", "mail"]).is_err());
    assert!(Cli::try_parse_from(["teams", "notes"]).is_err());
}

#[test]
fn parses_cached_and_fast_flags() {
    assert!(matches!(
        Cli::try_parse_from(["teams", "chats", "--cached"]).unwrap().command.unwrap(),
        Command::Chats { cached: true, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "chats", "--fast"]).unwrap().command.unwrap(),
        Command::Chats { cached: true, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "teams", "--fast"]).unwrap().command.unwrap(),
        Command::Teams { cached: true, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "channels", "dev", "--cached"]).unwrap().command.unwrap(),
        Command::Channels { cached: true, .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "read", "c1", "--fast"]).unwrap().command.unwrap(),
        Command::Read { cached: true, .. }
    ));
}

#[test]
fn fast_help_version_and_cache_cursor_helpers_work() {
    assert!(try_fast_help_or_version_from(["teams", "--help"], Persona::Teams).is_some());
    assert!(try_fast_help_or_version_from(["teams", "-V"], Persona::Teams).is_some());
    assert!(try_fast_help_or_version_from(["teams", "read", "--help"], Persona::Teams).is_some());
    assert!(try_fast_help_or_version_from(["teams", "chats"], Persona::Teams).is_none());
    assert!(try_fast_help_or_version_from(["teams", "send", "a", "--", "--help"], Persona::Teams).is_none());

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"oid":"user-123","sub":"sub-1"}"#);
    let jwt = format!("Bearer header.{payload}.sig");
    assert_eq!(extract_user_id_from_jwt(&jwt).as_deref(), Some("user-123"));

    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let cred_path = tmp.path().join("credentials.json");
    std::fs::create_dir_all(data_dir.join("solo-user")).unwrap();
    std::fs::write(data_dir.join("solo-user").join("cache.db"), b"").unwrap();

    let cfg = AppConfig::default();
    assert_eq!(
        resolve_storage_db_path_fast(&cfg, None, &data_dir, &cred_path).unwrap(),
        data_dir.join("solo-user").join("cache.db")
    );

    std::fs::create_dir_all(data_dir.join("second-user")).unwrap();
    std::fs::write(data_dir.join("second-user").join("cache.db"), b"").unwrap();
    assert!(resolve_storage_db_path_fast(&cfg, None, &data_dir, &cred_path).is_none());

    std::fs::write(&cred_path, r#"{"user_id":"second-user"}"#).unwrap();
    assert_eq!(
        resolve_storage_db_path_fast(&cfg, None, &data_dir, &cred_path).unwrap(),
        data_dir.join("second-user").join("cache.db")
    );

    assert_eq!(
        resolve_storage_db_path_fast(&cfg, Some(&jwt), &data_dir, &cred_path).unwrap(),
        data_dir.join("user-123").join("cache.db")
    );

    assert_eq!(parse_cli_cache_secs(None), 0);
    assert_eq!(parse_cli_cache_secs(Some(" 60 ")), 60);
    assert_eq!(parse_cli_cache_secs(Some("bad")), 0);

    let storage = Storage::open_in_memory().unwrap();
    assert!(!is_cursor_fresh(&storage, &["cli:chats"], 60));
    record_cli_sync_cursor(&storage, "cli:chats");
    assert!(is_cursor_fresh(&storage, &["cli:chats"], 60));
    assert!(!is_cursor_fresh(&storage, &["cli:chats"], 0));
}

#[test]
fn parses_daemon_subcommands() {
    assert!(matches!(
        Cli::try_parse_from(["teams", "daemon", "start"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Start { json: false }
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "daemon", "start", "--json"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Start { json: true }
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "daemon", "stop"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Stop { json: false }
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "daemon", "status"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Status { json: false }
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "daemon", "restart"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Restart { json: false }
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["teams", "daemon", "run"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Run
        }
    ));
}

#[tokio::test]
async fn cached_inline_hosted_image_exposes_effective_attachment_for_cli_helpers() {
    use axum::{http::StatusCode, routing::get, Router};

    let storage = Storage::open_in_memory().unwrap();
    let conv = ConversationSummary {
        conv_key: "chat:c1".to_string(),
        target: ConversationTarget::Chat {
            chat_id: "c1".to_string(),
        },
        kind: ConversationKind::OneOnOne,
        team_id: None,
        team_name: None,
        title: "Inline Hosted".to_string(),
        topic: None,
        unread_count: 0,
        last_read_at: None,
        last_msg_id: Some("m1".to_string()),
        last_msg_author: Some("Alice".to_string()),
        last_msg_preview: Some("Screenshot".to_string()),
        last_updated_at: Some("2026-11-18T10:00:00Z".to_string()),
        pinned: false,
        peer_user_id: None,
        web_url: None,
    };
    storage.upsert_conversations(std::slice::from_ref(&conv)).unwrap();

    let graph_hosted_url =
        "https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value";
    let cached_msg = MessageItem {
        msg_key: "chat:c1|m1".to_string(),
        conv_key: "chat:c1".to_string(),
        msg_id: "m1".to_string(),
        parent_msg_id: None,
        reply_to_id: None,
        reply_to_author: None,
        reply_to_preview: None,
        subject: None,
        author_id: Some("u1".to_string()),
        author_name: "Alice".to_string(),
        is_me: false,
        created_at: "2026-11-18T10:00:00Z".to_string(),
        last_modified_at: None,
        deleted: false,
        importance: "normal".to_string(),
        message_type: "message".to_string(),
        body_html: format!(r#"<p>See screenshot</p><img src="{graph_hosted_url}">"#),
        body_plain: "See screenshot".to_string(),
        attachments: vec![],
        mentions: vec![],
        reactions: vec![],
        reply_count: 0,
        web_url: None,
    };
    storage.upsert_messages(std::slice::from_ref(&cached_msg)).unwrap();

    // 1. CLI read helper hydrates attachments for --json & renders [📎 ...] tag in text.
    let read_res = read_conversation_messages_offline(&storage, &conv, 10, None).unwrap();
    assert_eq!(read_res.messages.len(), 1);
    assert_eq!(read_res.messages[0].attachments.len(), 1);
    assert_eq!(read_res.messages[0].attachments[0].id, "hosted:1");
    assert_eq!(
        read_res.messages[0].attachments[0].content_url.as_deref(),
        Some(graph_hosted_url)
    );
    let raw_msg = storage.get_message("chat:c1", "m1").unwrap().unwrap();
    assert!(raw_msg.attachments.is_empty());
    let line = format_message_line(&raw_msg, "");
    assert!(line.contains("[📎 "), "expected attachment tag in line: {line}");

    // 2. CLI preview selector finds hosted:1 even on unhydrated cached MessageItem.
    for selector in [None, Some("0"), Some("hosted:1")] {
        let att = select_teams_attachment(&raw_msg, selector).unwrap();
        assert_eq!(att.id, "hosted:1");
        assert_eq!(att.content_url.as_deref(), Some(graph_hosted_url));
        assert!(img_preview::is_image_mime_or_name(
            &att.content_type,
            att.name.as_deref()
        ));
    }

    // 3. CLI download helper fetches hosted:1 from mock Graph server for cached message.
    let png_bytes = b"\x89PNG\r\n\x1a\nfake-png-bytes".to_vec();
    let served_bytes = png_bytes.clone();
    let mock_app = Router::new().route(
        "/v1.0/chats/c1/messages/m1/hostedContents/1/$value",
        get(move || {
            let bytes = served_bytes.clone();
            async move { (StatusCode::OK, bytes) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mock_server = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let local_hosted_url = format!(
        "http://{addr}/v1.0/chats/c1/messages/m1/hostedContents/1/$value"
    );
    let mut local_msg = cached_msg;
    local_msg.msg_key = "chat:c1|m2".to_string();
    local_msg.msg_id = "m2".to_string();
    local_msg.body_html = format!(r#"<img src="{local_hosted_url}">"#);
    storage.upsert_messages(&[local_msg]).unwrap();

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tokens = StoredTokens {
        access_token: "tok".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("me-1".to_string()),
        user_display_name: None,
        user_email: None,
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg, auth).unwrap();
    let dl_dir = tempfile::tempdir().unwrap();

    let downloaded =
        download_message_attachment(&graph, &storage, &conv, "m2", 0, None, dl_dir.path())
            .await
            .unwrap();
    assert_eq!(downloaded.bytes_written, png_bytes.len() as u64);
    assert_eq!(std::fs::read(&downloaded.path).unwrap(), png_bytes);

    mock_server.abort();
}

#[test]
fn parses_hyphen_text_values_across_affected_commands() {
    let ko = "- 목록 항목";
    let multiline = "- first\n- second";

    // 1. send chat X (+ alias `msg`, multiline, subject, trailing --json)
    match Cli::try_parse_from(["teams", "send", "chat:c1", ko, "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::Send {
            target,
            markdown,
            json,
            ..
        } => {
            assert_eq!(target, "chat:c1");
            assert_eq!(markdown, ko);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from([
        "teams",
        "msg",
        "chat:c1",
        multiline,
        "--subject",
        ko,
        "--mention",
        "u1:Alice",
        "--important",
        "--json",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Send {
            target,
            markdown,
            subject,
            mention,
            important,
            json,
            full: false,
        } => {
            assert_eq!(target, "chat:c1");
            assert_eq!(markdown, multiline);
            assert_eq!(subject.as_deref(), Some(ko));
            assert_eq!(mention, vec!["u1:Alice"]);
            assert!(important);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    // send chat body --subject X
    match Cli::try_parse_from(["teams", "send", "chat:c1", "body", "--subject", ko, "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::Send {
            target,
            markdown,
            subject,
            json,
            ..
        } => {
            assert_eq!(target, "chat:c1");
            assert_eq!(markdown, "body");
            assert_eq!(subject.as_deref(), Some(ko));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 2. reply chat id X
    match Cli::try_parse_from([
        "teams", "reply", "chat:c1", "m1", multiline, "--mention", "u1:Alice", "--json",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Reply {
            target,
            msg_id,
            markdown,
            mention,
            json,
            ..
        } => {
            assert_eq!(target, "chat:c1");
            assert_eq!(msg_id, "m1");
            assert_eq!(markdown, multiline);
            assert_eq!(mention, vec!["u1:Alice"]);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 3. edit chat id X, edit --find/--replace, edit --subject
    match Cli::try_parse_from(["teams", "edit", "chat:c1", "m1", ko, "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::Edit {
            target,
            msg_id,
            new_markdown,
            json,
            ..
        } => {
            assert_eq!(target, "chat:c1");
            assert_eq!(msg_id, "m1");
            assert_eq!(new_markdown.as_deref(), Some(ko));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from([
        "teams", "edit", "chat:c1", "m1", "--find", "word", "--replace", ko, "--json",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Edit {
            find,
            replace,
            json,
            ..
        } => {
            assert_eq!(find.as_deref(), Some("word"));
            assert_eq!(replace.as_deref(), Some(ko));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from([
        "teams", "edit", "chat:c1", "m1", "--find", ko, "--replace", "word", "--json",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Edit {
            find,
            replace,
            json,
            ..
        } => {
            assert_eq!(find.as_deref(), Some(ko));
            assert_eq!(replace.as_deref(), Some("word"));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from([
        "teams", "edit", "chan:t1:c1", "m1", "--subject", ko, "--json",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Edit { subject, json, .. } => {
            assert_eq!(subject.as_deref(), Some(ko));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 4. new-chat --message and --topic
    match Cli::try_parse_from([
        "teams",
        "new-chat",
        "--to",
        "audit@example.invalid",
        "--topic",
        ko,
        "--message",
        multiline,
        "--json",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::NewChat {
            to,
            topic,
            message,
            json,
        } => {
            assert_eq!(to, vec!["audit@example.invalid"]);
            assert_eq!(topic.as_deref(), Some(ko));
            assert_eq!(message.as_deref(), Some(multiline));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 5. update-chat --topic
    match Cli::try_parse_from(["teams", "update-chat", "chat:c1", "--topic", ko, "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::UpdateChat {
            target,
            topic,
            json,
        } => {
            assert_eq!(target, "chat:c1");
            assert_eq!(topic, ko);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 6. search X (with trailing options including --top alias)
    match Cli::try_parse_from(["teams", "search", ko, "--top", "15", "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::Search {
            query, limit, json, ..
        } => {
            assert_eq!(query, ko);
            assert_eq!(limit, 15);
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // 7. send-file --caption (and alias `file`)
    match Cli::try_parse_from(["teams", "file", "chat:c1", "shot.png", "--caption", ko, "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::SendFile { caption, json, .. } => {
            assert_eq!(caption.as_deref(), Some(ko));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn preserves_flags_missing_values_equals_separator_and_help() {
    // Explicit `--flag=value` and positional `--` still allow flag-looking text
    match Cli::try_parse_from([
        "teams",
        "send",
        "chat:c1",
        "--subject=--dry-run",
        "--",
        "--dry-run",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Send {
            markdown, subject, ..
        } => {
            assert_eq!(markdown, "--dry-run");
            assert_eq!(subject.as_deref(), Some("--dry-run"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from([
        "teams",
        "edit",
        "chat:c1",
        "m1",
        "--find=--json",
        "--replace=--help",
    ])
    .unwrap()
    .command
    .unwrap()
    {
        Command::Edit { find, replace, .. } => {
            assert_eq!(find.as_deref(), Some("--json"));
            assert_eq!(replace.as_deref(), Some("--help"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from(["teams", "search", "--", "--json"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::Search { query, json, .. } => {
            assert_eq!(query, "--json");
            assert!(!json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match Cli::try_parse_from(["teams", "send", "chat:c1", "---\nbody"])
        .unwrap()
        .command
        .unwrap()
    {
        Command::Send { markdown, .. } => assert_eq!(markdown, "---\nbody"),
        other => panic!("unexpected: {other:?}"),
    }

    // Known and unknown flags must NOT be swallowed as values or positionals
    for invalid in [
        vec!["teams", "send", "chat:c1", "--json"],
        vec!["teams", "send", "chat:c1", "--dry-run"],
        vec!["teams", "send", "chat:c1", "body", "--subject", "--json"],
        vec!["teams", "send", "chat:c1", "body", "--subject", "--dry-run"],
        vec!["teams", "send", "chat:c1", "- 목록 항목", "--subject"],
        vec!["teams", "reply", "chat:c1", "m1", "--json"],
        vec!["teams", "reply", "chat:c1", "m1", "--dry-run"],
        vec!["teams", "edit", "chat:c1", "m1", "--json"],
        vec!["teams", "edit", "chat:c1", "m1", "--dry-run"],
        vec!["teams", "edit", "chat:c1", "m1", "--find", "--replace", "new"],
        vec!["teams", "edit", "chat:c1", "m1", "--find", "old", "--replace", "--json"],
        vec!["teams", "edit", "chat:c1", "m1", "--find", "old", "--replace", "--dry-run"],
        vec!["teams", "edit", "chat:c1", "m1", "--find", "- old", "--replace"],
        vec!["teams", "edit", "chat:c1", "m1", "--subject", "--json"],
        vec!["teams", "update-chat", "chat:c1", "--topic", "--json"],
        vec!["teams", "new-chat", "--to", "a@b", "--message", "--json"],
        vec!["teams", "new-chat", "--to", "a@b", "--topic", "--message", "hi"],
        vec!["teams", "search", "--json"],
        vec!["teams", "search", "--dry-run"],
        // Non-text fields (IDs/targets/numbers) keep their existing parsers
        vec!["teams", "send", "-bad-target", "body"],
        vec!["teams", "search", "- 목록 항목", "--limit", "-5"],
    ] {
        let err = Cli::try_parse_from(invalid.clone()).expect_err(&format!(
            "expected parse error for {invalid:?}"
        ));
        assert!(
            !matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ),
            "unexpected help/version for {invalid:?}: {err}"
        );
    }

    // Actual help invocations must return DisplayHelp (never swallowed as text)
    for help_argv in [
        vec!["teams", "send", "--help"],
        vec!["teams", "send", "chat:c1", "--help"],
        vec!["teams", "send", "chat:c1", "-h"],
        vec!["teams", "send", "chat:c1", "- 목록 항목", "--help"],
        vec!["teams", "edit", "chat:c1", "m1", "--help"],
        vec!["teams", "search", "--help"],
        vec!["teams", "search", "-h"],
        vec!["teams", "search", "- 목록 항목", "--help"],
    ] {
        let err = Cli::try_parse_from(help_argv.clone()).expect_err(&format!(
            "expected DisplayHelp for {help_argv:?}"
        ));
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelp,
            "expected DisplayHelp for {help_argv:?}, got {err}"
        );
        assert!(
            try_fast_help_or_version_from(help_argv.clone(), Persona::Teams).is_some(),
            "expected fast help for {help_argv:?}"
        );
    }

    // `--subject=--help` and `-- --help` are NOT help invocations
    assert!(
        try_fast_help_or_version_from(
            ["teams", "send", "chat:c1", "body", "--subject=--help"],
            Persona::Teams,
        )
        .is_none()
    );
    assert!(
        try_fast_help_or_version_from(
            ["teams", "send", "chat:c1", "--", "--help"],
            Persona::Teams,
        )
        .is_none()
    );

    // Ordinary argv with no candidate hyphen value must skip clap command inspection
    let to_os = |slice: &[&str]| -> Vec<std::ffi::OsString> {
        slice.iter().map(std::ffi::OsString::from).collect()
    };
    for ordinary in [
        &["teams"][..],
        &["teams", "--help"][..],
        &["teams", "chats", "--limit", "50", "--json"][..],
        &["teams", "read", "chat:c1", "--json", "--compact"][..],
        &["teams", "send", "chat:c1", "hello", "--subject", "notice", "--json"][..],
        &["teams", "send", "chat:c1", "--help"][..],
        &["teams", "send", "chat:c1", "-h"][..],
        &["teams", "send", "chat:c1", "--dry-run"][..],
        &["teams", "send", "chat:c1", "body", "--subject=--dry-run"][..],
        &["teams", "send", "chat:c1", "--", "- 목록 항목"][..],
        &["teams", "edit", "chat:c1", "m1", "--find=--json", "--replace=--help"][..],
        &["teams", "search", "normal query", "--top", "10", "--json"][..],
    ] {
        assert!(
            !has_candidate_hyphen_value(&to_os(ordinary)),
            "expected fast-path skip for {ordinary:?}"
        );
    }
    for candidate in [
        &["teams", "send", "chat:c1", "- 목록 항목"][..],
        &["teams", "msg", "chat:c1", "- first\n- second", "--json"][..],
        &["teams", "edit", "chat:c1", "m1", "--find", "- old", "--replace", "new"][..],
        &["teams", "update-chat", "chat:c1", "--topic", "- 목록 항목"][..],
        &["teams", "search", "- 목록 항목"][..],
    ] {
        assert!(
            has_candidate_hyphen_value(&to_os(candidate)),
            "expected candidate detection for {candidate:?}"
        );
    }
}

#[test]
fn paginate_boundaries_and_cursor_errors() {
    let items: Vec<usize> = (0..120).collect();

    let p1 = paginate(items.clone(), 50, None, 7).unwrap();
    assert_eq!(p1.items.len(), 50);
    assert_eq!(p1.items[0], 0);
    assert!(p1.has_more);
    assert_eq!(p1.next_cursor.as_deref(), Some("50-7"));
    assert_eq!(p1.total, 120);

    let p2 = paginate(items.clone(), 50, Some("50-7"), 7).unwrap();
    assert_eq!(p2.items.len(), 50);
    assert_eq!(p2.items[0], 50);
    assert!(p2.has_more);
    assert_eq!(p2.next_cursor.as_deref(), Some("100-7"));

    let p3 = paginate(items.clone(), 50, Some("100-7"), 7).unwrap();
    assert_eq!(p3.items.len(), 20);
    assert_eq!(p3.items[0], 100);
    assert!(!p3.has_more);
    assert_eq!(p3.next_cursor, None);

    let p_end = paginate(items.clone(), 50, Some("120-7"), 7).unwrap();
    assert!(p_end.items.is_empty());
    assert!(!p_end.has_more);
    assert_eq!(p_end.next_cursor, None);

    let p_all = paginate(items.clone(), 120, None, 7).unwrap();
    assert_eq!(p_all.items.len(), 120);
    assert!(!p_all.has_more);
    assert_eq!(p_all.next_cursor, None);

    let p_empty = paginate(Vec::<usize>::new(), 50, None, 7).unwrap();
    assert!(p_empty.items.is_empty());
    assert!(!p_empty.has_more);
    assert_eq!(p_empty.next_cursor, None);
    assert_eq!(p_empty.total, 0);

    for bad in ["abc", "50", "121-7", "-1-7"] {
        let err = paginate(items.clone(), 50, Some(bad), 7)
            .err()
            .expect("expected invalid cursor error");
        assert!(
            err.to_string().contains("Invalid cursor"),
            "cursor={bad} err={err}"
        );
    }

    let stale = paginate(items, 50, Some("50-8"), 7)
        .err()
        .expect("expected stale cursor error");
    assert!(stale.to_string().contains("Stale cursor"), "{stale}");
}

#[test]
fn compact_json_reports_omissions_and_hint() {
    use crate::models::ReactionSummary;

    let conv = ConversationSummary {
        conv_key: "chat:c1".to_string(),
        target: ConversationTarget::Chat {
            chat_id: "c1".to_string(),
        },
        kind: ConversationKind::Group,
        team_id: None,
        team_name: None,
        title: "Release Room".to_string(),
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
    };

    let base_msg = |id: &str| MessageItem {
        msg_key: format!("chat:c1|{id}"),
        conv_key: "chat:c1".to_string(),
        msg_id: id.to_string(),
        parent_msg_id: None,
        reply_to_id: None,
        reply_to_author: None,
        reply_to_preview: None,
        subject: None,
        author_id: Some("u1".to_string()),
        author_name: "Alice".to_string(),
        is_me: false,
        created_at: "2026-11-20T08:00:00Z".to_string(),
        last_modified_at: None,
        deleted: false,
        importance: "normal".to_string(),
        message_type: "message".to_string(),
        body_html: "<p>hi</p>".to_string(),
        body_plain: "hi".to_string(),
        attachments: vec![],
        mentions: vec![],
        reactions: vec![],
        reply_count: 0,
        web_url: None,
    };

    let m1 = base_msg("m1");
    let mut m2 = base_msg("m2");
    m2.deleted = true;
    m2.body_plain = "(deleted message)".to_string();

    let mut m3 = base_msg("m3");
    m3.subject = Some("Q4 Plan".to_string());
    m3.importance = "high".to_string();
    m3.mentions = vec![
        MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "Alice".to_string(),
        },
        MentionInfo {
            id: 1,
            user_id: "u2".to_string(),
            display_name: "Bob".to_string(),
        },
    ];
    m3.reactions = vec![ReactionSummary {
        reaction_type: "like".to_string(),
        count: 2,
        me_reacted: false,
        users: vec!["Alice".to_string(), "Bob".to_string()],
    }];

    let mut m4 = base_msg("m4");
    m4.reply_to_id = Some("m1".to_string());
    m4.attachments = vec![AttachmentInfo {
        id: "m1".to_string(),
        content_type: "messageReference".to_string(),
        content_url: None,
        name: None,
        content_json: Some("{}".to_string()),
        thumbnail_url: None,
    }];

    let mut m5 = base_msg("m5");
    m5.body_html =
        "<p>Check <a href=\"https://docs.example.com/runbook-42\">Incident Runbook</a></p>"
            .to_string();
    m5.body_plain = "Check Incident Runbook".to_string();

    let mut m6 = base_msg("m6");
    m6.body_html =
        "<p>See <a href=\"https://docs.example.com/bare\">https://docs.example.com/bare</a></p>"
            .to_string();
    m6.body_plain = "See https://docs.example.com/bare".to_string();

    let mut m7 = base_msg("m7");
    m7.attachments = vec![
        AttachmentInfo {
            id: "card-1".to_string(),
            content_type: "application/vnd.microsoft.card.adaptive".to_string(),
            content_url: None,
            name: None,
            content_json: Some(r#"{"type":"AdaptiveCard"}"#.to_string()),
            thumbnail_url: None,
        },
        AttachmentInfo {
            id: "file-1".to_string(),
            content_type: "reference".to_string(),
            content_url: Some("https://contoso.sharepoint.com/report.pdf".to_string()),
            name: Some("report.pdf".to_string()),
            content_json: None,
            thumbnail_url: None,
        },
        AttachmentInfo {
            id: "empty-ct".to_string(),
            content_type: "   ".to_string(),
            content_url: None,
            name: None,
            content_json: None,
            thumbnail_url: None,
        },
    ];

    let res = ReadMessagesResult {
        messages: vec![m1.clone(), m2, m3, m4, m5, m6, m7.clone()],
        has_more: false,
        next_before: Some("m1".to_string()),
    };

    let compact = read_messages_json(&conv, &res, Some("H"));
    assert_eq!(compact["omitted"]["mentions"], 2);
    assert_eq!(compact["omitted"]["reactions"], 1);
    assert_eq!(compact["omitted"]["fields"].as_array().unwrap().len(), 24);
    assert!(
        !compact["omitted"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "attachments.content_type")
    );
    assert_eq!(compact["hint"], "H");
    let msgs = compact["messages"].as_array().unwrap();
    assert!(msgs[0].get("deleted").is_none());
    assert!(msgs[0].get("subject").is_none());
    assert!(msgs[0].get("omitted").is_none());
    assert!(msgs[0].get("links").is_none());
    assert_eq!(msgs[1]["deleted"], true);
    assert_eq!(msgs[2]["subject"], "Q4 Plan");
    assert_eq!(msgs[2]["importance"], "high");
    assert_eq!(msgs[2]["omitted"], json!({ "mentions": 2, "reactions": 1 }));
    assert!(msgs[3].get("attachments").is_none());
    assert_eq!(
        msgs[4]["links"],
        json!([{ "text": "Incident Runbook", "url": "https://docs.example.com/runbook-42" }])
    );
    assert!(msgs[5].get("links").is_none(), "bare URL already in body_plain must not add links");
    assert_eq!(
        msgs[6]["attachments"],
        json!([
            {
                "id": "card-1",
                "name": null,
                "content_type": "application/vnd.microsoft.card.adaptive"
            },
            {
                "id": "file-1",
                "name": "report.pdf",
                "content_type": "reference"
            },
            {
                "id": "empty-ct",
                "name": null
            }
        ])
    );
    let ack = sent_ack_json(&conv, &m7);
    assert_eq!(ack["message"]["attachments"], msgs[6]["attachments"]);

    let full = read_messages_json(&conv, &res, None);
    assert!(full.get("omitted").is_none());
    assert!(full.get("hint").is_none());

    let thread = ThreadView {
        conv_key: conv.conv_key.clone(),
        root: m1,
        replies: vec![],
    };
    let t_compact = thread_json(
        &conv,
        &thread,
        ThreadCoverage {
            has_more: true,
            reply_total: None,
        },
        Some("H"),
    );
    assert!(t_compact["thread"]["root"]["reply_count"].is_null());
    assert_eq!(t_compact["thread"]["returned"], 0);
    assert_eq!(t_compact["thread"]["has_more"], true);
}

#[test]
fn members_page_distinguishes_empty_unknown_and_legacy() {
    let storage = Storage::open_in_memory().unwrap();
    let mk_conv = |id: &str| ConversationSummary {
        conv_key: format!("chat:{id}"),
        target: ConversationTarget::Chat {
            chat_id: id.to_string(),
        },
        kind: ConversationKind::Group,
        team_id: None,
        team_name: None,
        title: id.to_string(),
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
    };
    let mk_member = |uid: &str, name: &str| MemberInfo {
        user_id: uid.to_string(),
        member_id: None,
        display_name: name.to_string(),
        email: None,
        roles: vec![],
        presence: None,
    };

    // (a) Marker only, 0 rows -> verified empty
    let conv_a = mk_conv("a");
    record_cli_sync_cursor(&storage, &format!("cli:members:{}", conv_a.conv_key));
    let page_a = members_page(&storage, &conv_a, 50, None).unwrap();
    assert_eq!(page_a["total"], 0);
    assert_eq!(page_a["has_more"], false);
    assert!(page_a["fetched_at"].is_string());

    // (b) Neither marker nor rows -> unfetched error
    let conv_b = mk_conv("b");
    let err_b = members_page(&storage, &conv_b, 50, None).unwrap_err();
    assert!(
        err_b.to_string().contains("have not been fetched yet"),
        "{err_b}"
    );

    // (c) Legacy cache: 3 rows via upsert_members, no marker
    let conv_c = mk_conv("c");
    storage
        .upsert_members(
            &conv_c.conv_key,
            &[
                mk_member("u1", "Alice"),
                mk_member("u2", "Bob"),
                mk_member("u3", "Carol"),
            ],
        )
        .unwrap();
    let page_c = members_page(&storage, &conv_c, 50, None).unwrap();
    assert!(page_c["total"].is_null());
    assert!(page_c["has_more"].is_null());
    assert!(page_c["fetched_at"].is_null());
    let page_c2 = members_page(&storage, &conv_c, 2, None).unwrap();
    assert_eq!(page_c2["has_more"], true);
    assert_eq!(page_c2["next_cursor"], "2-0");

    // (d) 3 rows + marker
    let conv_d = mk_conv("d");
    storage
        .replace_members(
            &conv_d.conv_key,
            &[
                mk_member("u1", "Alice"),
                mk_member("u2", "Bob"),
                mk_member("u3", "Carol"),
            ],
        )
        .unwrap();
    record_cli_sync_cursor(&storage, &format!("cli:members:{}", conv_d.conv_key));
    let page_d = members_page(&storage, &conv_d, 50, None).unwrap();
    assert_eq!(page_d["total"], 3);
    assert_eq!(page_d["has_more"], false);
    assert!(page_d["fetched_at"].is_string());
}

#[test]
fn error_json_classifies_runtime_errors_and_candidates() {
    let not_signed = error_json(&anyhow::anyhow!(
        "Sign-in required. Run 'teams login' first."
    ));
    assert_eq!(not_signed["ok"], false);
    assert_eq!(not_signed["error"]["code"], "not_signed_in");
    assert!(not_signed["error"].get("http_status").is_none());

    let expired = error_json(&anyhow::Error::new(DetailedError {
        message: "Token expired (exp: 2026-01-01) — refresh the token and retry.".to_string(),
        details: json!({ "expired": true, "status": "expired" }),
    }));
    assert_eq!(expired["error"]["code"], "token_expired");
    assert_eq!(expired["error"]["details"]["expired"], true);

    let forbidden = error_json(&anyhow::anyhow!(
        "Graph API error (POST https://graph.microsoft.com/v1.0/chats/c1/messages): 403 Forbidden"
    ));
    assert_eq!(forbidden["error"]["code"], "permission_denied");
    assert_eq!(forbidden["error"]["http_status"], 403);

    let not_found = error_json(&anyhow::anyhow!(
        "Cannot reply: message missing-1 was not found in this chat: Graph API error (GET https://graph.microsoft.com/v1.0/chats/c1/messages/missing-1): 404 Not Found"
    ));
    assert_eq!(not_found["error"]["code"], "not_found");
    assert_eq!(not_found["error"]["http_status"], 404);

    // TEAMS-19: a "403" inside the URL / message id must not turn a 404 or 500 into
    // permission_denied; only the parsed response status (or a permission keyword) does.
    let id_with_403 = error_json(&anyhow::anyhow!(
        "Graph API error (GET https://graph.microsoft.com/v1.0/chats/c1/messages/1700000403123): 404 Not Found [NotFound] message 1700000403123 does not exist"
    ));
    assert_eq!(id_with_403["error"]["code"], "not_found");
    assert_eq!(id_with_403["error"]["http_status"], 404);
    let server_error = error_json(&anyhow::anyhow!(
        "Graph API error (GET https://graph.microsoft.com/v1.0/chats/19:403abc/messages): 500 Internal Server Error"
    ));
    assert_eq!(server_error["error"]["code"], "error");
    assert_eq!(server_error["error"]["http_status"], 500);
    assert_eq!(
        error_code_for_message(
            "Graph API error (GET https://graph.microsoft.com/v1.0/me/joinedTeams): 403 Forbidden [Forbidden] Missing scope"
        ),
        "permission_denied"
    );
    assert_eq!(
        error_code_for_message("Authorization_RequestDenied: Insufficient privileges to complete the operation."),
        "permission_denied"
    );
    let untrusted = error_json(&anyhow::anyhow!(
        "Graph HTTP request failed (GET https://graph.microsoft.com/v1.0/me). {} (unknown issuer). If your network uses a TLS-inspecting proxy, ...: error sending request",
        crate::http::TLS_UNTRUSTED_MARKER
    ));
    assert_eq!(untrusted["error"]["code"], "tls_untrusted_certificate");
    assert!(untrusted["error"].get("http_status").is_none());

    let target_missing = error_json(&anyhow::anyhow!(
        "Conversation/channel not found: NonExistentRoom"
    ));
    assert_eq!(target_missing["error"]["code"], "target_not_found");

    let generic = error_json(&anyhow::anyhow!(
        "Deleting with --json requires passing --yes."
    ));
    assert_eq!(generic["error"]["code"], "error");
    assert!(generic["error"].get("http_status").is_none());

    let amb = error_json(&anyhow::Error::new(resolver::AmbiguousTarget {
        message: "Multiple targets match title 'General' exactly.".to_string(),
        candidates: vec![
            resolver::TargetCandidate {
                conv_key: "chan:team-eng:c1".to_string(),
                title: "General".to_string(),
                team_name: Some("Engineering".to_string()),
            },
            resolver::TargetCandidate {
                conv_key: "chan:team-des:c2".to_string(),
                title: "General".to_string(),
                team_name: Some("Design".to_string()),
            },
        ],
    }));
    assert_eq!(amb["error"]["code"], "target_ambiguous");
    assert_eq!(amb["error"]["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(amb["error"]["candidates"][0]["team_name"], "Engineering");

    let unv = error_json(&anyhow::Error::new(resolver::UnverifiedTarget {
        message: "Not sent: cannot confirm target 'General' because the teams/channels lookup failed: Graph API error (GET https://graph.microsoft.com/v1.0/me/joinedTeams): 403 Forbidden".to_string(),
        candidates: vec![resolver::TargetCandidate {
            conv_key: "chat:19:group-general-sync@thread.v2".to_string(),
            title: "General Sync".to_string(),
            team_name: None,
        }],
    }));
    assert_eq!(unv["error"]["code"], "permission_denied");
    assert_eq!(unv["error"]["http_status"], 403);
    assert_eq!(
        unv["error"]["candidates"][0]["conv_key"],
        "chat:19:group-general-sync@thread.v2"
    );
    assert!(unv["error"]["candidates"][0].get("team_name").is_none());
}

#[tokio::test]
async fn build_quote_reply_uses_cache_and_targets_single_message_on_miss() {
    use axum::{routing::get, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let item_hits = Arc::new(AtomicUsize::new(0));
    let list_hits = Arc::new(AtomicUsize::new(0));
    let ih = Arc::clone(&item_hits);
    let lh = Arc::clone(&list_hits);

    let router = Router::new()
        .route(
            "/v1.0/chats/c1/messages/m-old",
            get(move || {
                let ih = Arc::clone(&ih);
                async move {
                    ih.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({
                        "id": "m-old",
                        "messageType": "message",
                        "createdDateTime": "2026-10-01T10:00:00Z",
                        "from": { "user": { "id": "u2", "displayName": "Alice" } },
                        "body": { "contentType": "text", "content": "older message body" }
                    }))
                }
            }),
        )
        .route(
            "/v1.0/chats/c1/messages",
            get(move || {
                let lh = Arc::clone(&lh);
                async move {
                    lh.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({ "value": [] }))
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
        user_id: Some("me".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me");
    let storage = Storage::open_in_memory().unwrap();
    let conv = ConversationSummary {
        conv_key: "chat:c1".into(),
        target: ConversationTarget::Chat {
            chat_id: "c1".into(),
        },
        kind: ConversationKind::OneOnOne,
        team_id: None,
        team_name: None,
        title: "Alice".into(),
        topic: None,
        unread_count: 0,
        last_read_at: None,
        last_msg_id: None,
        last_msg_author: None,
        last_msg_preview: None,
        last_updated_at: None,
        pinned: false,
        peer_user_id: Some("u2".into()),
        web_url: None,
    };

    // First call (cache miss): must issue 1 targeted GET /messages/m-old and 0 list calls.
    let q1 = build_quote_reply(&storage, &graph, &conv, "m-old")
        .await
        .expect("quoted reply should be built via targeted GET");
    assert_eq!(q1.msg_id, "m-old");
    assert_eq!(q1.author_name, "Alice");
    assert_eq!(q1.preview_plain, "older message body");
    assert_eq!(item_hits.load(Ordering::SeqCst), 1);
    assert_eq!(list_hits.load(Ordering::SeqCst), 0);

    // Second call (now cached in SQLite): must make 0 additional Graph calls.
    let q2 = build_quote_reply(&storage, &graph, &conv, "m-old")
        .await
        .expect("cached quote should succeed");
    assert_eq!(q2.msg_id, "m-old");
    assert_eq!(item_hits.load(Ordering::SeqCst), 1);
    assert_eq!(list_hits.load(Ordering::SeqCst), 0);

    server.abort();
}

#[test]
fn format_message_line_sanitizes_terminal_escapes_and_bidi_controls() {
    let msg = MessageItem {
        msg_key: "chat:c1:m-sec".into(),
        msg_id: "m-sec".into(),
        conv_key: "chat:c1".into(),
        parent_msg_id: None,
        author_id: Some("u1".into()),
        author_name: "\x1b[31mMallory\x1b[0m\r\nSpoof".into(),
        is_me: false,
        message_type: "message".into(),
        created_at: "2026-04-01T00:00:00Z".into(),
        last_modified_at: None,
        subject: Some("Alert\x1b]52;c;c2VjcmV0\x07\u{202E}gpj.exe".into()),
        body_html: String::new(),
        body_plain: "Hello \x1b[2Jworld\r\n\u{E0001}\u{E0069}safe".into(),
        importance: "normal".into(),
        reply_count: 0,
        deleted: false,
        attachments: vec![],
        reactions: vec![],
        mentions: vec![],
        reply_to_id: None,
        reply_to_author: Some("Alice\x1b[33m".into()),
        reply_to_preview: Some("line1\r\n\x1b[31mline2".into()),
        web_url: None,
    };
    let rendered = format_message_line(&msg, "");
    assert!(!rendered.contains('\x1b'));
    assert!(!rendered.contains('\u{202E}'));
    assert!(!rendered.contains('\u{E0001}'));
    assert!(rendered.contains("Mallory Spoof (m-sec)"));
    assert!(rendered.contains("[Subject: Alertgpj.exe]"));
    assert!(rendered.contains("↩ Alice: line1 line2"));
    assert!(rendered.ends_with(": Hello world\nsafe"));
}
