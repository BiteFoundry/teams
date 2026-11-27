//! Attachment downloads, upload conflict handling, MCP schema/compact output, gateway retry safety, and UPN/nextLink security E2E tests.

use super::server::*;
use tempfile::TempDir;
use std::sync::{Arc, RwLock};
use axum::{response::IntoResponse, extract::{Path, RawQuery, State}, http::{header, HeaderMap, StatusCode}, routing::{get, post}, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use teams_cli::{auth::{AuthManager, StoredTokens}, config::AppConfig, graph::GraphClient, mcp, models::ConversationTarget, serve, storage::Storage};

#[tokio::test]
async fn regression_teams_bug_13_download_by_single_message_get() {
    #[derive(Default)]
    struct Bug13State {
        requested_paths: RwLock<Vec<String>>,
    }

    let state = Arc::new(Bug13State::default());
    let chat_share_url = "https://contoso.sharepoint.com/personal/tester/archive.txt";
    let reply_share_url = "https://contoso.sharepoint.com/sites/eng/reply-log.txt";
    let expected_chat_bytes = b"archived-chat-attachment-bytes-bug13".to_vec();
    let expected_reply_bytes = b"channel-reply-attachment-bytes-bug13".to_vec();

    let chat_bytes_srv = expected_chat_bytes.clone();
    let reply_bytes_srv = expected_reply_bytes.clone();

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
                        "id": "19:dl@thread.v2",
                        "chatType": "group",
                        "topic": "DlChat",
                        "lastUpdatedDateTime": "2026-11-21T09:00:55Z",
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
        // List endpoints (only return recent 50, excluding d01!)
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(|State(st): State<Arc<Bug13State>>, Path(chat_id): Path<String>| async move {
                st.requested_paths
                    .write()
                    .unwrap()
                    .push(format!("LIST:/chats/{chat_id}/messages"));
                let recent: Vec<Value> = (6..=55)
                    .rev()
                    .map(|i| {
                        json!({
                            "id": format!("d{i:02}"),
                            "messageType": "message",
                            "createdDateTime": format!("2026-11-21T09:00:{i:02}Z"),
                            "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": format!("<p>d{i}</p>") },
                            "mentions": [],
                            "attachments": [],
                            "reactions": []
                        })
                    })
                    .collect();
                Json(json!({ "value": recent }))
            }),
        )
        // Single-message GET for chat
        .route(
            "/v1.0/chats/{chat_id}/messages/{msg_id}",
            get(
                move |State(st): State<Arc<Bug13State>>,
                 Path((chat_id, msg_id)): Path<(String, String)>| async move {
                    st.requested_paths
                        .write()
                        .unwrap()
                        .push(format!("GET:/chats/{chat_id}/messages/{msg_id}"));
                    if msg_id == "d01" {
                        Json(json!({
                            "id": "d01",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T08:00:01Z",
                            "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>old archive</p><attachment id=\"att-d01\"></attachment>" },
                            "mentions": [],
                            "attachments": [{
                                "id": "att-d01",
                                "contentType": "reference",
                                "contentUrl": chat_share_url,
                                "name": "archive.txt"
                            }],
                            "reactions": []
                        }))
                        .into_response()
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "NotFound", "message": "message not found" } })),
                        )
                            .into_response()
                    }
                },
            ),
        )
        // Single-message GET for channel reply
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}/replies/{msg_id}",
            get(
                move |State(st): State<Arc<Bug13State>>,
                 Path((team_id, channel_id, root_id, msg_id)): Path<(String, String, String, String)>| async move {
                    st.requested_paths.write().unwrap().push(format!(
                        "GET:/teams/{team_id}/channels/{channel_id}/messages/{root_id}/replies/{msg_id}"
                    ));
                    if root_id == "root-1" && msg_id == "rep-old" {
                        Json(json!({
                            "id": "rep-old",
                            "replyToId": "root-1",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T08:05:00Z",
                            "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>reply log</p><attachment id=\"att-rep\"></attachment>" },
                            "mentions": [],
                            "attachments": [{
                                "id": "att-rep",
                                "contentType": "reference",
                                "contentUrl": reply_share_url,
                                "name": "reply-log.txt"
                            }],
                            "reactions": []
                        }))
                        .into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                },
            ),
        )
        // Guard: thread replies list and channel messages list must NOT be called
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            get(|State(st): State<Arc<Bug13State>>| async move {
                st.requested_paths
                    .write()
                    .unwrap()
                    .push("LIST:channel_messages".to_string());
                Json(json!({ "value": [] }))
            }),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}/replies",
            get(|State(st): State<Arc<Bug13State>>| async move {
                st.requested_paths
                    .write()
                    .unwrap()
                    .push("LIST:thread_replies".to_string());
                Json(json!({ "value": [] }))
            }),
        )
        .route(
            "/v1.0/shares/{share_token}/driveItem/content",
            get(move |Path(share_token): Path<String>| {
                let chat_bytes = chat_bytes_srv.clone();
                let reply_bytes = reply_bytes_srv.clone();
                async move {
                    let encoded = share_token.strip_prefix("u!").unwrap_or("");
                    let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).unwrap_or_default())
                        .unwrap_or_default();
                    if decoded == chat_share_url {
                        (StatusCode::OK, chat_bytes).into_response()
                    } else if decoded == reply_share_url {
                        (StatusCode::OK, reply_bytes).into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }
            }),
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
    let dl_dir = tmp.path().join("dl");
    std::fs::create_dir_all(&dl_dir).unwrap();

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
                .env("TEAMS_AUTO_DAEMON", "0")
                .env_remove("TEAMS_TOKEN_FILE")
                .env_remove("TEAMS_TOKEN_REFRESH_CMD")
                .output()
                .await
                .expect("run teams CLI")
        }
    };

    // Seed conversations only (not messages)
    assert!(run_cli(&["chats", "--refresh", "--json"]).await.status.success());
    assert!(run_cli(&["teams", "--refresh", "--json"]).await.status.success());

    // 1. Download attachment from chat message outside recent 50 (d01) -> single GET, no list GET
    state.requested_paths.write().unwrap().clear();
    let out = run_cli(&["download", "DlChat", "d01", "--out", dl_dir.to_str().unwrap(), "--json"]).await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let dj: Value = serde_json::from_slice(&out.stdout).unwrap();
    let saved = std::path::PathBuf::from(dj["path"].as_str().unwrap());
    assert_eq!(std::fs::read(&saved).unwrap(), expected_chat_bytes);
    let paths1 = state.requested_paths.read().unwrap().clone();
    assert!(
        paths1.iter().any(|p| p == "GET:/chats/19:dl@thread.v2/messages/d01"),
        "expected single-message GET, got: {paths1:?}"
    );
    assert!(
        !paths1.iter().any(|p| p.starts_with("LIST:")),
        "must not issue list query on single-message download: {paths1:?}"
    );

    // 2. Regression control: cached message d01 makes 0 message GETs on second download
    state.requested_paths.write().unwrap().clear();
    let dl_dir2 = tmp.path().join("dl2");
    std::fs::create_dir_all(&dl_dir2).unwrap();
    let out2 = run_cli(&["download", "DlChat", "d01", "--out", dl_dir2.to_str().unwrap(), "--json"]).await;
    assert!(out2.status.success());
    let paths2 = state.requested_paths.read().unwrap().clone();
    assert!(
        paths2.is_empty(),
        "cached message download must issue 0 message GETs, got: {paths2:?}"
    );

    // 3. Invalid ID returning 404 returns error and creates 0 files
    let dl_dir_404 = tmp.path().join("dl404");
    std::fs::create_dir_all(&dl_dir_404).unwrap();
    let out_404 = run_cli(&[
        "download",
        "DlChat",
        "nonexistent-id",
        "--out",
        dl_dir_404.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert_eq!(out_404.status.code(), Some(1));
    let err_404 = String::from_utf8_lossy(&out_404.stderr);
    assert!(err_404.contains("404"), "expected 404 error, got: {err_404}");
    let out_404_json: Value = serde_json::from_slice(&out_404.stdout)
        .expect("--json 404 error must emit JSON on stdout");
    assert_eq!(out_404_json["ok"], false);
    assert_eq!(out_404_json["error"]["code"], "not_found");
    assert_eq!(out_404_json["error"]["http_status"], 404);
    assert_eq!(std::fs::read_dir(&dl_dir_404).unwrap().count(), 0, "no file should be created on 404");

    // 4. Regression control: channel reply attachment uses ONLY /messages/{root}/replies/{id}
    state.requested_paths.write().unwrap().clear();
    let dl_dir_rep = tmp.path().join("dlrep");
    std::fs::create_dir_all(&dl_dir_rep).unwrap();
    let out_rep = run_cli(&[
        "download",
        "Eng/General",
        "rep-old",
        "--thread",
        "root-1",
        "--out",
        dl_dir_rep.to_str().unwrap(),
        "--json",
    ])
    .await;
    assert!(
        out_rep.status.success(),
        "reply download failed: stderr={}",
        String::from_utf8_lossy(&out_rep.stderr)
    );
    let rj: Value = serde_json::from_slice(&out_rep.stdout).unwrap();
    assert_eq!(
        std::fs::read(rj["path"].as_str().unwrap()).unwrap(),
        expected_reply_bytes
    );
    let paths_rep = state.requested_paths.read().unwrap().clone();
    assert_eq!(
        paths_rep,
        vec!["GET:/teams/t1/channels/ch1/messages/root-1/replies/rep-old"]
    );

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_14_mcp_list_chats_schema_and_presence_reset() {
    #[derive(Default)]
    struct Bug14State {
        chats_get_count: RwLock<usize>,
        posted_messages: RwLock<Vec<String>>,
        cleared_presence_count: RwLock<usize>,
        base_url: RwLock<String>,
    }

    let state = Arc::new(Bug14State::default());

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
            get(|State(st): State<Arc<Bug14State>>, RawQuery(query): RawQuery| async move {
                *st.chats_get_count.write().unwrap() += 1;
                let q = query.unwrap_or_default();
                let mut top = 50usize;
                let mut skip = 0usize;
                for pair in q.split('&').filter(|s| !s.is_empty()) {
                    if let Some((k, v)) = pair.split_once('=') {
                        if k == "$top" {
                            if let Ok(n) = v.parse::<usize>() {
                                top = n;
                            }
                        } else if k == "$skiptoken" {
                            if let Ok(n) = v.parse::<usize>() {
                                skip = n;
                            }
                        }
                    }
                }
                let all: Vec<Value> = (1..=5)
                    .map(|i| {
                        json!({
                            "id": format!("19:c{i}@thread.v2"),
                            "chatType": "group",
                            "topic": format!("Chat{i}"),
                            "lastUpdatedDateTime": format!("2026-11-21T09:00:{i:02}Z"),
                            "members": [
                                { "userId": "me-user-1", "displayName": "Me (Tester)" },
                                { "userId": "peer-kim", "displayName": "Alice" }
                            ]
                        })
                    })
                    .collect();
                let page: Vec<Value> = all.into_iter().skip(skip).take(top).collect();
                let next_skip = skip + page.len();
                let mut body = json!({ "value": page });
                if next_skip < 5 {
                    let base = st.base_url.read().unwrap().clone();
                    body["@odata.nextLink"] = Value::String(format!(
                        "{base}/me/chats?$top={top}&$skiptoken={next_skip}"
                    ));
                }
                Json(body)
            }),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async {
                Json(json!({ "value": [{ "id": "t1", "displayName": "BigTeam" }] }))
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
                            "body": { "value": [{ "id": "ch1", "displayName": "Channel-01", "membershipType": "standard" }] }
                        })
                    })
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            post(
                |State(st): State<Arc<Bug14State>>,
                 Path((team_id, channel_id)): Path<(String, String)>,
                 Json(payload): Json<Value>| async move {
                    st.posted_messages
                        .write()
                        .unwrap()
                        .push(format!("/teams/{team_id}/channels/{channel_id}/messages"));
                    let html = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "sent-1",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T09:05:00Z",
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
            "/v1.0/me/presence/clearUserPreferredPresence",
            post(|State(st): State<Arc<Bug14State>>| async move {
                *st.cleared_presence_count.write().unwrap() += 1;
                StatusCode::OK
            }),
        )
        .route(
            "/v1.0/me/presence",
            get(|| async {
                Json(json!({
                    "id": "me-user-1",
                    "availability": "Busy",
                    "activity": "InACall"
                }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let graph_base = format!("http://{addr}/v1.0");
    let auth_base = format!("http://{addr}");
    *state.base_url.write().unwrap() = graph_base.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let cfg = AppConfig {
        graph_base_url: graph_base.clone(),
        auth_base_url: auth_base.clone(),
        ..Default::default()
    };
    let tokens = StoredTokens {
        access_token: "tok".into(),
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
    let storage = Storage::open_in_memory().unwrap();

    // Part A: Server has 5 chats -> list_chats(limit=2) returns 2, then list_chats(limit=5) returns 5
    let call_mcp = |name: &str, arguments: Value| {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .unwrap();
        let cfg = cfg.clone();
        let graph = graph.clone();
        let storage = storage.clone();
        async move { mcp::handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap() }
    };

    let r2 = call_mcp("list_chats", json!({ "limit": 2 })).await;
    assert_eq!(r2["result"]["isError"], false, "{r2}");
    let p2: Value = serde_json::from_str(r2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p2["chats"].as_array().unwrap().len(), 2);

    let r5 = call_mcp("list_chats", json!({ "limit": 5 })).await;
    assert_eq!(r5["result"]["isError"], false, "{r5}");
    let p5: Value = serde_json::from_str(r5["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        p5["chats"].as_array().unwrap().len(),
        5,
        "list_chats(limit=5) after limit=2 must fetch remaining chats: {p5}"
    );

    // Control: once all 5 chats are fetched (EOF), subsequent list_chats(limit=50) makes 0 Graph requests
    let hits_before = *state.chats_get_count.read().unwrap();
    let r50 = call_mcp("list_chats", json!({ "limit": 50 })).await;
    assert_eq!(r50["result"]["isError"], false);
    let p50: Value = serde_json::from_str(r50["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p50["chats"].as_array().unwrap().len(), 5);
    assert_eq!(
        *state.chats_get_count.read().unwrap(),
        hits_before,
        "complete small chat list must not re-request Graph"
    );

    // Part B: send_message with unknown args (thread_root_id / bogus_param) -> isError=true, 0 POSTs
    for bad_args in [
        json!({ "target": "BigTeam/Channel-01", "markdown": "hello", "thread_root_id": "r1" }),
        json!({ "target": "BigTeam/Channel-01", "markdown": "hello", "bogus_param": 123 }),
    ] {
        let resp = call_mcp("send_message", bad_args.clone()).await;
        assert_eq!(
            resp["result"]["isError"], true,
            "unknown arg must be rejected for {bad_args}: {resp}"
        );
    }
    assert_eq!(
        state.posted_messages.read().unwrap().len(),
        0,
        "rejected send_message must perform 0 POSTs"
    );

    // Control: valid schema args send exactly once
    let valid_resp = call_mcp(
        "send_message",
        json!({ "target": "BigTeam/Channel-01", "markdown": "valid message", "subject": "Valid" }),
    )
    .await;
    assert_eq!(valid_resp["result"]["isError"], false, "{valid_resp}");
    assert_eq!(state.posted_messages.read().unwrap().len(), 1);

    // Part C: HTTP POST /presence ("Reset") caches actual Graph presence ("Busy"), while MCP rejects removed set_presence
    let mcp_reset = call_mcp("set_presence", json!({ "availability": "Reset" })).await;
    assert_eq!(mcp_reset["result"]["isError"], true, "{mcp_reset}");

    let http_storage = Storage::open_in_memory().unwrap();
    let api_router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        http_storage.clone(),
        Some("tok-bug14".to_string()),
    );
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_task = tokio::spawn(async move {
        axum::serve(api_listener, api_router).await.unwrap();
    });
    let http_resp = reqwest::Client::new()
        .post(format!("http://{api_addr}/presence"))
        .bearer_auth("tok-bug14")
        .json(&json!({ "availability": "Reset" }))
        .send()
        .await
        .unwrap();
    assert_eq!(http_resp.status(), reqwest::StatusCode::OK);
    let cached_after_http = http_storage.get_presences().unwrap();
    let p_http = cached_after_http.get("me-user-1").expect("presence cached after HTTP reset");
    assert_eq!(
        p_http.availability, "Busy",
        "HTTP Reset must store actual Graph presence, not literal Reset: {p_http:?}"
    );
    assert_eq!(p_http.activity, "InACall");
    api_task.abort();

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_20_compact_default_and_full() {
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
                        "id": "19:release-room@thread.v2",
                        "chatType": "group",
                        "topic": "Release Room",
                        "lastUpdatedDateTime": "2026-11-20T08:06:00Z",
                        "members": [
                            { "userId": "u-alice", "displayName": "Alice Kim" },
                            { "userId": "u-bob", "displayName": "Bob Lee" }
                        ]
                    }]
                }))
            }),
        )
        .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(|Path(_chat_id): Path<String>| async {
                Json(json!({
                    "value": [
                        {
                            "id": "c-msg-6",
                            "messageType": "message",
                            "createdDateTime": "2026-11-20T08:06:00Z",
                            "from": { "user": { "id": "u-bob", "displayName": "Bob Lee" } },
                            "body": {
                                "contentType": "html",
                                "content": "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"c-msg-1\"><strong itemprop=\"mri\" itemid=\"u-alice\">Alice Kim</strong><span itemprop=\"time\" itemid=\"c-msg-1\">11/20/2026 8:00 AM</span><p itemprop=\"preview\">Check the rollout plan</p></blockquote><p>Legacy quote reply</p>"
                            },
                            "attachments": [],
                            "mentions": [],
                            "reactions": []
                        },
                        {
                            "id": "c-msg-5",
                            "messageType": "message",
                            "createdDateTime": "2026-11-20T08:04:00Z",
                            "from": { "user": { "id": "u-bob", "displayName": "Bob Lee" } },
                            "body": { "contentType": "html", "content": "<p>(deleted message)</p>" },
                            "attachments": [],
                            "mentions": [],
                            "reactions": []
                        },
                        {
                            "id": "c-msg-4",
                            "messageType": "systemEventMessage",
                            "createdDateTime": "2026-11-20T08:03:00Z",
                            "from": null,
                            "body": { "contentType": "html", "content": "<systemEventMessage/>" },
                            "eventDetail": { "@odata.type": "#microsoft.graph.membersAddedEventMessageDetail" },
                            "attachments": [],
                            "mentions": [],
                            "reactions": []
                        },
                        {
                            "id": "c-msg-3",
                            "messageType": "message",
                            "createdDateTime": "2026-11-20T08:02:00Z",
                            "deletedDateTime": "2026-11-20T08:02:30Z",
                            "from": { "user": { "id": "u-bob", "displayName": "Bob Lee" } },
                            "body": { "contentType": "html", "content": "" },
                            "attachments": [],
                            "mentions": [],
                            "reactions": []
                        },
                        {
                            "id": "c-msg-2",
                            "messageType": "message",
                            "createdDateTime": "2026-11-20T08:01:00Z",
                            "from": { "user": { "id": "u-bob", "displayName": "Bob Lee" } },
                            "body": { "contentType": "html", "content": "<attachment id=\"c-msg-1\"></attachment><p>Looks good</p>" },
                            "attachments": [{
                                "id": "c-msg-1",
                                "contentType": "messageReference",
                                "contentUrl": null,
                                "name": null,
                                "content": "{\"messageId\":\"c-msg-1\",\"messagePreview\":\"Check the rollout plan\",\"messageSender\":{\"user\":{\"id\":\"u-alice\",\"displayName\":\"Alice Kim\"}}}"
                            }],
                            "mentions": [],
                            "reactions": []
                        },
                        {
                            "id": "c-msg-1",
                            "messageType": "message",
                            "createdDateTime": "2026-11-20T08:00:00Z",
                            "from": { "user": { "id": "u-alice", "displayName": "Alice Kim" } },
                            "body": { "contentType": "html", "content": "<p>Check the rollout plan</p>" },
                            "attachments": [],
                            "mentions": [],
                            "reactions": []
                        }
                    ]
                }))
            })
            .post(|Json(payload): Json<Value>| async move {
                let html = payload
                    .pointer("/body/content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<p></p>");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": "c-msg-sent-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-20T08:10:00Z",
                        "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                        "body": { "contentType": "html", "content": html },
                        "attachments": [],
                        "mentions": [],
                        "reactions": []
                    })),
                )
            }),
        );

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

    let tmp = tempfile::tempdir().unwrap();

    let def_out = run_cli(&tmp, &["read", "Release Room", "--json"]).await;
    assert!(def_out.status.success(), "stderr={}", String::from_utf8_lossy(&def_out.stderr));
    let alias_out = run_cli(&tmp, &["read", "Release Room", "--json", "--compact"]).await;
    assert!(alias_out.status.success());
    assert_eq!(def_out.stdout, alias_out.stdout);

    let j: Value = serde_json::from_slice(&def_out.stdout).unwrap();
    let msgs = j["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 6);

    let m2 = msgs.iter().find(|m| m["msg_id"] == "c-msg-2").unwrap();
    assert_eq!(m2["body_plain"], "Looks good");
    assert_eq!(m2["reply_to_id"], "c-msg-1");
    assert!(m2.get("attachments").is_none(), "{m2}");

    let m3 = msgs.iter().find(|m| m["msg_id"] == "c-msg-3").unwrap();
    assert_eq!(m3["body_plain"], "(deleted message)");
    assert_eq!(m3["deleted"], true);

    let m4 = msgs.iter().find(|m| m["msg_id"] == "c-msg-4").unwrap();
    assert_eq!(m4["message_type"], "systemEventMessage");

    let m5 = msgs.iter().find(|m| m["msg_id"] == "c-msg-5").unwrap();
    assert_eq!(m5["body_plain"], "(deleted message)");
    assert!(m5.get("deleted").is_none(), "{m5}");

    let m6 = msgs.iter().find(|m| m["msg_id"] == "c-msg-6").unwrap();
    assert_eq!(m6["body_plain"], "> Alice Kim\n> Check the rollout plan\nLegacy quote reply");
    assert_eq!(m6["reply_to_id"], "c-msg-1");

    assert!(j["hint"].as_str().unwrap().ends_with("--json --full --limit 30"));
    assert_eq!(j["omitted"]["fields"].as_array().unwrap().len(), 24);

    // --json --full returns full objects without omitted/hint and preserves messageReference attachment
    let full_out = run_cli(&tmp, &["read", "Release Room", "--json", "--full"]).await;
    assert!(full_out.status.success());
    let jf: Value = serde_json::from_slice(&full_out.stdout).unwrap();
    assert!(jf.get("omitted").is_none());
    assert!(jf.get("hint").is_none());
    let fm2 = jf["messages"].as_array().unwrap().iter().find(|m| m["msg_id"] == "c-msg-2").unwrap();
    assert!(fm2.get("body_html").is_some());
    assert_eq!(fm2["attachments"][0]["content_type"], "messageReference");

    // --json --compact --full exits 2
    let conflict = run_cli(&tmp, &["read", "Release Room", "--json", "--compact", "--full"]).await;
    assert_eq!(conflict.status.code(), Some(2));

    // MCP get_messages default (compact) vs compact: false (full)
    let dl_tmp = tempfile::tempdir().unwrap();
    let (cfg, graph) = build_direct_graph_client(&auth_base, dl_tmp.path(), &jwt);
    let storage = Storage::open_in_memory().unwrap();

    let mcp_def = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_messages","arguments":{"target":"Release Room"}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_def["result"]["isError"], false, "{mcp_def}");
    let mp_def: Value =
        serde_json::from_str(mcp_def["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(mp_def["hint"].as_str().unwrap().contains("get_messages"));
    assert!(mp_def["messages"][0].get("body_html").is_none());

    let mcp_full = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"get_messages","arguments":{"target":"Release Room","compact":false}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_full["result"]["isError"], false, "{mcp_full}");
    let mp_full: Value =
        serde_json::from_str(mcp_full["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(mp_full.get("omitted").is_none());
    assert!(mp_full.get("hint").is_none());
    assert!(mp_full["messages"][0].get("body_html").is_some());

    // W6a: send --json returns compact ack by default; --json --full returns full object
    let send_compact = run_cli(&tmp, &["send", "Release Room", "Compact ack body", "--json"]).await;
    assert!(
        send_compact.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&send_compact.stderr)
    );
    let sc_json: Value = serde_json::from_slice(&send_compact.stdout).unwrap();
    assert_eq!(sc_json["ok"], true);
    assert_eq!(sc_json["conversation"]["conv_key"], "chat:19:release-room@thread.v2");
    assert_eq!(sc_json["message"]["msg_id"], "c-msg-sent-1");
    assert!(sc_json["message"].get("body_html").is_none(), "{sc_json}");

    let send_full = run_cli(
        &tmp,
        &["send", "Release Room", "Full ack body", "--json", "--full"],
    )
    .await;
    assert!(
        send_full.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&send_full.stderr)
    );
    let sf_json: Value = serde_json::from_slice(&send_full.stdout).unwrap();
    assert_eq!(sf_json["ok"], true);
    assert_eq!(sf_json["message"]["msg_id"], "c-msg-sent-1");
    assert!(sf_json["message"].get("body_html").is_some(), "{sf_json}");

    // MCP send_message compact default vs compact: false
    let mcp_send_def = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"send_message","arguments":{"target":"Release Room","markdown":"MCP compact ack"}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_send_def["result"]["isError"], false, "{mcp_send_def}");
    let msd_json: Value = serde_json::from_str(
        mcp_send_def["result"]["content"][0]["text"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(msd_json["message"]["msg_id"], "c-msg-sent-1");
    assert!(msd_json["message"].get("body_html").is_none(), "{msd_json}");

    let mcp_send_full = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"send_message","arguments":{"target":"Release Room","markdown":"MCP full ack","compact":false}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_send_full["result"]["isError"], false, "{mcp_send_full}");
    let msf_json: Value = serde_json::from_str(
        mcp_send_full["result"]["content"][0]["text"].as_str().unwrap(),
    )
    .unwrap();
    assert!(msf_json["message"].get("body_html").is_some(), "{msf_json}");

    server.abort();
}

// ---------------------------------------------------------------------------------------
// v0.9.1 regression tests

#[tokio::test]
async fn regression_teams_28_small_upload_renames_on_conflict() {
    ensure_isolated_env();
    let (base_url, state) = spawn_mock_graph_server("tok-28").await;
    *state.emulate_upload_rename.write().unwrap() = true;
    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph) = build_direct_graph_client(&base_url, dir.path(), "tok-28");

    let local = dir.path().join("report.pdf");
    let chat = ConversationTarget::Chat { chat_id: "19:chat-28@thread.v2".to_string() };
    let channel = ConversationTarget::Channel {
        team_id: "team-dev".to_string(),
        channel_id: "19:deploy@thread.tacv2".to_string(),
    };

    for (target, first_id) in [(&chat, "drive-item-chat-1"), (&channel, "sp-item-drive-sp-dev-1")] {
        std::fs::write(&local, b"first version").unwrap();
        let first = graph.upload_reference_file(target, &local).await.unwrap();
        std::fs::write(&local, b"second, different version").unwrap();
        let second = graph.upload_reference_file(target, &local).await.unwrap();

        assert_eq!(first.drive_item_id.as_deref(), Some(first_id));
        assert_ne!(first.drive_item_id, second.drive_item_id, "second upload must be a new driveItem");
        assert_eq!(first.name, "report.pdf");
        assert_eq!(second.name, "report 1.pdf", "attachment name follows Graph's rename");
        // The earlier item is untouched by the second upload.
        assert_eq!(
            state.uploaded_files.read().unwrap().get(first_id).map(Vec::as_slice),
            Some(&b"first version"[..])
        );
    }

    let queries = state.upload_queries.read().unwrap().clone();
    assert_eq!(queries.len(), 4);
    for q in &queries {
        assert_eq!(q, "@microsoft.graph.conflictBehavior=rename", "PUT must carry the literal conflict query");
    }
}

