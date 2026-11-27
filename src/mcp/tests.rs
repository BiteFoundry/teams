use super::*;
use crate::models::ConversationTarget;

// Tool descriptions are read by agents before every call: state what a call costs, and keep
// them short because each one is paid for in every agent context.
#[test]
fn tool_descriptions_state_request_cost_and_stay_short() {
    const BUDGET: usize = 400;
    for tool in tool_catalog() {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(description.len() <= BUDGET, "{name}: {} chars", description.len());
        let props = &tool["inputSchema"]["properties"];
        if let Some(refresh) = props.get("refresh") {
            let d = refresh["description"].as_str().unwrap_or("");
            assert!(d.contains("Graph") && d.contains("request"), "{name}.refresh: {d}");
        }
        if let Some(offline) = props.get("offline") {
            let d = offline["description"].as_str().unwrap_or("");
            assert!(
                d.contains("no Graph request") || d.contains("without calling Microsoft Graph"),
                "{name}.offline: {d}"
            );
        }
    }
    let cost_stated = |name: &str| {
        tool_catalog()
            .into_iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t["description"].as_str().map(str::to_string))
            .is_some_and(|d| d.contains("request") || d.contains("size"))
    };
    for name in [
        "list_chats",
        "list_teams_channels",
        "get_messages",
        "get_thread",
        "list_members",
        "send_file",
        "download_attachment",
        "search_messages",
        "search_users",
        "get_presence",
    ] {
        assert!(cost_stated(name), "{name} has no cost hint");
    }
}
use crate::auth::AuthManager;

fn test_ctx() -> (AppConfig, GraphClient, Storage) {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    (cfg, graph, storage)
}

#[tokio::test]
async fn handshake_and_notifications_and_ping() {
    let (cfg, graph, storage) = test_ctx();

    // Supported version echoed back
    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(resp["result"]["serverInfo"]["name"], "teams");
    assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);

    // Unknown protocol version falls back to 2025-06-18
    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");

    // Notification produces no response
    assert!(handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await
    .is_none());

    // Ping returns empty object
    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
    )
    .await
    .unwrap();
    assert_eq!(resp["result"], json!({}));

    // Malformed JSON -> -32700
    let err = handle_rpc_line(&cfg, &graph, &storage, "{not json")
        .await
        .unwrap();
    assert_eq!(err["error"]["code"], PARSE_ERROR);

    // Unknown method -> -32601
    let err = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":4,"method":"unknown/op"}"#,
    )
    .await
    .unwrap();
    assert_eq!(err["error"]["code"], METHOD_NOT_FOUND);
}

#[tokio::test]
async fn tools_list_exposes_every_cataloged_tool() {
    let (cfg, graph, storage) = test_ctx();
    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/list"}"#,
    )
    .await
    .unwrap();

    let tools = resp["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "list_chats",
            "create_chat",
            "update_chat",
            "list_teams_channels",
            "get_messages",
            "get_thread",
            "send_message",
            "reply_message",
            "send_file",
            "download_attachment",
            "edit_message",
            "delete_message",
            "search_messages",
            "list_members",
            "search_users",
            "get_presence",
        ]
    );
    for name in &names {
        assert!(
            !name.contains("mail")
                && !name.contains("calendar")
                && !name.contains("contact")
                && !name.contains("todo")
                && !name.starts_with("outlook_")
                && !name.starts_with("onenote_"),
            "unexpected non-Teams tool: {name}"
        );
    }

    for tool in tools {
        let tool_name = tool["name"].as_str().unwrap();
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool_name}");
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()), "{tool_name}");
        let ann = tool["annotations"]
            .as_object()
            .unwrap_or_else(|| panic!("missing annotations on {tool_name}"));
        for key in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(
                ann.get(key).is_some_and(Value::is_boolean),
                "missing boolean annotation {key} on {tool_name}"
            );
        }
        if let Some(props) = tool["inputSchema"]["properties"].as_object() {
            for (prop_name, prop_schema) in props {
                assert!(
                    prop_schema["description"]
                        .as_str()
                        .is_some_and(|d| !d.is_empty()),
                    "missing description on {tool_name}.{prop_name}"
                );
            }
        }
    }

    let get_msgs = tools.iter().find(|t| t["name"] == "get_messages").unwrap();
    assert!(get_msgs["inputSchema"]["properties"]["before"].is_object());
}

