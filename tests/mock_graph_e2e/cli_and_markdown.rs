//! Built CLI binary hyphen inputs, Markdown lists/links/mentions, quote-replies, partial edits, and FTS5 search E2E tests.

use super::server::*;
use tempfile::TempDir;
use std::{collections::HashMap, sync::{Arc, RwLock}};
use axum::{response::IntoResponse, extract::{Path, State}, http::StatusCode, routing::{get, post}, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use teams_cli::{auth::{AuthManager, StoredTokens}, config::AppConfig, graph::GraphClient, mcp, models::{ConversationTarget, MessageItem}, serve, storage::Storage};

#[tokio::test]
async fn e2e_built_cli_hyphen_inputs_markdown_lists_and_partial_edit_mock_graph() {
    #[derive(Default)]
    struct CliMockState {
        created_chats: RwLock<Vec<Value>>,
        updated_chat_topics: RwLock<Vec<(String, Value)>>,
        posted_chat_messages: RwLock<Vec<(String, Value)>>,
        posted_channel_messages: RwLock<Vec<(String, String, Value)>>,
        posted_channel_replies: RwLock<Vec<(String, String, String, Value)>>,
        patched_chat_messages: RwLock<Vec<(String, String, Value)>>,
        chat_messages: RwLock<HashMap<(String, String), String>>,
        next_id: RwLock<u64>,
    }

    impl CliMockState {
        fn next_msg_id(&self, prefix: &str) -> String {
            let mut seq = self.next_id.write().unwrap();
            *seq += 1;
            format!("{prefix}-{}", *seq)
        }

        fn total_writes(&self) -> usize {
            self.created_chats.read().unwrap().len()
                + self.updated_chat_topics.read().unwrap().len()
                + self.posted_chat_messages.read().unwrap().len()
                + self.posted_channel_messages.read().unwrap().len()
                + self.posted_channel_replies.read().unwrap().len()
                + self.patched_chat_messages.read().unwrap().len()
        }
    }

    let state = Arc::new(CliMockState::default());
    {
        let mut msgs = state.chat_messages.write().unwrap();
        msgs.insert(
            ("19:cli-chat@thread.v2".to_string(), "m-unique".to_string()),
            "<p><a href=\"keep\">ref</a> <b>alpha</b> beta</p>".to_string(),
        );
        msgs.insert(
            ("19:cli-chat@thread.v2".to_string(), "m-hyphen".to_string()),
            "<p><a href=\"keep\">ref</a> <b>- 목록 항목</b> tail</p>".to_string(),
        );
        msgs.insert(
            ("19:cli-chat@thread.v2".to_string(), "m-split".to_string()),
            "<p>alpha <b>beta</b></p>".to_string(),
        );
        msgs.insert(
            ("19:cli-chat@thread.v2".to_string(), "m-both".to_string()),
            "<p>alpha beta / alpha <b>beta</b></p>".to_string(),
        );
        msgs.insert(
            ("19:cli-chat@thread.v2".to_string(), "m-orig".to_string()),
            "<p>Original message</p>".to_string(),
        );
    }

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
            "/v1.0/me/chats",
            get(|| async move {
                Json(json!({
                    "value": [{
                        "id": "19:cli-chat@thread.v2",
                        "chatType": "group",
                        "topic": "CLI Test Chat",
                        "lastUpdatedDateTime": "2026-11-21T05:00:00Z",
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "peer-kim", "displayName": "Alice" }
                        ]
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async move { Json(json!({ "value": mock_teams_payload() })) }),
        )
        .route(
            "/v1.0/$batch",
            post(|Json(payload): Json<Value>| async move {
                let reqs = payload
                    .get("requests")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let responses: Vec<Value> = reqs
                    .into_iter()
                    .map(|req| {
                        let req_id = req.get("id").and_then(|v| v.as_str()).unwrap_or("0");
                        let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("")
                            // Channel listings are requested from `/allChannels`; answer them like `/channels`.
                            .replace("/allChannels", "/channels");
                        if url.contains("/teams/team-dev/channels") {
                            json!({
                                "id": req_id,
                                "status": 200,
                                "body": { "value": mock_channels_payload() }
                            })
                        } else {
                            json!({
                                "id": req_id,
                                "status": 404,
                                "body": { "error": { "code": "NotFound", "message": "not found" } }
                            })
                        }
                    })
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/chats",
            post(|State(st): State<Arc<CliMockState>>, Json(payload): Json<Value>| async move {
                st.created_chats.write().unwrap().push(payload.clone());
                let topic = payload
                    .get("topic")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let chat_type = payload
                    .get("chatType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("group");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": "19:created-chat@thread.v2",
                        "chatType": chat_type,
                        "topic": topic,
                        "lastUpdatedDateTime": "2026-11-21T05:00:00Z",
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "peer-kim", "displayName": "Alice" }
                        ]
                    })),
                )
            }),
        )
        .route(
            "/v1.0/chats/{chat_id}",
            get(|Path(chat_id): Path<String>| async move {
                Json(json!({
                    "id": chat_id,
                    "chatType": "group",
                    "topic": "Old Topic",
                    "members": [
                        { "userId": "me-user-1", "displayName": "Me (Tester)" },
                        { "userId": "peer-kim", "displayName": "Alice" }
                    ]
                }))
            })
            .patch(
                |State(st): State<Arc<CliMockState>>,
                 Path(chat_id): Path<String>,
                 Json(payload): Json<Value>| async move {
                    st.updated_chat_topics
                        .write()
                        .unwrap()
                        .push((chat_id.clone(), payload.clone()));
                    let topic = payload["topic"].as_str().unwrap_or("");
                    Json(json!({
                        "id": chat_id,
                        "chatType": "group",
                        "topic": topic,
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "peer-kim", "displayName": "Alice" }
                        ]
                    }))
                },
            ),
        )
        .route(
            "/v1.0/chats/{chat_id}/messages",
            post(
                |State(st): State<Arc<CliMockState>>,
                 Path(chat_id): Path<String>,
                 Json(payload): Json<Value>| async move {
                    st.posted_chat_messages
                        .write()
                        .unwrap()
                        .push((chat_id.clone(), payload.clone()));
                    let id = st.next_msg_id("chat-msg");
                    let content = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    st.chat_messages
                        .write()
                        .unwrap()
                        .insert((chat_id, id.clone()), content.clone());
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": id,
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T05:10:00Z",
                            "subject": payload.get("subject").cloned().unwrap_or(Value::Null),
                            "importance": payload.get("importance").cloned().unwrap_or(json!("normal")),
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": content },
                            "mentions": payload.get("mentions").cloned().unwrap_or(json!([])),
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .route(
            "/v1.0/chats/{chat_id}/messages/{msg_id}",
            get(
                |State(st): State<Arc<CliMockState>>,
                 Path((chat_id, msg_id)): Path<(String, String)>| async move {
                    let guard = st.chat_messages.read().unwrap();
                    let Some(content) = guard.get(&(chat_id, msg_id.clone())).cloned() else {
                        return StatusCode::NOT_FOUND.into_response();
                    };
                    Json(json!({
                        "id": msg_id,
                        "messageType": "message",
                        "createdDateTime": "2026-11-21T05:10:00Z",
                        "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                        "body": { "contentType": "html", "content": content },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }))
                    .into_response()
                },
            )
            .patch(
                |State(st): State<Arc<CliMockState>>,
                 Path((chat_id, msg_id)): Path<(String, String)>,
                 Json(payload): Json<Value>| async move {
                    st.patched_chat_messages.write().unwrap().push((
                        chat_id.clone(),
                        msg_id.clone(),
                        payload.clone(),
                    ));
                    if let Some(content) =
                        payload.pointer("/body/content").and_then(|v| v.as_str())
                    {
                        st.chat_messages
                            .write()
                            .unwrap()
                            .insert((chat_id, msg_id), content.to_string());
                    }
                    StatusCode::NO_CONTENT.into_response()
                },
            ),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            post(
                |State(st): State<Arc<CliMockState>>,
                 Path((team_id, channel_id)): Path<(String, String)>,
                 Json(payload): Json<Value>| async move {
                    st.posted_channel_messages.write().unwrap().push((
                        team_id,
                        channel_id,
                        payload.clone(),
                    ));
                    let id = st.next_msg_id("chan-root");
                    let content = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": id,
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T05:11:00Z",
                            "subject": payload.get("subject").cloned().unwrap_or(Value::Null),
                            "importance": "normal",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": content },
                            "mentions": payload.get("mentions").cloned().unwrap_or(json!([])),
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}/replies",
            post(
                |State(st): State<Arc<CliMockState>>,
                 Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
                 Json(payload): Json<Value>| async move {
                    st.posted_channel_replies.write().unwrap().push((
                        team_id,
                        channel_id,
                        root_id.clone(),
                        payload.clone(),
                    ));
                    let id = st.next_msg_id("chan-reply");
                    let content = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": id,
                            "replyToId": root_id,
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T05:12:00Z",
                            "importance": "normal",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": content },
                            "mentions": payload.get("mentions").cloned().unwrap_or(json!([])),
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind built-CLI mock Graph server");
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve built-CLI mock Graph");
    });

    let tmp = tempfile::tempdir().expect("create built-CLI test tempdir");
    let cfg_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");
    let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload_b64 = URL_SAFE_NO_PAD.encode(
        json!({
            "oid": "me-user-1",
            "name": "Me (Tester)",
            "upn": "tester@contoso.com",
            "exp": 4_102_444_800_i64
        })
        .to_string()
        .as_bytes(),
    );
    let jwt = format!("{header_b64}.{payload_b64}.sig");
    let graph_base = format!("http://{addr}/v1.0");
    let auth_base = format!("http://{addr}");

    let run_cli = |args: &[&str]| {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let graph_base = graph_base.clone();
        let auth_base = auth_base.clone();
        let jwt = jwt.clone();
        let cfg_dir = cfg_dir.clone();
        let data_dir = data_dir.clone();
        async move {
            tokio::process::Command::new(env!("CARGO_BIN_EXE_teams"))
                .args(&owned)
                .env("TEAMS_GRAPH_BASE_URL", &graph_base)
                .env("TEAMS_AUTH_BASE_URL", &auth_base)
                .env("TEAMS_ACCESS_TOKEN", &jwt)
                .env("TEAMS_DISABLE_KEYRING", "1")
                .env("TEAMS_CONFIG_DIR", &cfg_dir)
                .env("TEAMS_DATA_DIR", &data_dir)
                .env_remove("TEAMS_TOKEN_FILE")
                .env_remove("TEAMS_TOKEN_REFRESH_CMD")
                .env_remove("TEAMS_AUTO_DAEMON")
                .output()
                .await
                .expect("run built teams CLI")
        }
    };

    // 1. Invalid flags / missing option values / help MUST execute 0 Graph writes
    for bad_args in [
        &["send", "chat:19:cli-chat@thread.v2", "--json"][..],
        &["send", "chat:19:cli-chat@thread.v2", "--dry-run"][..],
        &["send", "chat:19:cli-chat@thread.v2", "body", "--subject", "--json"][..],
        &["send", "chat:19:cli-chat@thread.v2", "body", "--subject", "--dry-run"][..],
        &["edit", "chat:19:cli-chat@thread.v2", "m-orig", "--dry-run"][..],
        &["edit", "chat:19:cli-chat@thread.v2", "m-orig", "--find", "old", "--replace", "--json"][..],
        &["edit", "chat:19:cli-chat@thread.v2", "m-orig", "--find", "old", "--replace", "--dry-run"][..],
        &["update-chat", "chat:19:cli-chat@thread.v2", "--topic", "--json"][..],
        &["new-chat", "--to", "peer@example.invalid", "--message", "--json"][..],
    ] {
        let out = run_cli(bad_args).await;
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected clap error exit 2 for {bad_args:?}, got {:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let help_out = run_cli(&["send", "chat:19:cli-chat@thread.v2", "- 목록 항목", "--help"]).await;
    assert!(help_out.status.success(), "{:?}", help_out.stderr);
    let help_stdout = String::from_utf8_lossy(&help_out.stdout);
    assert!(
        help_stdout.contains("Send a Markdown message") && help_stdout.contains("Usage:"),
        "unexpected help output: {help_stdout}"
    );
    let short_help_out = run_cli(&["send", "chat:19:cli-chat@thread.v2", "-h"]).await;
    assert!(short_help_out.status.success(), "{:?}", short_help_out.stderr);
    assert!(String::from_utf8_lossy(&short_help_out.stdout).contains("Usage:"));
    let subject_help_out = run_cli(&["send", "chat:19:cli-chat@thread.v2", "body", "--subject", "--help"]).await;
    assert!(
        subject_help_out.status.success() || subject_help_out.status.code() == Some(2),
        "unexpected status for --subject --help: {:?}",
        subject_help_out.status
    );
    assert_eq!(state.total_writes(), 0, "failed parses and --help must perform 0 writes");

    // 2. Chat send: two-line list + mention + trailing --json
    let out = run_cli(&[
        "send",
        "chat:19:cli-chat@thread.v2",
        "- first @Alice\n- second",
        "--mention",
        "peer-kim:Alice",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let chat_send_payload = state.posted_chat_messages.read().unwrap().last().unwrap().1.clone();
    assert_eq!(
        chat_send_payload["body"]["content"],
        "<ul><li>first <at id=\"0\">Alice</at></li><li>second</li></ul>"
    );
    assert_eq!(chat_send_payload["mentions"][0]["mentioned"]["user"]["id"], "peer-kim");

    // 3. Chat send: blank line ends list + numbered text keeps '7.' + fenced code block preserves literal hyphens
    let out = run_cli(&[
        "send",
        "chat:19:cli-chat@thread.v2",
        "- 목록 항목\n- 둘째 항목\n\n7. keep seven\n8. keep eight\n\n```\n- code line 1\n- code line 2\n```",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let code_payload = state.posted_chat_messages.read().unwrap().last().unwrap().1.clone();
    assert_eq!(
        code_payload["body"]["content"],
        "<ul><li>목록 항목</li><li>둘째 항목</li></ul><p>7. keep seven<br>8. keep eight</p><pre><code>- code line 1\n- code line 2</code></pre>"
    );

    // 4. Channel send: two-line list + hyphenated --subject + mention
    let out = run_cli(&[
        "send",
        "chan:team-dev:chan-deploy",
        "- first @Alice\n- second",
        "--subject",
        "- 공지 제목",
        "--mention",
        "peer-kim:Alice",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let chan_send_payload = state
        .posted_channel_messages
        .read()
        .unwrap()
        .last()
        .unwrap()
        .2
        .clone();
    assert_eq!(chan_send_payload["subject"], "- 공지 제목");
    assert_eq!(
        chan_send_payload["body"]["content"],
        "<ul><li>first <at id=\"0\">Alice</at></li><li>second</li></ul>"
    );
    assert_eq!(chan_send_payload["mentions"][0]["mentioned"]["user"]["id"], "peer-kim");

    // 5. Chat reply & Channel reply: one <ul>, two <li> in order, and @mention
    let out = run_cli(&[
        "reply",
        "chat:19:cli-chat@thread.v2",
        "m-orig",
        "- first @Alice\n- second",
        "--mention",
        "peer-kim:Alice",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let chat_reply_payload = state.posted_chat_messages.read().unwrap().last().unwrap().1.clone();
    let chat_reply_html = chat_reply_payload["body"]["content"].as_str().unwrap();
    assert_eq!(chat_reply_html.matches("<ul>").count(), 1, "{chat_reply_html}");
    assert_eq!(chat_reply_html.matches("<li>").count(), 2, "{chat_reply_html}");
    assert!(
        chat_reply_html.ends_with("<ul><li>first <at id=\"0\">Alice</at></li><li>second</li></ul>"),
        "{chat_reply_html}"
    );

    let out = run_cli(&[
        "reply",
        "chan:team-dev:chan-deploy",
        "chan-root-1",
        "- first @Alice\n- second",
        "--mention",
        "peer-kim:Alice",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let chan_reply_payload = state
        .posted_channel_replies
        .read()
        .unwrap()
        .last()
        .unwrap()
        .3
        .clone();
    assert_eq!(
        chan_reply_payload["body"]["content"],
        "<ul><li>first <at id=\"0\">Alice</at></li><li>second</li></ul>"
    );
    assert_eq!(chan_reply_payload["mentions"][0]["mentioned"]["user"]["id"], "peer-kim");

    // 6. Explicit `--flag=value` and positional `--` send literal `--dry-run`
    let out = run_cli(&[
        "send",
        "chan:team-dev:chan-deploy",
        "--subject=--dry-run",
        "--",
        "--dry-run",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let literal_payload = state
        .posted_channel_messages
        .read()
        .unwrap()
        .last()
        .unwrap()
        .2
        .clone();
    assert_eq!(literal_payload["subject"], "--dry-run");
    assert_eq!(literal_payload["body"]["content"], "<p>--dry-run</p>");

    // 7. new-chat --topic & --message, and update-chat --topic
    let out = run_cli(&[
        "new-chat",
        "--to",
        "audit@example.invalid",
        "--topic",
        "- 새 주제",
        "--message",
        "- first\n- second",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let created_chat = state.created_chats.read().unwrap().last().unwrap().clone();
    assert_eq!(created_chat["topic"], "- 새 주제");
    let new_chat_msg = state.posted_chat_messages.read().unwrap().last().unwrap().clone();
    assert_eq!(new_chat_msg.0, "19:created-chat@thread.v2");
    assert_eq!(
        new_chat_msg.1["body"]["content"],
        "<ul><li>first</li><li>second</li></ul>"
    );

    let out = run_cli(&[
        "update-chat",
        "chat:19:cli-chat@thread.v2",
        "--topic",
        "- 목록 항목",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let updated_topic = state
        .updated_chat_topics
        .read()
        .unwrap()
        .last()
        .unwrap()
        .1
        .clone();
    assert_eq!(updated_topic, json!({ "topic": "- 목록 항목" }));

    // 8. Full body edit with hyphen list + partial replace control comparison (layout.json)
    let out = run_cli(&[
        "edit",
        "chat:19:cli-chat@thread.v2",
        "m-orig",
        "- first\n- second",
        "--json",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let full_edit_patch = state
        .patched_chat_messages
        .read()
        .unwrap()
        .last()
        .unwrap()
        .2
        .clone();
    assert_eq!(
        full_edit_patch["body"]["content"],
        "<ul><li>first</li><li>second</li></ul>"
    );

    // Unique partial replace preserves link and bold markup
    let out = run_cli(&[
        "edit",
        "chat:19:cli-chat@thread.v2",
        "m-unique",
        "--find",
        "alpha",
        "--replace",
        "NEW",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let unique_patch = state
        .patched_chat_messages
        .read()
        .unwrap()
        .last()
        .unwrap()
        .2
        .clone();
    assert_eq!(
        unique_patch["body"]["content"],
        "<p><a href=\"keep\">ref</a> <b>NEW</b> beta</p>"
    );

    // Hyphenated --find / --replace preserves surrounding link and bold markup
    let out = run_cli(&[
        "edit",
        "chat:19:cli-chat@thread.v2",
        "m-hyphen",
        "--find",
        "- 목록 항목",
        "--replace",
        "- 완료 항목",
    ])
    .await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let hyphen_patch = state
        .patched_chat_messages
        .read()
        .unwrap()
        .last()
        .unwrap()
        .2
        .clone();
    assert_eq!(
        hyphen_patch["body"]["content"],
        "<p><a href=\"keep\">ref</a> <b>- 완료 항목</b> tail</p>"
    );

    // Split and duplicate matches MUST fail with 0 writes
    let patches_before_reject = state.patched_chat_messages.read().unwrap().len();
    let split_out = run_cli(&[
        "edit",
        "chat:19:cli-chat@thread.v2",
        "m-split",
        "--find",
        "alpha beta",
        "--replace",
        "NEW",
    ])
    .await;
    assert!(!split_out.status.success());
    let both_out = run_cli(&[
        "edit",
        "chat:19:cli-chat@thread.v2",
        "m-both",
        "--find",
        "alpha beta",
        "--replace",
        "NEW",
    ])
    .await;
    assert!(!both_out.status.success());
    assert_eq!(
        state.patched_chat_messages.read().unwrap().len(),
        patches_before_reject,
        "split/duplicate partial edits must write 0 PATCH requests"
    );

    // 9. Local cache search with hyphen query finds the previously sent message
    let search_out = run_cli(&["search", "- 목록 항목", "--json"]).await;
    assert!(
        search_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&search_out.stderr)
    );
    let search_json: Value = serde_json::from_slice(&search_out.stdout).expect("parse search json");
    assert_eq!(search_json["query"], "- 목록 항목");
    assert!(
        !search_json["hits"].as_array().unwrap().is_empty(),
        "expected cached search hit for '- 목록 항목', got {search_json}"
    );

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_01_single_quote_reply_blockquote() {
    #[derive(Default)]
    struct Bug01State {
        posted_chat_bodies: RwLock<Vec<String>>,
        posted_channel_reply_bodies: RwLock<Vec<(String, String)>>,
    }

    let state = Arc::new(Bug01State::default());

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|| async {
                Json(json!({
                    "id": "me-user-1",
                    "displayName": "Me (Tester)",
                    "mail": "tester@contoso.com",
                    "userPrincipalName": "tester@contoso.com"
                }))
            }),
        )
        .route(
            "/v1.0/me/chats",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "19:q@thread.v2",
                        "chatType": "group",
                        "topic": "QuoteChat",
                        "lastUpdatedDateTime": "2026-11-21T09:00:05Z",
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "peer-kim", "displayName": "Alice" }
                        ]
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async {
                Json(json!({ "value": [{ "id": "t1", "displayName": "Eng" }] }))
            }),
        )
        .route(
            "/v1.0/$batch",
            post(|Json(payload): Json<Value>| async move {
                let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                let responses: Vec<Value> = reqs
                    .into_iter()
                    .map(|req| {
                        json!({
                            "id": req["id"].as_str().unwrap_or("0"),
                            "status": 200,
                            "body": { "value": [{ "id": "ch1", "displayName": "General", "membershipType": "standard" }] }
                        })
                    })
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "m-orig",
                        "messageType": "message",
                        "createdDateTime": "2026-11-21T09:00:01Z",
                        "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                        "body": { "contentType": "html", "content": "<p>Original message text</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }]
                }))
            })
            .post(
                |State(st): State<Arc<Bug01State>>, Json(payload): Json<Value>| async move {
                    let html = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    st.posted_chat_bodies.write().unwrap().push(html.clone());
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "m-reply-new",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T09:01:00Z",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": html },
                            "mentions": [],
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .route(
            "/v1.0/chats/{chat_id}/messages/{msg_id}",
            get(|Path((_chat_id, msg_id)): Path<(String, String)>| async move {
                if msg_id == "nonexistent-msg-999" {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({
                            "error": { "code": "NotFound", "message": "Message not found" }
                        })),
                    )
                        .into_response();
                }
                Json(json!({
                    "id": msg_id,
                    "messageType": "message",
                    "createdDateTime": "2026-11-21T08:00:00Z",
                    "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                    "body": { "contentType": "html", "content": "<p>Older single-fetch message</p>" },
                    "mentions": [],
                    "attachments": [],
                    "reactions": []
                }))
                .into_response()
            }),
        )
        .route(
            "/v1.0/teams/t1/channels/ch1/messages/{root_id}/replies",
            post(
                |State(st): State<Arc<Bug01State>>,
                 Path(root_id): Path<String>,
                 Json(payload): Json<Value>| async move {
                    let html = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    st.posted_channel_reply_bodies
                        .write()
                        .unwrap()
                        .push((root_id.clone(), html.clone()));
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "ch-rep-1",
                            "replyToId": root_id,
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T09:02:00Z",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": html },
                            "mentions": [],
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let graph_base = format!("http://{addr}/v1.0");
    let auth_base = format!("http://{addr}");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload_b64 = URL_SAFE_NO_PAD.encode(
        json!({
            "oid": "me-user-1",
            "name": "Me (Tester)",
            "upn": "tester@contoso.com",
            "exp": 4_102_444_800_i64
        })
        .to_string()
        .as_bytes(),
    );
    let jwt = format!("{header_b64}.{payload_b64}.sig");

    let run_cli = |tmp: &TempDir, args: &[&str]| {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let graph_base = graph_base.clone();
        let auth_base = auth_base.clone();
        let jwt = jwt.clone();
        let cfg_dir = tmp.path().join("config");
        let data_dir = tmp.path().join("data");
        async move {
            tokio::process::Command::new(env!("CARGO_BIN_EXE_teams"))
                .args(&owned)
                .env("TEAMS_GRAPH_BASE_URL", &graph_base)
                .env("TEAMS_AUTH_BASE_URL", &auth_base)
                .env("TEAMS_ACCESS_TOKEN", &jwt)
                .env("TEAMS_DISABLE_KEYRING", "1")
                .env("TEAMS_CONFIG_DIR", &cfg_dir)
                .env("TEAMS_DATA_DIR", &data_dir)
                .env("TEAMS_AUTO_DAEMON", "0")
                .env_remove("TEAMS_TOKEN_FILE")
                .env_remove("TEAMS_TOKEN_REFRESH_CMD")
                .output()
                .await
                .expect("run teams CLI")
        }
    };

    let assert_single_reply_bq = |html: &str, expected_id: &str, expected_body: &str, ctx: &str| {
        assert_eq!(
            html.matches("itemtype=\"http://schema.skype.com/Reply\"").count(),
            1,
            "{ctx}: expected exactly 1 Skype Reply blockquote, got: {html}"
        );
        assert!(
            html.contains(&format!("itemid=\"{expected_id}\"")),
            "{ctx}: missing itemid={expected_id} in {html}"
        );
        assert_eq!(
            html.matches(expected_body).count(),
            1,
            "{ctx}: expected body once in {html}"
        );
    };

    // 1. CLI cached & uncached (both in-window and single-message fallback outside window)
    let tmp_cached = tempfile::tempdir().unwrap();
    assert!(run_cli(&tmp_cached, &["read", "QuoteChat", "--json"]).await.status.success());
    let out = run_cli(&tmp_cached, &["reply", "QuoteChat", "m-orig", "CLI cached body", "--json"]).await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let html = state.posted_chat_bodies.read().unwrap().last().unwrap().clone();
    assert_single_reply_bq(&html, "m-orig", "CLI cached body", "CLI cached");

    // User-written markdown blockquote in reply preserves both Skype Reply + user blockquote
    let out_user_bq = run_cli(
        &tmp_cached,
        &["reply", "QuoteChat", "m-orig", "> user quote line\n\nMy response", "--json"],
    )
    .await;
    assert!(out_user_bq.status.success());
    let html_user_bq = state.posted_chat_bodies.read().unwrap().last().unwrap().clone();
    assert_single_reply_bq(&html_user_bq, "m-orig", "My response", "CLI with user blockquote");
    assert!(
        html_user_bq.contains("<blockquote>user quote line</blockquote>"),
        "user markdown blockquote must be preserved: {html_user_bq}"
    );

    let tmp_uncached = tempfile::tempdir().unwrap();
    let out_u1 = run_cli(&tmp_uncached, &["reply", "QuoteChat", "m-orig", "CLI uncached body", "--json"]).await;
    assert!(out_u1.status.success());
    let html_u1 = state.posted_chat_bodies.read().unwrap().last().unwrap().clone();
    assert_single_reply_bq(&html_u1, "m-orig", "CLI uncached body", "CLI uncached");

    let out_u2 = run_cli(&tmp_uncached, &["reply", "QuoteChat", "m-older-99", "CLI fallback body", "--json"]).await;
    assert!(out_u2.status.success());
    let html_u2 = state.posted_chat_bodies.read().unwrap().last().unwrap().clone();
    assert_single_reply_bq(&html_u2, "m-older-99", "CLI fallback body", "CLI single-GET fallback");

    // 2. MCP cached & uncached
    let cfg = AppConfig {
        graph_base_url: graph_base.clone(),
        auth_base_url: auth_base.clone(),
        ..Default::default()
    };
    let tokens = StoredTokens {
        access_token: jwt.clone(),
        refresh_token: None,
        expires_at: 4_102_444_800,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("me-user-1".into()),
        user_display_name: Some("Me (Tester)".into()),
        user_email: Some("tester@contoso.com".into()),
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth).unwrap().with_my_user_id("me-user-1");

    for (label, warm_cache) in [("MCP uncached", false), ("MCP cached", true)] {
        let storage = Storage::open_in_memory().unwrap();
        if warm_cache {
            let chats = graph.list_chats(10).await.unwrap();
            storage.upsert_conversations(&chats).unwrap();
            let msgs = graph.list_messages(&chats[0].target, 10).await.unwrap();
            storage.upsert_messages(&msgs).unwrap();
        }
        let body_text = format!("{label} reply");
        let call_line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "reply_message",
                "arguments": {
                    "target": "QuoteChat",
                    "msg_id": "m-orig",
                    "markdown": body_text
                }
            }
        }))
        .unwrap();
        let resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &call_line)
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{label}: {resp}");
        let html_mcp = state.posted_chat_bodies.read().unwrap().last().unwrap().clone();
        assert_single_reply_bq(&html_mcp, "m-orig", &body_text, label);
    }

    // 3. HTTP cached & uncached
    for (label, warm_cache) in [("HTTP uncached", false), ("HTTP cached", true)] {
        let storage = Storage::open_in_memory().unwrap();
        if warm_cache {
            let chats = graph.list_chats(10).await.unwrap();
            storage.upsert_conversations(&chats).unwrap();
            let msgs = graph.list_messages(&chats[0].target, 10).await.unwrap();
            storage.upsert_messages(&msgs).unwrap();
        }
        let api_router = serve::build_router(
            cfg.clone(),
            graph.clone(),
            storage,
            Some("http-test-tok".to_string()),
        );
        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api_listener.local_addr().unwrap();
        let api_task = tokio::spawn(async move {
            axum::serve(api_listener, api_router).await.unwrap();
        });
        let body_text = format!("{label} reply");
        let resp = reqwest::Client::new()
            .post(format!(
                "http://{api_addr}/conversations/{}/messages/m-orig/replies",
                urlencoding::encode("chat:19:q@thread.v2")
            ))
            .bearer_auth("http-test-tok")
            .json(&json!({ "markdown": body_text }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "{label}");
        let html_http = state.posted_chat_bodies.read().unwrap().last().unwrap().clone();
        assert_single_reply_bq(&html_http, "m-orig", &body_text, label);
        api_task.abort();
    }

    // Regression control: Channel thread reply uses /replies endpoint with 0 Skype Reply blockquotes
    let chan_rep = run_cli(&tmp_cached, &["reply", "Eng/General", "root-1", "Channel reply text", "--json"]).await;
    assert!(chan_rep.status.success(), "stderr={}", String::from_utf8_lossy(&chan_rep.stderr));
    {
        let chan_posts = state.posted_channel_reply_bodies.read().unwrap();
        assert_eq!(chan_posts.len(), 1);
        assert_eq!(chan_posts[0].0, "root-1");
        assert_eq!(chan_posts[0].1, "<p>Channel reply text</p>");
    }

    // W2: Replying to a nonexistent chat message ID fails without posting an empty quote reply
    let posts_before = state.posted_chat_bodies.read().unwrap().len();
    let missing_rep = run_cli(
        &tmp_uncached,
        &["reply", "QuoteChat", "nonexistent-msg-999", "Should not post", "--json"],
    )
    .await;
    assert!(
        !missing_rep.status.success(),
        "reply to missing chat message must fail; stdout={}",
        String::from_utf8_lossy(&missing_rep.stdout)
    );
    assert_eq!(
        state.posted_chat_bodies.read().unwrap().len(),
        posts_before,
        "missing chat reply target must not POST"
    );
    let missing_stderr = String::from_utf8_lossy(&missing_rep.stderr);
    assert!(
        missing_stderr.contains("Cannot reply: message nonexistent-msg-999 was not found in this chat"),
        "unexpected stderr: {missing_stderr}"
    );
    let missing_json: Value =
        serde_json::from_slice(&missing_rep.stdout).expect("missing reply --json error stdout");
    assert_eq!(missing_json["ok"], false);
    assert_eq!(missing_json["error"]["code"], "not_found");
    assert_eq!(missing_json["error"]["http_status"], 404);

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_10_markdown_links_and_mentions() {
    #[derive(Default)]
    struct Bug10State {
        posted: RwLock<Vec<Value>>,
    }

    let state = Arc::new(Bug10State::default());

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|| async {
                Json(json!({
                    "id": "me-user-1",
                    "displayName": "Me (Tester)",
                    "mail": "tester@contoso.com",
                    "userPrincipalName": "tester@contoso.com"
                }))
            }),
        )
        .route(
            "/v1.0/me/chats",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "19:md@thread.v2",
                        "chatType": "group",
                        "topic": "MdChat",
                        "lastUpdatedDateTime": "2026-11-21T09:00:01Z",
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "u1", "displayName": "Alex" }
                        ]
                    }]
                }))
            }),
        )
        .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            post(
                |State(st): State<Arc<Bug10State>>, Json(payload): Json<Value>| async move {
                    st.posted.write().unwrap().push(payload.clone());
                    let html = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "md-sent",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T09:01:00Z",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": html },
                            "mentions": payload.get("mentions").cloned().unwrap_or(json!([])),
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let graph_base = format!("http://{addr}/v1.0");
    let auth_base = format!("http://{addr}");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload_b64 = URL_SAFE_NO_PAD.encode(
        json!({
            "oid": "me-user-1",
            "name": "Me (Tester)",
            "upn": "tester@contoso.com",
            "exp": 4_102_444_800_i64
        })
        .to_string()
        .as_bytes(),
    );
    let jwt = format!("{header_b64}.{payload_b64}.sig");
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");

    let run_send = |args: &[&str]| {
        let mut full = vec!["send".to_string(), "MdChat".to_string()];
        full.extend(args.iter().map(|s| (*s).to_string()));
        full.push("--json".to_string());
        let graph_base = graph_base.clone();
        let auth_base = auth_base.clone();
        let jwt = jwt.clone();
        let cfg_dir = cfg_dir.clone();
        let data_dir = data_dir.clone();
        let state = state.clone();
        async move {
            let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_teams"))
                .args(&full)
                .env("TEAMS_GRAPH_BASE_URL", &graph_base)
                .env("TEAMS_AUTH_BASE_URL", &auth_base)
                .env("TEAMS_ACCESS_TOKEN", &jwt)
                .env("TEAMS_DISABLE_KEYRING", "1")
                .env("TEAMS_CONFIG_DIR", &cfg_dir)
                .env("TEAMS_DATA_DIR", &data_dir)
                .env("TEAMS_AUTO_DAEMON", "0")
                .env_remove("TEAMS_TOKEN_FILE")
                .env_remove("TEAMS_TOKEN_REFRESH_CMD")
                .output()
                .await
                .expect("run teams send");
            assert!(
                out.status.success(),
                "send {full:?} failed: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let payload = state.posted.read().unwrap().last().unwrap().clone();
            // Invariant: HTML <at id="N"> ID set MUST equal payload.mentions ID set
            let html = payload["body"]["content"].as_str().unwrap_or("");
            let mut html_at_ids = std::collections::BTreeSet::new();
            let mut rest = html;
            while let Some(pos) = rest.find("<at id=\"") {
                rest = &rest[pos + 8..];
                if let Some(end) = rest.find('"') {
                    let id_num: i64 = rest[..end].parse().expect("numeric at id");
                    html_at_ids.insert(id_num);
                    rest = &rest[end + 1..];
                }
            }
            let mention_ids: std::collections::BTreeSet<i64> = payload
                .get("mentions")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|m| m["id"].as_i64().expect("numeric mention id"))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                html_at_ids, mention_ids,
                "HTML <at> IDs and payload.mentions IDs diverged for {full:?}: payload={payload}"
            );
            payload
        }
    };

    // 1. Tag prefix before Markdown link: [URGENT] Check [runbook](url) now
    let p1 = run_send(&["[URGENT] Check [runbook](https://example.com/runbook) now"]).await;
    assert_eq!(
        p1["body"]["content"],
        "<p>[URGENT] Check <a href=\"https://example.com/runbook\">runbook</a> now</p>"
    );

    // 2. Balanced parentheses inside URL
    let p2 = run_send(&[
        "See [Rust](https://en.wikipedia.org/wiki/Rust_(programming_language)) docs",
    ])
    .await;
    assert_eq!(
        p2["body"]["content"],
        "<p>See <a href=\"https://en.wikipedia.org/wiki/Rust_(programming_language)\">Rust</a> docs</p>"
    );

    // 3. Duplicate display names map sequentially to distinct user IDs
    let p3 = run_send(&[
        "Hi @Alex and @Alex please review",
        "--mention",
        "u1:Alex",
        "--mention",
        "u2:Alex",
    ])
    .await;
    assert_eq!(
        p3["body"]["content"],
        "<p>Hi <at id=\"0\">Alex</at> and <at id=\"1\">Alex</at> please review</p>"
    );
    let m3: Vec<(i64, &str)> = p3["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["id"].as_i64().unwrap(),
                m["mentioned"]["user"]["id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(m3, vec![(0, "u1"), (1, "u2")]);

    // 4. Korean honorific "님" stays outside <at>
    let p4 = run_send(&["@김철수님 확인 부탁드립니다", "--mention", "u3:김철수"]).await;
    assert_eq!(
        p4["body"]["content"],
        "<p><at id=\"0\">김철수</at>님 확인 부탁드립니다</p>"
    );
    assert_eq!(p4["mentions"][0]["mentioned"]["user"]["id"], "u3");

    // 5. Case-insensitive English mention
    let p5 = run_send(&["ping @alice", "--mention", "u4:Alice"]).await;
    assert_eq!(p5["body"]["content"], "<p>ping <at id=\"0\">Alice</at></p>");
    assert_eq!(p5["mentions"][0]["mentioned"]["user"]["id"], "u4");

    // 6. Unused mention fails before POST and leaves posted count unchanged (W3)
    let run_send_raw = |args: &[&str]| {
        let mut full = vec!["send".to_string(), "MdChat".to_string()];
        full.extend(args.iter().map(|s| (*s).to_string()));
        full.push("--json".to_string());
        let graph_base = graph_base.clone();
        let auth_base = auth_base.clone();
        let jwt = jwt.clone();
        let cfg_dir = cfg_dir.clone();
        let data_dir = data_dir.clone();
        async move {
            tokio::process::Command::new(env!("CARGO_BIN_EXE_teams"))
                .args(&full)
                .env("TEAMS_GRAPH_BASE_URL", &graph_base)
                .env("TEAMS_AUTH_BASE_URL", &auth_base)
                .env("TEAMS_ACCESS_TOKEN", &jwt)
                .env("TEAMS_DISABLE_KEYRING", "1")
                .env("TEAMS_CONFIG_DIR", &cfg_dir)
                .env("TEAMS_DATA_DIR", &data_dir)
                .env("TEAMS_AUTO_DAEMON", "0")
                .env_remove("TEAMS_TOKEN_FILE")
                .env_remove("TEAMS_TOKEN_REFRESH_CMD")
                .output()
                .await
                .expect("run teams send")
        }
    };
    let posted_before = state.posted.read().unwrap().len();
    let unused_out = run_send_raw(&["no at here", "--mention", "u5:Bob"]).await;
    assert!(!unused_out.status.success(), "unused mention must fail");
    assert_eq!(
        state.posted.read().unwrap().len(),
        posted_before,
        "unused mention must perform 0 POSTs"
    );
    let unused_stderr = String::from_utf8_lossy(&unused_out.stderr);
    assert!(
        unused_stderr.contains("Mention not found in message body: @Bob (u5)"),
        "unexpected stderr: {unused_stderr}"
    );
    let unused_json: Value =
        serde_json::from_slice(&unused_out.stdout).expect("unused mention --json error");
    assert_eq!(unused_json["ok"], false);
    assert_eq!(unused_json["error"]["code"], "error");

    // Partial unused mention also fails before POST
    let partial_unused = run_send_raw(&[
        "Check [docs](https://example.com) & <tag> @김철수 님",
        "--mention",
        "u-unused:Nobody",
        "--mention",
        "u3:김철수",
    ])
    .await;
    assert!(!partial_unused.status.success());
    assert_eq!(state.posted.read().unwrap().len(), posted_before);
    assert!(
        String::from_utf8_lossy(&partial_unused.stderr)
            .contains("Mention not found in message body: @Nobody (u-unused)")
    );

    // 7. Regression controls: plain link, space-separated Korean mention, HTML escaping
    let p7 = run_send(&[
        "Check [docs](https://example.com) & <tag> @김철수 님",
        "--mention",
        "u3:김철수",
    ])
    .await;
    assert_eq!(
        p7["body"]["content"],
        "<p>Check <a href=\"https://example.com\">docs</a> &amp; &lt;tag&gt; <at id=\"0\">김철수</at> 님</p>"
    );
    assert_eq!(p7["mentions"].as_array().unwrap().len(), 1);
    assert_eq!(p7["mentions"][0]["id"], 0);
    assert_eq!(p7["mentions"][0]["mentioned"]["user"]["id"], "u3");

    server.abort();
}

#[test]
fn regression_teams_bug_07_short_token_multiword_search() {
    use teams_cli::models::ConversationKind;
    use teams_cli::models::ConversationSummary;

    let storage = Storage::open_in_memory().unwrap();
    let c1_target = ConversationTarget::Chat {
        chat_id: "c1".to_string(),
    };
    let c2_target = ConversationTarget::Chat {
        chat_id: "c2".to_string(),
    };
    let conv1 = ConversationSummary {
        conv_key: c1_target.conv_key(),
        target: c1_target,
        kind: ConversationKind::Group,
        team_id: None,
        team_name: None,
        title: "SearchChat1".to_string(),
        topic: Some("SearchChat1".to_string()),
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
    let conv2 = ConversationSummary {
        conv_key: c2_target.conv_key(),
        target: c2_target,
        kind: ConversationKind::Group,
        team_id: None,
        team_name: None,
        title: "SearchChat2".to_string(),
        topic: Some("SearchChat2".to_string()),
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
    storage.upsert_conversations(&[conv1.clone(), conv2.clone()]).unwrap();

    let mk = |conv_key: &str, id: &str, ts: &str, body: &str, subj: Option<&str>| MessageItem {
        msg_key: MessageItem::make_msg_key(conv_key, id),
        conv_key: conv_key.to_string(),
        msg_id: id.to_string(),
        parent_msg_id: None,
        reply_to_id: None,
        reply_to_author: None,
        reply_to_preview: None,
        subject: subj.map(str::to_string),
        author_id: Some("u1".to_string()),
        author_name: "Alice".to_string(),
        is_me: false,
        created_at: ts.to_string(),
        last_modified_at: None,
        deleted: false,
        importance: "normal".to_string(),
        message_type: "message".to_string(),
        body_html: format!("<p>{body}</p>"),
        body_plain: body.to_string(),
        attachments: vec![],
        mentions: vec![],
        reactions: vec![],
        reply_count: 0,
        web_url: None,
    };

    storage
        .upsert_messages(&[
            mk(&conv1.conv_key, "s1", "2026-11-21T09:00:01Z", "프로덕션 배포 오늘 완료", None),
            mk(&conv1.conv_key, "s2", "2026-11-21T09:00:02Z", "deploy release v2", None),
            mk(&conv1.conv_key, "s3", "2026-11-21T09:00:03Z", "canary rollout will deploy at noon", None),
            mk(&conv1.conv_key, "s4", "2026-11-21T09:00:04Z", "프로덕션 배포만 진행", None),
            mk(&conv1.conv_key, "s5", "2026-11-21T09:00:05Z", "progress 100% ok", None),
            mk(&conv1.conv_key, "s6", "2026-11-21T09:00:06Z", "progress 1000 ok", None),
            mk(&conv2.conv_key, "s7", "2026-11-21T09:00:07Z", "프로덕션 배포 오늘 완료", None),
        ])
        .unwrap();

    let hit_ids = |q: &str, scope: Option<&str>| -> Vec<String> {
        storage
            .search_messages(q, scope, 20)
            .unwrap()
            .into_iter()
            .map(|h| h.msg_id)
            .collect()
    };

    // Multi-word with 2-char Korean tokens matches regardless of token order and excludes partial-token s4
    assert_eq!(hit_ids("배포 완료", Some(&conv1.conv_key)), vec!["s1"]);
    assert_eq!(hit_ids("완료 배포", Some(&conv1.conv_key)), vec!["s1"]);

    // Mixed short English token + long token matches regardless of order and excludes single-token s3
    assert_eq!(hit_ids("v2 deploy", Some(&conv1.conv_key)), vec!["s2"]);
    assert_eq!(hit_ids("deploy v2", Some(&conv1.conv_key)), vec!["s2"]);

    // LIKE wildcard escaping: '100% ok' matches literal '100%' (s5), not '1000' (s6)
    assert_eq!(hit_ids("100% ok", Some(&conv1.conv_key)), vec!["s5"]);

    // Regression controls: single 2-char token, >=3-char multiword AND, conversation scope filter
    assert_eq!(hit_ids("배포", Some(&conv1.conv_key)), vec!["s4", "s1"]);
    assert_eq!(hit_ids("canary deploy", Some(&conv1.conv_key)), vec!["s3"]);
    assert_eq!(hit_ids("배포 완료", None), vec!["s7", "s1"]);
    assert_eq!(hit_ids("배포 완료", Some(&conv2.conv_key)), vec!["s7"]);
}