#[tokio::test]
async fn regression_teams_28_post_not_retried_on_gateway_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    ensure_isolated_env();

    let gateway_posts = Arc::new(AtomicUsize::new(0));
    let throttled_posts = Arc::new(AtomicUsize::new(0));
    let flaky_gets = Arc::new(AtomicUsize::new(0));
    let (gp, tp, fg) = (gateway_posts.clone(), throttled_posts.clone(), flaky_gets.clone());

    let app = Router::new()
        .route(
            "/v1.0/chats/gateway/messages",
            post(move || {
                let calls = gp.clone();
                async move {
                    // The first attempt "times out at the gateway" after Graph may have applied it.
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::GATEWAY_TIMEOUT, Json(json!({"error": {"code": "GatewayTimeout", "message": "upstream timeout"}})))
                    } else {
                        (StatusCode::CREATED, Json(json!({"id": "duplicate"})))
                    }
                }
            }),
        )
        .route(
            "/v1.0/chats/throttled/messages",
            post(move || {
                let calls = tp.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::TOO_MANY_REQUESTS, [("Retry-After", "1")], Json(json!({"error": {"code": "TooManyRequests"}})))
                    } else {
                        (StatusCode::CREATED, [("Retry-After", "0")], Json(json!({"id": "sent-after-throttle"})))
                    }
                }
            }),
        )
        .route(
            "/v1.0/flaky-read",
            get(move || {
                let calls = fg.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::BAD_GATEWAY, [("Retry-After", "1")], Json(json!({"error": {"code": "BadGateway"}})))
                    } else {
                        (StatusCode::OK, [("Retry-After", "0")], Json(json!({"ok": true})))
                    }
                }
            }),
        );
    let (base_url, server) = spawn_test_router(app).await;
    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph) = build_direct_graph_client(&base_url, dir.path(), "tok");
    let body = json!({"body": {"contentType": "html", "content": "<p>hi</p>"}});

    // 504 on a write surfaces as an error after exactly one attempt.
    let err = graph
        .post_json::<_, Value>("/chats/gateway/messages", &body)
        .await
        .expect_err("a 504 on POST must not be replayed");
    assert_eq!(gateway_posts.load(Ordering::SeqCst), 1);
    let msg = err.to_string();
    assert!(msg.contains("): 504"), "status stays parseable: {msg}");

    // 429 is a pre-processing rejection: still retried.
    let sent: Value = graph.post_json("/chats/throttled/messages", &body).await.unwrap();
    assert_eq!(sent["id"], "sent-after-throttle");
    assert_eq!(throttled_posts.load(Ordering::SeqCst), 2);

    // Reads keep the gateway retry.
    let read: Value = graph.get_json("/flaky-read").await.unwrap();
    assert_eq!(read["ok"], true);
    assert_eq!(flaky_gets.load(Ordering::SeqCst), 2);

    server.abort();
}

