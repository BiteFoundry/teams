//! Core auth, chats, channels, files, agent surfaces, 403 diagnostics, inline preview, and updater E2E tests.

use super::server::*;
use std::io::Cursor;
use axum::{response::IntoResponse, extract::State, http::{HeaderMap, StatusCode}, routing::get, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use image::{ImageBuffer, Rgba, RgbaImage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use teams_cli::{auth::{diagnose_access_token_scopes, explain_graph_permission_error, AuthManager, StoredTokens}, config::ImageProtocolPreference, graph::GraphClient, img_preview::{self, TerminalGraphicsCaps, TerminalImageProtocol}, mcp, models::{AttachmentInfo, ConversationTarget, MentionInfo, MessageItem, TeamSummary}, parser, resolver, serve, updater, storage::Storage};

#[tokio::test]
async fn e2e_mock_graph_auth_chats_channels_files_and_agent_surfaces() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");

    let (base_url, mock) = spawn_mock_graph_server("fresh-access-tok-2026").await;
    let cfg = build_test_config(&base_url, &download_dir);

    // -----------------------------------------------------------------------
    // 1. Token auto-refresh: expired StoredTokens refreshes before calling /me
    // -----------------------------------------------------------------------
    let expired_tokens = StoredTokens {
        access_token: "expired-access-tok".to_string(),
        refresh_token: Some("stale-refresh-tok".to_string()),
        expires_at: chrono::Utc::now().timestamp() - 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: None,
        user_display_name: None,
        user_email: None,
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), expired_tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone())
        .expect("build GraphClient")
        .with_my_user_id("me-user-1");

    let me = graph.get_me().await.expect("get_me after auto-refresh");
    assert_eq!(me.id, "me-user-1");
    assert_eq!(me.display_name, "Me (Tester)");
    assert_eq!(*mock.refresh_calls.read().unwrap(), 1);
    assert_eq!(
        mock.me_auth_headers.read().unwrap().as_slice(),
        &["Bearer fresh-access-tok-2026".to_string()]
    );
    let refreshed_state = auth.current_tokens().await.expect("tokens present");
    assert_eq!(refreshed_state.access_token, "fresh-access-tok-2026");
    assert_eq!(
        refreshed_state.refresh_token.as_deref(),
        Some("rotated-refresh-tok")
    );

    // -----------------------------------------------------------------------
    // 2. 1:1 & Group Chats + Markdown/@mention send + SQLite FTS5 Korean/EN search
    // -----------------------------------------------------------------------
    let storage = Storage::open(&db_path).expect("open SQLite storage");
    let chats = graph.list_chats(50).await.expect("list_chats");
    assert_eq!(chats.len(), 2);
    storage.upsert_conversations(&chats).expect("upsert chats");

    let one_on_one = chats
        .iter()
        .find(|c| c.conv_key == "chat:19:oneonone-peer@unq.gbl.spaces")
        .expect("1:1 chat present");
    assert_eq!(one_on_one.title, "Alice");
    assert_eq!(one_on_one.peer_user_id.as_deref(), Some("peer-kim"));

    let group_chat = chats
        .iter()
        .find(|c| c.conv_key == "chat:19:group-sprint@thread.v2")
        .expect("group chat present");
    assert_eq!(group_chat.title, "Backend Sprint");

    let outgoing_chat_msg = parser::compile_outgoing_markdown(
        "Hello @Alice! Staging **Kubernetes** 롤아웃 completed.",
        None,
        vec![MentionInfo {
            id: 0,
            user_id: "peer-kim".to_string(),
            display_name: "Alice".to_string(),
        }],
        vec![],
        vec![],
        None,
    );
    assert!(outgoing_chat_msg.body_html.contains("<at id=\"0\">Alice</at>"));
    assert!(outgoing_chat_msg
        .body_html
        .contains("<strong>Kubernetes</strong>"));

    let sent_chat = graph
        .send_message(&one_on_one.target, &outgoing_chat_msg)
        .await
        .expect("send chat message");
    assert!(sent_chat.is_me);

    let fetched_chat_msgs = graph
        .list_chat_messages("19:oneonone-peer@unq.gbl.spaces", 20)
        .await
        .expect("list_chat_messages");
    assert_eq!(fetched_chat_msgs.len(), 1);
    storage
        .upsert_messages(&fetched_chat_msgs)
        .expect("upsert chat messages");

    let ko_hits = storage
        .search_messages("롤아웃", None, 10)
        .expect("search Korean");
    assert_eq!(ko_hits.len(), 1);
    assert_eq!(ko_hits[0].conv_key, one_on_one.conv_key);
    assert!(ko_hits[0].snippet.contains("롤아웃"));

    let en_hits = storage
        .search_messages("Kubernetes", Some(&one_on_one.conv_key), 10)
        .expect("search English");
    assert_eq!(en_hits.len(), 1);
    assert_eq!(en_hits[0].msg_id, sent_chat.msg_id);

    // -----------------------------------------------------------------------
    // 3. Team -> Channel -> Thread: resolve "Engineering/Deployments", post with subject,
    //    reply in thread, fetch get_thread, verify reply_count in Storage
    // -----------------------------------------------------------------------
    let fetched = graph
        .list_joined_teams_with_channels()
        .await
        .expect("list_joined_teams_with_channels");
    let teams: Vec<TeamSummary> = storage
        .upsert_joined_teams(&fetched)
        .expect("upsert joined teams");
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].display_name, "Engineering");
    assert_eq!(teams[0].channels.len(), 2);

    let all_convs = storage.list_conversations().expect("list_conversations");
    let deploy_chan =
        resolver::resolve_from_slice(&all_convs, "Engineering/Deployments").expect("resolve channel");
    assert_eq!(
        deploy_chan.target,
        ConversationTarget::Channel {
            team_id: "team-dev".to_string(),
            channel_id: "chan-deploy".to_string(),
        }
    );

    let chan_post = parser::compile_outgoing_markdown(
        "**Starting production** v2.4.0 deployment. [Runbook](https://example.test/ops) @Alice",
        Some("v2.4.0 Release Notice".to_string()),
        vec![MentionInfo {
            id: 0,
            user_id: "peer-kim".to_string(),
            display_name: "Alice".to_string(),
        }],
        vec![],
        vec![],
        None,
    );
    let root_msg = graph
        .send_message(&deploy_chan.target, &chan_post)
        .await
        .expect("send channel root post");
    assert_eq!(root_msg.subject.as_deref(), Some("v2.4.0 Release Notice"));

    let chan_reply = parser::compile_outgoing_markdown(
        "Canary health check passed.",
        None,
        vec![],
        vec![],
        vec![],
        None,
    );
    let reply_msg = graph
        .reply_message(&deploy_chan.target, &root_msg.msg_id, &chan_reply)
        .await
        .expect("reply to channel thread");
    assert_eq!(
        reply_msg.parent_msg_id.as_deref(),
        Some(root_msg.msg_id.as_str())
    );

    let thread = graph
        .get_thread("team-dev", "chan-deploy", &root_msg.msg_id, 50)
        .await
        .expect("get_thread");
    assert_eq!(thread.root.msg_id, root_msg.msg_id);
    assert_eq!(thread.replies.len(), 1);
    assert_eq!(thread.root.reply_count, 1);

    let mut thread_bundle = vec![thread.root.clone()];
    thread_bundle.extend(thread.replies.iter().cloned());
    storage
        .upsert_messages(&thread_bundle)
        .expect("persist thread in storage");

    let stored_root = storage
        .get_message(&deploy_chan.conv_key, &root_msg.msg_id)
        .expect("get stored root")
        .expect("root exists in storage");
    assert_eq!(stored_root.reply_count, 1);

    let stored_thread = storage
        .get_thread(&deploy_chan.conv_key, &root_msg.msg_id, 50)
        .expect("get_thread from storage")
        .expect("thread exists in storage");
    assert_eq!(stored_thread.replies.len(), 1);

    // -----------------------------------------------------------------------
    // 4. File Attachments: Chat send_file, Channel Thread send_file, download_attachment
    // -----------------------------------------------------------------------
    let chat_file_path = temp.path().join("chat-spec.txt");
    let chat_file_bytes = b"Teams Chat Upload Payload \xed\x95\x9c\xea\xb8\x80";
    std::fs::write(&chat_file_path, chat_file_bytes).expect("write chat test file");

    let sent_chat_file = graph
        .send_file(&one_on_one.target, &chat_file_path, Some("<p>Chat attachment</p>"), None)
        .await
        .expect("send_file to chat");
    assert_eq!(sent_chat_file.attachments.len(), 1);
    assert_eq!(sent_chat_file.attachments[0].content_type, "reference");
    assert!(sent_chat_file
        .body_html
        .contains("<attachment id=\"153fa47d-18c9-4179-be08-9879815a9f90\"></attachment>"));

    // Verify wire payload posted to POST /chats/{id}/messages
    let chat_wire = mock
        .posted_chat_payloads
        .read()
        .unwrap()
        .last()
        .cloned()
        .expect("posted chat payload");
    let wire_body = chat_wire["body"]["content"].as_str().unwrap_or("");
    assert!(wire_body.contains("<attachment id=\"153fa47d-18c9-4179-be08-9879815a9f90\">"));
    assert_eq!(chat_wire["attachments"][0]["contentType"], "reference");

    // Channel Thread send_file
    let chan_file_path = temp.path().join("release-notes.md");
    let chan_file_bytes = b"# Release v2.4.0\nAll checks passed.\n";
    std::fs::write(&chan_file_path, chan_file_bytes).expect("write channel test file");

    let sent_chan_file = graph
        .send_file(
            &deploy_chan.target,
            &chan_file_path,
            Some("<p>Release notes attached</p>"),
            Some(&root_msg.msg_id),
        )
        .await
        .expect("send_file to channel thread");
    assert_eq!(sent_chan_file.attachments.len(), 1);
    assert_eq!(sent_chan_file.attachments[0].content_type, "reference");
    assert_eq!(
        sent_chan_file.parent_msg_id.as_deref(),
        Some(root_msg.msg_id.as_str())
    );
    let reply_wire = mock
        .posted_reply_payloads
        .read()
        .unwrap()
        .last()
        .cloned()
        .expect("posted reply payload");
    assert_eq!(reply_wire["attachments"][0]["contentType"], "reference");
    assert!(reply_wire["body"]["content"]
        .as_str()
        .unwrap_or("")
        .contains("<attachment id=\"223fa47d-18c9-4179-be08-9879815a9f91\">"));

    // download_attachment via /shares/u!.../driveItem/content
    let chat_att: &AttachmentInfo = &sent_chat_file.attachments[0];
    let downloaded = graph
        .download_attachment(chat_att, &download_dir)
        .await
        .expect("download_attachment");
    assert!(downloaded.path.exists());
    assert_eq!(
        std::fs::read(&downloaded.path).expect("read downloaded file"),
        chat_file_bytes
    );

    // -----------------------------------------------------------------------
    // 5. Agent Surfaces: mcp::handle_rpc_line & serve::build_router
    // -----------------------------------------------------------------------
    let init_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}"#,
    )
    .await
    .expect("mcp initialize response");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "teams");

    let tools_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await
    .expect("mcp tools/list response");
    let tool_names: Vec<&str> = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"list_chats"));
    assert!(tool_names.contains(&"get_thread"));
    assert!(tool_names.contains(&"delete_message"));

    let list_chats_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "list_chats",
            "arguments": { "refresh": true }
        }
    });
    let list_chats_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &list_chats_req.to_string())
        .await
        .expect("mcp list_chats response");
    assert_ne!(list_chats_resp["result"]["isError"], Value::Bool(true));
    let chats_text = list_chats_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("list_chats text");
    assert!(chats_text.contains("Alice"));
    assert!(chats_text.contains("Backend Sprint"));

    let get_thread_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "get_thread",
            "arguments": {
                "target": "Engineering/Deployments",
                "root_msg_id": root_msg.msg_id
            }
        }
    });
    let mut cold_thread_req = get_thread_req.clone();
    cold_thread_req["params"]["arguments"]["offline"] = json!(true);
    let cold_thread = mcp::handle_rpc_line(&cfg, &graph, &storage, &cold_thread_req.to_string())
        .await
        .expect("unverified offline thread response");
    assert_eq!(cold_thread["result"]["isError"], true);

    let get_thread_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &get_thread_req.to_string())
        .await
        .expect("mcp get_thread response");
    assert_ne!(get_thread_resp["result"]["isError"], Value::Bool(true));
    let thread_text = get_thread_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("get_thread text");
    assert!(thread_text.contains("v2.4.0 Release Notice"));
    assert!(thread_text.contains("Canary health check passed."));

    let mut offline_req = get_thread_req.clone();
    let arguments = &mut offline_req["params"]["arguments"];
    arguments["offline"] = json!(true);
    arguments["compact"] = json!(true);
    arguments["limit"] = json!(1);
    let offline_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &offline_req.to_string())
        .await
        .expect("offline compact get_thread response");
    assert_eq!(offline_resp["result"]["isError"], false, "{offline_resp}");
    let compact: Value = serde_json::from_str(
        offline_resp["result"]["content"][0]["text"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(compact["conversation"]["conv_key"], deploy_chan.conv_key);
    assert_eq!(compact["thread"]["root"]["msg_id"], root_msg.msg_id);
    assert_eq!(compact["thread"]["root"]["body_plain"], root_msg.body_plain);
    assert!(compact["thread"]["root"].get("body_html").is_none());
    assert_eq!(compact["thread"]["replies"].as_array().unwrap().len(), 1);
    assert_eq!(compact["thread"]["replies"][0]["msg_id"], sent_chan_file.msg_id);
    assert_eq!(compact["thread"]["replies"][0]["parent_msg_id"], root_msg.msg_id);

    offline_req["params"]["arguments"]["root_msg_id"] = json!("missing");
    let cache_miss = mcp::handle_rpc_line(&cfg, &graph, &storage, &offline_req.to_string())
        .await
        .expect("offline get_thread cache miss");
    assert_eq!(cache_miss["result"]["isError"], true);
    assert!(cache_miss["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Thread not found in local cache"));

    let before_edit = storage
        .get_thread(&deploy_chan.conv_key, &root_msg.msg_id, 50)
        .unwrap()
        .unwrap();
    let edit_req = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": {
            "name": "edit_message",
            "arguments": {
                "target": deploy_chan.conv_key, "msg_id": root_msg.msg_id,
                "find": "Starting production", "replace": "Ready <QA>",
            }
        }
    });
    let edited = mcp::handle_rpc_line(&cfg, &graph, &storage, &edit_req.to_string())
        .await
        .expect("partial edit response");
    assert_eq!(edited["result"]["isError"], false, "{edited}");
    let edited_thread = storage
        .get_thread(&deploy_chan.conv_key, &root_msg.msg_id, 50)
        .unwrap()
        .unwrap();
    assert_eq!(
        edited_thread.root.body_html,
        root_msg.body_html.replace("Starting production", "Ready &lt;QA&gt;")
    );
    assert_eq!(edited_thread.root.subject, root_msg.subject);
    assert_eq!(edited_thread.replies, before_edit.replies);

    let subject_req = json!({
        "jsonrpc": "2.0", "id": 11, "method": "tools/call",
        "params": {
            "name": "edit_message", "arguments": {
                "target": deploy_chan.conv_key,
                "msg_id": root_msg.msg_id,
                "subject": "Release title <QA>"
            }
        }
    });
    let result = mcp::handle_rpc_line(&cfg, &graph, &storage, &subject_req.to_string())
        .await
        .unwrap();
    assert_eq!(result["result"]["isError"], false, "{result}");
    let titled_thread = storage
        .get_thread(&deploy_chan.conv_key, &root_msg.msg_id, 50)
        .unwrap()
        .unwrap();
    let mut expected_root = edited_thread.root;
    assert_eq!(expected_root.mentions.len(), 1);
    expected_root.subject = Some("Release title <QA>".to_string());
    assert_eq!(titled_thread.root, expected_root);
    assert_eq!(titled_thread.replies, edited_thread.replies);

    storage.set_conversation_pinned(&group_chat.conv_key, true).unwrap();
    storage.increment_conversation_unread(&group_chat.conv_key, 2).unwrap();
    let mut expected_chat = storage.get_conversation(&group_chat.conv_key).unwrap().unwrap();
    let update_topic_req = json!({
        "jsonrpc": "2.0", "id": 12, "method": "tools/call",
        "params": {
            "name": "update_chat",
            "arguments": { "target": group_chat.conv_key, "topic": "Release group" }
        }
    });
    let result = mcp::handle_rpc_line(&cfg, &graph, &storage, &update_topic_req.to_string())
        .await
        .unwrap();
    assert_eq!(result["result"]["isError"], false, "{result}");
    expected_chat.title = "Release group".to_string();
    expected_chat.topic = Some(expected_chat.title.clone());
    assert_eq!(
        storage.get_conversation(&group_chat.conv_key).unwrap().unwrap(),
        expected_chat
    );

    // delete_message without confirm: true must be rejected
    let delete_missing_confirm = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "delete_message",
            "arguments": {
                "target": "Engineering/Deployments",
                "msg_id": root_msg.msg_id
            }
        }
    });
    let del_err_resp =
        mcp::handle_rpc_line(&cfg, &graph, &storage, &delete_missing_confirm.to_string())
            .await
            .expect("mcp delete_message rejection");
    assert_eq!(del_err_resp["result"]["isError"], Value::Bool(true));
    let del_err_json: Value = serde_json::from_str(
        del_err_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("error text"),
    )
    .expect("MCP error text must parse as JSON");
    assert_eq!(del_err_json["ok"], false);
    assert_eq!(del_err_json["error"]["code"], "error");
    assert!(
        del_err_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("confirm"),
        "{del_err_json}"
    );

    let delete_false_confirm = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "delete_message",
            "arguments": {
                "target": "Engineering/Deployments",
                "msg_id": root_msg.msg_id,
                "confirm": false
            }
        }
    });
    let del_false_resp =
        mcp::handle_rpc_line(&cfg, &graph, &storage, &delete_false_confirm.to_string())
            .await
            .expect("mcp delete_message false rejection");
    assert_eq!(del_false_resp["result"]["isError"], Value::Bool(true));

    // HTTP JSON API (`serve::build_router`)
    let router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        Some("e2e-secret".to_string()),
    );
    let serve_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind serve listener");
    let serve_addr = serve_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(serve_listener, router)
            .await
            .expect("serve HTTP API");
    });

    let http = reqwest::Client::new();
    let serve_base = format!("http://{serve_addr}");

    let health_res = http
        .get(format!("{serve_base}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(health_res.status(), StatusCode::OK);
    let health_json: Value = health_res.json().await.unwrap();
    assert_eq!(health_json["ok"], true);
    assert_eq!(health_json["user_id"], "me-user-1");

    let unauth_res = http
        .get(format!("{serve_base}/conversations"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_res.status(), StatusCode::UNAUTHORIZED);

    let convs_res = http
        .get(format!("{serve_base}/conversations"))
        .bearer_auth("e2e-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(convs_res.status(), StatusCode::OK);
    let convs_json: Value = convs_res.json().await.unwrap();
    assert!(convs_json["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["title"] == "Deployments"));

    let search_res = http
        .get(format!(
            "{serve_base}/search?q={}",
            urlencoding::encode("롤아웃")
        ))
        .bearer_auth("e2e-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(search_res.status(), StatusCode::OK);
    let search_json: Value = search_res.json().await.unwrap();
    assert_eq!(search_json["hits"].as_array().unwrap().len(), 1);

    let thread_res = http
        .get(format!(
            "{serve_base}/conversations/{}/threads/{}",
            urlencoding::encode(&deploy_chan.conv_key),
            urlencoding::encode(&root_msg.msg_id)
        ))
        .bearer_auth("e2e-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(thread_res.status(), StatusCode::OK);
    let thread_json: Value = thread_res.json().await.unwrap();
    assert_eq!(thread_json["thread"]["root"]["msg_id"], root_msg.msg_id);
    assert_eq!(
        thread_json["thread"]["replies"].as_array().unwrap().len(),
        2
    );

    let _: Option<MessageItem> = storage
        .get_message(&deploy_chan.conv_key, &root_msg.msg_id)
        .unwrap();
}


#[tokio::test]
async fn e2e_mock_graph_restricted_chat_read_basic_token_and_403_diagnostics() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create restricted token test tempdir");
    let download_dir = temp.path().join("downloads");

    // Construct JWT with "scp": "User.Read Chat.ReadBasic"
    let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload_json = json!({
        "oid": "me-user-1",
        "name": "Me (Tester)",
        "upn": "tester@contoso.com",
        "scp": "User.Read Chat.ReadBasic",
        "exp": 4_102_444_800_i64
    });
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.to_string().as_bytes());
    let restricted_jwt = format!("{header_b64}.{payload_b64}.mock-signature");

    let diag = diagnose_access_token_scopes(&restricted_jwt);
    assert!(diag.has_scope_claims);
    assert!(diag.chat_read_basic_only);
    assert!(!diag.can_read_chats);
    assert!(
        diag.warnings
            .iter()
            .any(|w| w.contains("Chat.ReadBasic") && w.contains("message bodies")),
        "expected clear Chat.ReadBasic warning, got: {:?}",
        diag.warnings
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 403 mock server");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|| async move {
                Json(json!({
                    "id": "me-user-1",
                    "displayName": "Me (Tester)",
                    "mail": "tester@contoso.com",
                    "userPrincipalName": "tester@contoso.com"
                }))
            }),
        )
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(|| async move {
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": {
                            "code": "Authorization_RequestDenied",
                            "message": "Insufficient privileges to complete the operation."
                        }
                    })),
                )
            }),
        );

    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve 403 mock Graph");
    });

    let (_cfg, graph) = build_direct_graph_client(&base_url, &download_dir, &restricted_jwt);
    let target = ConversationTarget::Chat {
        chat_id: "19:restricted-chat@thread.v2".to_string(),
    };

    let err = graph
        .fetch_older_messages(&target, None, None, 20)
        .await
        .expect_err("403 Forbidden must return Err, not empty Ok");

    let explanation = explain_graph_permission_error(&err.to_string(), Some(&restricted_jwt))
        .expect("explain_graph_permission_error should explain 403");
    assert!(
        explanation.contains(teams_cli::auth::INSUFFICIENT_PERMISSIONS_PREFIX)
            && explanation.contains("Chat.ReadBasic"),
        "expected Chat.ReadBasic explanation, got: {explanation}"
    );

    server.abort();
}