#[tokio::test]
async fn delete_message_rejects_missing_or_non_literal_true_confirm() {
    let (cfg, graph, storage) = test_ctx();

    for bad_args in [
        json!({ "target": "chat:1", "msg_id": "m1" }),
        json!({ "target": "chat:1", "msg_id": "m1", "confirm": false }),
        json!({ "target": "chat:1", "msg_id": "m1", "confirm": "true" }),
        json!({ "target": "chat:1", "msg_id": "m1", "confirm": 1 }),
        json!({ "target": "chat:1", "msg_id": "m1", "confirm": null }),
    ] {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {
                "name": "delete_message",
                "arguments": bad_args,
            }
        })
        .to_string();
        let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
        assert_eq!(resp["result"]["isError"], true, "{resp}");
        let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("confirm"), "{msg}");
    }

    // Literal true passes validation
    validate_tool_args(
        "delete_message",
        &json!({ "target": "chat:1", "msg_id": "m1", "confirm": true }),
    )
    .unwrap();
}

#[test]
fn argument_validation_covers_tools() {
    assert!(validate_tool_args(
        "get_messages",
        &json!({ "target": "chat:1", "limit": 100, "before": "2026-11-01" })
    )
    .is_ok());
    assert!(validate_tool_args(
        "get_messages",
        &json!({ "target": "chat:1", "before": 123 })
    )
    .is_err());

    assert!(validate_tool_args(
        "send_message",
        &json!({
            "target": "Engineering/General",
            "markdown": "Hello @Alice",
            "mentions": ["u1:Alice", { "user_id": "u2", "display_name": "Bob" }]
        })
    )
    .is_ok());

    assert!(validate_tool_args(
        "send_message",
        &json!({ "target": "chat:1", "markdown": "   " })
    )
    .is_err());

    assert!(validate_tool_args("create_chat", &json!({ "members": [] })).is_err());
    assert!(
        validate_tool_args("create_chat", &json!({ "members": ["a@b.com"] })).is_ok()
    );

    for removed in ["react_message", "mark_read", "set_presence"] {
        assert!(
            validate_tool_args(removed, &json!({})).is_err(),
            "{removed} should not be exposed as an MCP tool"
        );
    }
}

#[test]
fn edited_and_compact_tools_validate_their_options() {
    for arguments in [
        json!({"target": "chat:c", "msg_id": "m", "markdown": "Full"}),
        json!({"target": "chat:c", "msg_id": "m", "find": "old", "replace": ""}),
        json!({"target": "chan:t:c", "msg_id": "m", "subject": "New title"}),
    ] {
        validate_tool_args("edit_message", &arguments).unwrap();
    }
    for arguments in [
        json!({"target": "chat:c", "msg_id": "m", "markdown": "Full", "subject": "title"}),
        json!({"target": "chat:c", "msg_id": "m", "markdown": "Full", "find": "old", "replace": ""}),
        json!({"target": "chat:c", "msg_id": "m", "find": "old"}),
        json!({"target": "chat:c", "msg_id": "m", "find": "", "replace": "new"}),
        json!({"target": "chat:c", "msg_id": "m", "find": "old", "replace": null}),
        json!({"target": "chan:t:c", "msg_id": "m", "subject": "New title", "thread_root_id": "root"}),
        json!({"target": "chan:t:c", "msg_id": "m", "subject": " "}),
    ] {
        assert!(validate_tool_args("edit_message", &arguments).is_err(), "{arguments}");
    }
    validate_tool_args(
        "get_thread",
        &json!({"target": "chan:t:c", "root_msg_id": "m", "offline": true, "compact": true}),
    )
    .unwrap();
    assert!(validate_tool_args(
        "get_thread",
        &json!({"target": "chan:t:c", "root_msg_id": "m", "offline": "true"}),
    )
    .is_err());
    assert!(validate_tool_args(
        "get_messages",
        &json!({"target": "chat:c", "format": "plain"}),
    )
    .is_err());
    assert!(validate_tool_args("update_chat", &json!({
        "target": "chat:c", "topic": "New topic", "members": []
    })).is_err());
    validate_tool_args(
        "search_users",
        &json!({"query": "Alice", "limit": 5}),
    )
    .unwrap();
    assert!(validate_tool_args("search_users", &json!({})).is_err());
    assert!(validate_tool_args("search_users", &json!({"query": ""})).is_err());
    assert!(validate_tool_args(
        "search_users",
        &json!({"query": "Alice", "extra": true}),
    )
    .is_err());
    validate_tool_args(
        "send_message",
        &json!({"target": "chat:c", "markdown": "hi", "compact": false}),
    )
    .unwrap();
    assert!(validate_tool_args(
        "send_message",
        &json!({"target": "chat:c", "markdown": "hi", "compact": "false"}),
    )
    .is_err());
}