#[tokio::test]
async fn regression_teams_01_bearer_not_sent_to_foreign_next_link() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    ensure_isolated_env();

    // A second listener stands in for an attacker-controlled host named by a nextLink.
    let foreign_hits = Arc::new(AtomicUsize::new(0));
    let foreign_auth: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));
    let (fh, fa) = (foreign_hits.clone(), foreign_auth.clone());
    let foreign = Router::new().fallback(move |headers: HeaderMap| {
        let (hits, seen) = (fh.clone(), fa.clone());
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
                seen.write().unwrap().push(v.to_string());
            }
            Json(json!({ "value": [] }))
        }
    });
    let (foreign_base, foreign_server) = spawn_test_router(foreign).await;

    let next_link = format!("{foreign_base}/v1.0/me/chats?$skiptoken=page2");
    let app = Router::new().route(
        "/v1.0/me/chats",
        get(move || {
            let next_link = next_link.clone();
            async move {
                Json(json!({
                    "value": [{ "id": "19:first@thread.v2", "chatType": "group", "topic": "First", "members": [] }],
                    "@odata.nextLink": next_link
                }))
            }
        }),
    );
    let (base_url, server) = spawn_test_router(app).await;
    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph) = build_direct_graph_client(&base_url, dir.path(), "tok-01");

    let err = graph
        .list_chats(100)
        .await
        .expect_err("the walk must stop instead of following the foreign link with a token");
    let msg = format!("{err:#}");
    assert!(msg.contains("Refusing to send Graph credentials to an unexpected host: 127.0.0.1"), "{msg}");
    assert!(!msg.contains("skiptoken"), "only the host is reported: {msg}");
    assert_eq!(foreign_hits.load(Ordering::SeqCst), 0, "no request may reach the foreign host");
    assert!(foreign_auth.read().unwrap().is_empty());

    // The same guard covers a cursor handed in directly (e.g. one read back from SQLite).
    let err = graph
        .list_chats_page(50, Some(&format!("{foreign_base}/v1.0/me/chats")))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("Refusing to send Graph credentials"));
    assert_eq!(foreign_hits.load(Ordering::SeqCst), 0);

    server.abort();
    foreign_server.abort();
}