#[tokio::test]
async fn e2e_inline_image_preview_and_self_updater() {
    // 1. Inline Terminal Image Preview: generate in-memory PNG via image::RgbaImage
    let img: RgbaImage = ImageBuffer::from_fn(16, 12, |x, y| {
        Rgba([((x * 16) % 256) as u8, ((y * 20) % 256) as u8, 180, 255])
    });
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .expect("encode RGBA image to PNG");

    let rendered = img_preview::decode_and_render_halfblocks(&png_bytes, 16, 8, (0, 0, 0))
        .expect("decode_and_render_halfblocks");
    assert_eq!(rendered.width_px, 16);
    assert_eq!(rendered.height_px, 12);
    assert_eq!(rendered.cols, 16);
    assert_eq!(rendered.rows, 6);
    assert_eq!(rendered.format_label, "PNG");
    assert_eq!(rendered.lines.len(), 6);
    for line in &rendered.lines {
        assert!(!line.spans.is_empty());
        assert!(line.spans.iter().all(|s| !s.content.is_empty() && s.content.chars().all(|c| c == '▀')));
    }

    let ansi_stdout = img_preview::render_ansi_stdout(&png_bytes, 16, 8)
        .expect("render_ansi_stdout");
    assert!(!ansi_stdout.trim().is_empty());
    assert!(
        ansi_stdout.contains('▀')
            || ansi_stdout.starts_with("\x1b]")
            || ansi_stdout.starts_with("\x1b_G")
            || ansi_stdout.starts_with("\x1bP"),
        "expected valid halfblock, iTerm2, Kitty, or Sixel escape output"
    );

    let halfblock_stdout = img_preview::render_ansi_stdout_with_pref(
        &png_bytes,
        16,
        8,
        ImageProtocolPreference::Halfblock,
    )
    .expect("render_ansi_stdout_with_pref(Halfblock)");
    assert!(halfblock_stdout.contains('▀'));
    assert!(halfblock_stdout.contains("\x1b[38;2;"));

    let kitty_stdout = img_preview::render_ansi_stdout_with_pref(
        &png_bytes,
        16,
        8,
        ImageProtocolPreference::Kitty,
    )
    .expect("render_ansi_stdout_with_pref(Kitty)");
    assert!(kitty_stdout.starts_with("\x1b_Gf=100,a=T,"));
    assert!(kitty_stdout.ends_with("\x1b\\\n"));

    let iterm2_stdout = img_preview::render_ansi_stdout_with_pref(
        &png_bytes,
        16,
        8,
        ImageProtocolPreference::Iterm2,
    )
    .expect("render_ansi_stdout_with_pref(Iterm2)");
    assert!(iterm2_stdout.starts_with("\x1b]1337;File=inline=1;"));
    assert!(iterm2_stdout.ends_with("\x07\n"));

    let sixel_stdout = img_preview::render_ansi_stdout_with_pref(
        &png_bytes,
        16,
        8,
        ImageProtocolPreference::Sixel,
    )
    .expect("render_ansi_stdout_with_pref(Sixel)");
    assert!(sixel_stdout.starts_with("\x1bP0;1;q\"1;1;"));
    assert!(sixel_stdout.ends_with("\x1b\\\n"));

    let sixel_tui = img_preview::decode_and_render_for_protocol(
        &png_bytes,
        img_preview::FitBox::fixed(16, 8),
        (37, 42, 61),
        TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        },
    )
    .expect("decode_and_render_for_protocol(Sixel)");
    assert_eq!(sixel_tui.protocol, TerminalImageProtocol::Sixel);
        assert!(
            sixel_tui
                .pixel_payload
                .as_deref()
                .is_some_and(|p| p.starts_with("\x1bP0;1;q\"1;1;") && p.ends_with("\x1b\\"))
        );

    // 1b. End-to-end mock Graph hostedContents inline screenshot synthesis -> fetch -> Halfblock/Sixel render
    ensure_isolated_env();
    #[derive(Clone)]
    struct HostedImgMockState {
        base_url: String,
        png_bytes: Vec<u8>,
    }
    let hosted_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hostedContents mock Graph server");
    let hosted_addr = hosted_listener.local_addr().unwrap();
    let hosted_base = format!("http://{hosted_addr}");
    let hosted_state = HostedImgMockState {
        base_url: hosted_base.clone(),
        png_bytes: png_bytes.clone(),
    };
    let hosted_app = Router::new()
        .route(
            "/v1.0/chats/c-img/messages",
            get(|State(st): State<HostedImgMockState>, headers: HeaderMap| async move {
                if let Some(resp) = check_bearer_header(&headers, "inline-img-tok") {
                    return resp;
                }
                let img_url = format!(
                    "{}/v1.0/chats/c-img/messages/m-img/hostedContents/hc-1/$value",
                    st.base_url
                );
                Json(json!({
                    "value": [{
                        "id": "m-img",
                        "messageType": "message",
                        "createdDateTime": "2026-11-18T12:00:00Z",
                        "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                        "body": {
                            "contentType": "html",
                            "content": format!("<p>Screenshot:</p><img src=\"{img_url}\">")
                        },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1.0/chats/c-img/messages/m-img/hostedContents/hc-1/$value",
            get(|State(st): State<HostedImgMockState>, headers: HeaderMap| async move {
                if let Some(resp) = check_bearer_header(&headers, "inline-img-tok") {
                    return resp;
                }
                (StatusCode::OK, st.png_bytes).into_response()
            }),
        )
        .with_state(hosted_state);

    let hosted_server = tokio::spawn(async move {
        axum::serve(hosted_listener, hosted_app)
            .await
            .expect("serve hostedContents mock Graph");
    });

    let hosted_tmp = tempfile::tempdir().expect("hosted tmp");
    let hosted_cfg = build_test_config(&hosted_base, hosted_tmp.path());
    let hosted_tokens = StoredTokens {
        access_token: "inline-img-tok".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: hosted_cfg.client_id.clone(),
        tenant_id: hosted_cfg.tenant_id.clone(),
        user_id: Some("me-user-1".to_string()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".to_string(),
    };
    let hosted_auth = AuthManager::from_tokens(hosted_cfg.clone(), hosted_tokens);
    let hosted_graph = GraphClient::new(hosted_cfg, hosted_auth).expect("hosted GraphClient");

    let fetched_msgs = hosted_graph
        .list_chat_messages("c-img", 10)
        .await
        .expect("list_chat_messages with inline hostedContents img");
    assert_eq!(fetched_msgs.len(), 1);
    let eff_atts = fetched_msgs[0].effective_attachments();
    assert_eq!(eff_atts.len(), 1);
    assert_eq!(eff_atts[0].id, "hosted:hc-1");
    assert_eq!(eff_atts[0].content_type, "hostedContent");

    let (_dl_name, _dl_ct, dl_bytes) = hosted_graph
        .fetch_attachment_bytes(&eff_atts[0])
        .await
        .expect("fetch_attachment_bytes for hosted:hc-1");
    assert_eq!(dl_bytes, png_bytes);

    let hosted_halfblock = img_preview::decode_and_render_halfblocks(&dl_bytes, 16, 8, (0, 0, 0))
        .expect("render fetched hosted:hc-1 halfblocks");
    assert!(!hosted_halfblock.lines.is_empty());

    let hosted_sixel = img_preview::render_ansi_stdout_with_pref(
        &dl_bytes,
        16,
        8,
        ImageProtocolPreference::Sixel,
    )
    .expect("render fetched hosted:hc-1 Sixel");
    assert!(hosted_sixel.starts_with("\x1bP0;1;q\"1;1;"));
    assert!(hosted_sixel.ends_with("\x1b\\\n"));

    hosted_server.abort();

    // 2. Self-Updater E2E against mock GitHub Releases server
    #[derive(Clone)]
    struct MockGhState {
        base_url: String,
        asset_name: String,
        asset_bytes: Vec<u8>,
        sums_text: String,
    }

    let asset_name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "teams.exe".to_string(),
        ("windows", "aarch64") => "teams-9.9.9-aarch64-pc-windows-msvc.zip".to_string(),
        ("macos", "x86_64") => "teams-9.9.9-x86_64-apple-darwin.tar.gz".to_string(),
        ("macos", "aarch64") => "teams-9.9.9-aarch64-apple-darwin.tar.gz".to_string(),
        ("linux", "x86_64") => "teams-9.9.9-x86_64-unknown-linux-gnu.tar.gz".to_string(),
        ("linux", "aarch64") => "teams-9.9.9-aarch64-unknown-linux-gnu.tar.gz".to_string(),
        _ => "teams".to_string(),
    };
    let asset_bytes = b"MOCK-RELEASE-BINARY-V9.9.9".to_vec();
    let digest = teams_cli::hex_encode(Sha256::digest(&asset_bytes));
    let sums_text = format!("{digest}  {asset_name}\n");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock GitHub server");
    let addr = listener.local_addr().unwrap();
    let gh_base = format!("http://{addr}");

    let gh_state = MockGhState {
        base_url: gh_base.clone(),
        asset_name: asset_name.clone(),
        asset_bytes,
        sums_text,
    };

    let gh_app = Router::new()
        .route(
            "/repos/BiteFoundry/teams/releases/latest",
            get(|State(st): State<MockGhState>| async move {
                Json(json!({
                    "tag_name": "v9.9.9",
                    "html_url": format!("{}/BiteFoundry/teams/releases/tag/v9.9.9", st.base_url),
                    "assets": [
                        {
                            "name": st.asset_name,
                            "size": st.asset_bytes.len() as u64,
                            "url": format!("{}/assets/binary", st.base_url),
                            "browser_download_url": format!("{}/download/{}", st.base_url, st.asset_name)
                        },
                        {
                            "name": "SHA256SUMS.txt",
                            "size": st.sums_text.len() as u64,
                            "url": format!("{}/assets/sums", st.base_url),
                            "browser_download_url": format!("{}/download/SHA256SUMS.txt", st.base_url)
                        }
                    ]
                }))
            }),
        )
        .route(
            "/assets/sums",
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.sums_text) }),
        )
        .route(
            "/download/SHA256SUMS.txt",
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.sums_text) }),
        )
        .route(
            "/assets/binary",
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.asset_bytes) }),
        )
        .route(
            &format!("/download/{asset_name}"),
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.asset_bytes) }),
        )
        .with_state(gh_state);

    let gh_server = tokio::spawn(async move {
        axum::serve(listener, gh_app)
            .await
            .expect("serve mock GitHub");
    });

    let result = updater::check_or_apply_update_with_api_base(
        &gh_base,
        "BiteFoundry/teams",
        true,
        false,
    )
    .await
    .expect("check_or_apply_update_with_api_base");

    assert_eq!(result.current_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(result.latest_version, "9.9.9");
    assert!(result.update_available);
    assert!(!result.updated);
    assert_eq!(result.asset_name.as_deref(), Some(asset_name.as_str()));
    assert!(result.release_url.contains("v9.9.9"));

    gh_server.abort();
}