#[tokio::test]
async fn offline_get_messages_before_and_search_messages_work_against_sqlite() {
    use crate::models::{ConversationKind, MessageItem};

    let (cfg, graph, storage) = test_ctx();
    storage
        .upsert_conversations(&[ConversationSummary {
            conv_key: "chat:19:c1".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "19:c1".to_string(),
            },
            kind: ConversationKind::Group,
            team_id: None,
            team_name: None,
            title: "Engineering Discussion".to_string(),
            topic: Some("Engineering Discussion".to_string()),
            unread_count: 0,
            last_read_at: None,
            last_msg_id: Some("msg-2".to_string()),
            last_msg_author: Some("Bob".to_string()),
            last_msg_preview: Some("Check release tag".to_string()),
            last_updated_at: Some("2026-11-15T06:00:00Z".to_string()),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }])
        .unwrap();
    storage
        .upsert_messages(&[
            MessageItem {
                msg_key: "chat:19:c1|msg-1".to_string(),
                conv_key: "chat:19:c1".to_string(),
                msg_id: "msg-1".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u1".to_string()),
                author_name: "Bob".to_string(),
                is_me: false,
                created_at: "2026-11-10T06:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>Older message</p>".to_string(),
                body_plain: "Older message".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            },
            MessageItem {
                msg_key: "chat:19:c1|msg-2".to_string(),
                conv_key: "chat:19:c1".to_string(),
                msg_id: "msg-2".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u1".to_string()),
                author_name: "Bob".to_string(),
                is_me: false,
                created_at: "2026-11-15T06:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>Please check the release tag</p>".to_string(),
                body_plain: "Please check the release tag".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            },
        ])
        .unwrap();

    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "get_messages",
                "arguments": { "target": "Engineering Discussion", "offline": true, "before": "msg-2" }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let payload: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
    assert_eq!(payload["messages"][0]["msg_id"], "msg-1");
    assert_eq!(payload["has_more"], false);
    assert_eq!(payload["next_before"], "msg-1");

    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "search_messages",
                "arguments": { "query": "release", "target": "Engineering Discussion" }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let payload: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn reply_message_supports_mentions_and_important() {
    let catalog = tool_catalog();
    let reply_tool = catalog
        .iter()
        .find(|t| t["name"] == "reply_message")
        .unwrap();
    assert!(reply_tool["inputSchema"]["properties"]["mentions"].is_object());
    assert!(reply_tool["inputSchema"]["properties"]["important"].is_object());

    assert!(validate_tool_args(
        "reply_message",
        &json!({
            "target": "Engineering/General",
            "msg_id": "m1",
            "markdown": "Thanks @Alice",
            "mentions": ["u1:Alice"],
            "important": true
        })
    )
    .is_ok());

    assert!(validate_tool_args(
        "reply_message",
        &json!({
            "target": "Engineering/General",
            "msg_id": "m1",
            "markdown": "Thanks",
            "important": "yes"
        })
    )
    .is_err());
}