#[tokio::test]
async fn regression_teams_16_create_chat_escapes_quoted_and_guest_upn() {
    ensure_isolated_env();
    let bodies: Arc<RwLock<Vec<Value>>> = Arc::new(RwLock::new(Vec::new()));
    let captured = bodies.clone();
    let app = Router::new().route(
        "/v1.0/chats",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                captured.write().unwrap().push(body);
                (
                    StatusCode::CREATED,
                    Json(json!({ "id": "19:new@unq.gbl.spaces", "chatType": "oneOnOne", "members": [] })),
                )
            }
        }),
    );
    let (base_url, server) = spawn_test_router(app).await;
    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph) = build_direct_graph_client(&base_url, dir.path(), "tok-16");

    graph.create_chat(&["o'brien@contoso.com".to_string()], None).await.unwrap();
    graph
        .create_chat(
            &["guest_ext.com#EXT#@tenant.onmicrosoft.com".to_string(), "bob@contoso.com".to_string()],
            Some("Guests"),
        )
        .await
        .unwrap();

    let bodies = bodies.read().unwrap();
    let binds = |body: &Value| -> Vec<String> {
        body["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["user@odata.bind"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        binds(&bodies[0]),
        [
            "https://graph.microsoft.com/v1.0/users('me-user-1')",
            "https://graph.microsoft.com/v1.0/users('o''brien@contoso.com')",
        ]
    );
    assert_eq!(
        binds(&bodies[1]),
        [
            "https://graph.microsoft.com/v1.0/users('me-user-1')",
            "https://graph.microsoft.com/v1.0/users('guest_ext.com%23EXT%23@tenant.onmicrosoft.com')",
            "https://graph.microsoft.com/v1.0/users('bob@contoso.com')",
        ]
    );
    server.abort();
}