#[tokio::test]
async fn bounded_line_reader_caps_oversized_frames() {
    let data = b"hello\r\n0123456789ABCDEF\nworld";
    let mut reader = BufReader::new(std::io::Cursor::new(&data[..]));

    match read_line_bounded(&mut reader, 10).await.unwrap() {
        BoundedLine::Line(s) => assert_eq!(s, "hello"),
        _ => panic!("expected first line"),
    }
    match read_line_bounded(&mut reader, 10).await.unwrap() {
        BoundedLine::TooLong => {}
        _ => panic!("expected TooLong"),
    }
    match read_line_bounded(&mut reader, 10).await.unwrap() {
        BoundedLine::Line(s) => assert_eq!(s, "world"),
        _ => panic!("expected trailing line at EOF"),
    }
    match read_line_bounded(&mut reader, 10).await.unwrap() {
        BoundedLine::Eof => {}
        _ => panic!("expected Eof"),
    }
}

#[tokio::test]
async fn cached_inline_hosted_image_exposed_in_mcp_get_messages_and_download() {
    use crate::auth::StoredTokens;
    use crate::models::{ConversationKind, MessageItem};
    use axum::{http::StatusCode, routing::get, Router};

    let png_bytes = b"\x89PNG\r\n\x1a\nmcp-inline-shot".to_vec();
    let served_bytes = png_bytes.clone();
    let mock_app = Router::new().route(
        "/v1.0/chats/c1/messages/m2/hostedContents/1/$value",
        get(move || {
            let bytes = served_bytes.clone();
            async move { (StatusCode::OK, bytes) }
        }),
    );
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_server = tokio::spawn(async move {
        axum::serve(mock_listener, mock_app).await.unwrap();
    });

    let dl_dir = tempfile::tempdir().unwrap();
    let cfg = AppConfig {
        graph_base_url: format!("http://{mock_addr}/v1.0"),
        download_dir: dl_dir.path().display().to_string(),
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
    let graph = GraphClient::new(cfg.clone(), auth).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    storage
        .upsert_conversations(&[ConversationSummary {
            conv_key: "chat:c1".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "MCP Inline Chat".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: Some("m2".to_string()),
            last_msg_author: Some("Alice".to_string()),
            last_msg_preview: Some("Inline".to_string()),
            last_updated_at: Some("2026-11-18T10:01:00Z".to_string()),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }])
        .unwrap();

    let graph_hosted_url =
        "https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value";
    let local_hosted_url =
        format!("http://{mock_addr}/v1.0/chats/c1/messages/m2/hostedContents/1/$value");

    storage
        .upsert_messages(&[
            MessageItem {
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
                body_html: format!(r#"<p>Inline</p><img src="{graph_hosted_url}">"#),
                body_plain: "Inline".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            },
            MessageItem {
                msg_key: "chat:c1|m2".to_string(),
                conv_key: "chat:c1".to_string(),
                msg_id: "m2".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u1".to_string()),
                author_name: "Alice".to_string(),
                is_me: false,
                created_at: "2026-11-18T10:01:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: format!(r#"<img src="{local_hosted_url}">"#),
                body_plain: String::new(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            },
        ])
        .unwrap();

    // MCP get_messages (offline) hydrates hosted:1 attachment
    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 30,
            "method": "tools/call",
            "params": {
                "name": "get_messages",
                "arguments": { "target": "chat:c1", "offline": true, "compact": false }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let payload: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["messages"][0]["attachments"][0]["id"], "hosted:1");
    assert_eq!(
        payload["messages"][0]["attachments"][0]["content_url"],
        graph_hosted_url
    );

    // MCP download_attachment downloads hosted:1 from cached message
    let dl_resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "tools/call",
            "params": {
                "name": "download_attachment",
                "arguments": { "target": "chat:c1", "msg_id": "m2", "index": 0 }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(dl_resp["result"]["isError"], false, "{dl_resp}");
    let dl_payload: Value =
        serde_json::from_str(dl_resp["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(dl_payload["ok"], true);
    assert_eq!(dl_payload["bytes_written"], png_bytes.len());
    assert_eq!(dl_payload["size_bytes"], png_bytes.len());
    assert_eq!(dl_payload["mime_type"], "image/png");
    assert_eq!(dl_payload["is_image"], true);
    let saved_path = dl_payload["path"].as_str().unwrap();
    assert!(std::path::Path::new(saved_path).is_absolute());
    assert_eq!(std::fs::read(saved_path).unwrap(), png_bytes);

    mock_server.abort();
}

#[test]
fn paged_list_tools_validate_limit_and_cursor() {
    validate_tool_args(
        "list_members",
        &json!({ "target": "chat:c1", "limit": 5, "cursor": "10-0", "offline": true }),
    )
    .unwrap();
    assert!(validate_tool_args("list_members", &json!({ "target": "chat:c1", "limit": 0 })).is_err());
    assert!(validate_tool_args("list_members", &json!({ "target": "chat:c1", "limit": 1001 })).is_err());
    assert!(validate_tool_args("list_members", &json!({ "target": "chat:c1", "cursor": 5 })).is_err());
    validate_tool_args(
        "list_teams_channels",
        &json!({ "limit": 50, "cursor": "0-0" }),
    )
    .unwrap();
    assert!(validate_tool_args("list_teams_channels", &json!({ "bogus": 1 })).is_err());
    assert!(validate_tool_args(
        "list_teams_channels",
        &json!({ "refresh": true, "cursor": "0-0" })
    )
    .is_err());
}

#[tokio::test]
async fn compact_get_messages_includes_non_empty_attachment_content_type() {
    use crate::models::{AttachmentInfo, ConversationKind, MessageItem};

    let (cfg, graph, storage) = test_ctx();
    storage
        .upsert_conversations(&[ConversationSummary {
            conv_key: "chat:19:cards".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "19:cards".to_string(),
            },
            kind: ConversationKind::Group,
            team_id: None,
            team_name: None,
            title: "Alerts Bot".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: Some("msg-card".to_string()),
            last_msg_author: Some("AlertBot".to_string()),
            last_msg_preview: Some("Deploy alert".to_string()),
            last_updated_at: Some("2026-11-20T09:00:00Z".to_string()),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }])
        .unwrap();
    storage
        .upsert_messages(&[MessageItem {
            msg_key: "chat:19:cards|msg-card".to_string(),
            conv_key: "chat:19:cards".to_string(),
            msg_id: "msg-card".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("bot-1".to_string()),
            author_name: "AlertBot".to_string(),
            is_me: false,
            created_at: "2026-11-20T09:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: String::new(),
            body_plain: "Deploy alert".to_string(),
            attachments: vec![
                AttachmentInfo {
                    id: "card-att-1".to_string(),
                    content_type: "application/vnd.microsoft.card.adaptive".to_string(),
                    content_url: None,
                    name: None,
                    content_json: Some(r#"{"type":"AdaptiveCard"}"#.to_string()),
                    thumbnail_url: None,
                },
                AttachmentInfo {
                    id: "unnamed-att-2".to_string(),
                    content_type: String::new(),
                    content_url: None,
                    name: None,
                    content_json: None,
                    thumbnail_url: None,
                },
            ],
            mentions: vec![],
            reactions: vec![],
            reply_count: 0,
            web_url: None,
        }])
        .unwrap();

    let resp = handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 40,
            "method": "tools/call",
            "params": {
                "name": "get_messages",
                "arguments": { "target": "chat:19:cards", "offline": true }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let payload: Value =
        serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload["messages"][0]["attachments"],
        json!([
            {
                "id": "card-att-1",
                "name": null,
                "content_type": "application/vnd.microsoft.card.adaptive"
            },
            {
                "id": "unnamed-att-2",
                "name": null
            }
        ])
    );
}
