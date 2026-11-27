//! Multi-page chat/channel history, SQLite tie-breaking, timestamp boundaries, history cursors, and cache sufficiency E2E tests.

use super::server::*;
use tempfile::TempDir;
use std::{collections::HashMap, sync::{Arc, RwLock}};
use axum::{response::IntoResponse, extract::{Path, RawQuery, State}, http::StatusCode, routing::get, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use teams_cli::{models::ConversationTarget, storage::Storage};

#[tokio::test]
async fn e2e_mock_graph_multipage_chat_history_and_sqlite_tiebreak() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create history test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");
    let token = "history-e2e-tok-2026";
    let chat_id = "19:history-chat@thread.v2";

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind history mock server");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let graph_base = format!("{base_url}/v1.0");

    let encoded_chat = urlencoding::encode(chat_id).into_owned();
    let p2_link = format!("{graph_base}/chats/{encoded_chat}/messages?$skiptoken=p2");
    let p3_link = format!("{graph_base}/chats/{encoded_chat}/messages?$skiptoken=p3");
    let expired_link = format!("{graph_base}/chats/{encoded_chat}/messages?$skiptoken=expired");

    let requested_queries = Arc::new(RwLock::new(Vec::<String>::new()));
    let rq_clone = requested_queries.clone();
    let (p2_c, p3_c) = (p2_link.clone(), p3_link.clone());

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
            get(move |RawQuery(q): RawQuery| {
                let (p2_url, p3_url, queries) = (p2_c.clone(), p3_c.clone(), rq_clone.clone());
                async move {
                    let query = q.unwrap_or_default();
                    queries.write().unwrap().push(query.clone());
                    if query.contains("$skiptoken=expired") {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": {
                                    "code": "InvalidSkipToken",
                                    "message": "The skiptoken has expired."
                                }
                            })),
                        )
                            .into_response();
                    }
                    if query.contains("$skiptoken=p2") {
                        return Json(json!({
                            "value": [
                                mock_graph_message("m4", "2026-11-10T10:00:00Z", "Message 4 (same timestamp)", None, None),
                                mock_graph_message("m3", "2026-11-10T10:00:00Z", "Message 3 (same timestamp)", None, None),
                            ],
                            "@odata.nextLink": p3_url
                        }))
                        .into_response();
                    }
                    if query.contains("$skiptoken=p3") {
                        return Json(json!({
                            "value": [
                                mock_graph_message("m2", "2026-11-10T09:00:00Z", "Message 2", None, None),
                                mock_graph_message("m1", "2026-11-10T08:00:00Z", "Message 1 (oldest)", None, None),
                            ]
                        }))
                        .into_response();
                    }
                    Json(json!({
                        "value": [
                            mock_graph_message("m6", "2026-11-10T12:00:00Z", "Message 6 (latest)", None, None),
                            mock_graph_message("m5", "2026-11-10T11:00:00Z", "Message 5", None, None),
                        ],
                        "@odata.nextLink": p2_url
                    }))
                    .into_response()
                }
            }),
        );

    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve history mock Graph");
    });

    let (_cfg, graph) = build_direct_graph_client(&base_url, &download_dir, token);
    let storage = Storage::open(&db_path).expect("open SQLite storage");
    let target = ConversationTarget::Chat {
        chat_id: chat_id.to_string(),
    };
    let conv_key = target.conv_key();

    // 1. Page 1 -> returns newest messages (m5, m6) + @odata.nextLink pointing to p2
    let (p1_msgs, next_after_p1) = graph
        .list_messages_page(&target, 50, None)
        .await
        .expect("list_messages_page page 1");
    let p1_ids: Vec<&str> = p1_msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(p1_ids, vec!["m5", "m6"]);
    assert_eq!(next_after_p1.as_deref(), Some(p2_link.as_str()));
    storage.upsert_messages(&p1_msgs).expect("upsert page 1");

    // 2. Page 2 via fetch_older_messages(saved_next_link = p2) -> same-timestamp m3, m4 + p3 link
    let (p2_msgs, next_after_p2) = graph
        .fetch_older_messages(&target, next_after_p1.as_deref(), Some("2026-11-10T11:00:00Z"), 2)
        .await
        .expect("fetch_older_messages page 2");
    let p2_ids: Vec<&str> = p2_msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(p2_ids, vec!["m3", "m4"]);
    assert_eq!(
        next_after_p2.as_deref(),
        Some(p3_link.as_str()),
        "nextLink should still point to page 3 before EOF"
    );
    storage.upsert_messages(&p2_msgs).expect("upsert page 2");

    // 3. Page 3 via fetch_older_messages(saved_next_link = p3) -> m1, m2 + None (EOF)
    let (p3_msgs, next_after_p3) = graph
        .fetch_older_messages(&target, next_after_p2.as_deref(), Some("2026-11-10T10:00:00Z"), 2)
        .await
        .expect("fetch_older_messages page 3");
    let p3_ids: Vec<&str> = p3_msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(p3_ids, vec!["m1", "m2"]);
    assert_eq!(next_after_p3, None, "new_next_link must be None only at EOF");
    storage.upsert_messages(&p3_msgs).expect("upsert page 3");

    // 4. SQLite tie-breaking cursor walk from m6 back to m1 without skipping m3/m4
    let mut walked_desc = vec!["m6".to_string()];
    let mut cursor_ts = "2026-11-10T12:00:00Z".to_string();
    let mut cursor_id = Some("m6".to_string());
    loop {
        let older = storage
            .list_older_root_messages(&conv_key, 1, &cursor_ts, cursor_id.as_deref())
            .expect("list_older_root_messages step");
        if older.is_empty() {
            break;
        }
        assert_eq!(older.len(), 1);
        walked_desc.push(older[0].msg_id.clone());
        cursor_ts = older[0].created_at.clone();
        cursor_id = Some(older[0].msg_id.clone());
    }
    assert_eq!(walked_desc, vec!["m6", "m5", "m4", "m3", "m2", "m1"]);

    // Also verify batch retrieval strictly before m4 includes m3 (same timestamp) + m2 + m1
    let before_m4 = storage
        .list_older_root_messages(&conv_key, 10, "2026-11-10T10:00:00Z", Some("m4"))
        .expect("list_older_root_messages before m4");
    let before_m4_ids: Vec<&str> = before_m4.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(before_m4_ids, vec!["m1", "m2", "m3"]);

    // 5. Expired $skiptoken=expired -> 400 InvalidSkipToken -> transparent fallback to Page 1 -> p2 -> p3
    let (fallback_msgs, fallback_next) = graph
        .fetch_older_messages(&target, Some(&expired_link), None, 6)
        .await
        .expect("fetch_older_messages should fall back on expired skiptoken");
    let fallback_ids: Vec<&str> = fallback_msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(fallback_ids, vec!["m1", "m2", "m3", "m4", "m5", "m6"]);
    assert_eq!(fallback_next, None);
    assert!(
        requested_queries
            .read()
            .unwrap()
            .iter()
            .any(|q| q.contains("$skiptoken=expired")),
        "expected expired skiptoken to be attempted before fallback"
    );

    server.abort();
}

#[tokio::test]
async fn e2e_mock_graph_channel_expand_replies_pagination_and_chats_orderby_fallback() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create channel/chats pagination tempdir");
    let download_dir = temp.path().join("downloads");
    let token = "channel-chats-e2e-tok-2026";

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind channel/chats mock server");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let graph_base = format!("{base_url}/v1.0");

    let chan_p2_link =
        format!("{graph_base}/teams/team-dev/channels/chan-general/messages?$skiptoken=chan_p2");
    let chats_p2_link = format!("{graph_base}/me/chats?$skiptoken=chats_p2");

    let channel_queries = Arc::new(RwLock::new(Vec::<String>::new()));
    let chat_queries = Arc::new(RwLock::new(Vec::<String>::new()));
    let cq_clone = channel_queries.clone();
    let chq_clone = chat_queries.clone();

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
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            get(move |RawQuery(q): RawQuery| {
                let (chan_p2, queries) = (chan_p2_link.clone(), cq_clone.clone());
                async move {
                    let query = q.unwrap_or_default();
                    queries.write().unwrap().push(query.clone());
                    if query.contains("$skiptoken=chan_p2") {
                        return Json(json!({
                            "value": [
                                mock_graph_message(
                                    "root-1",
                                    "2026-11-10T09:00:00Z",
                                    "Older channel root 1",
                                    None,
                                    Some(vec![mock_graph_message(
                                        "reply-1a",
                                        "2026-11-10T09:05:00Z",
                                        "Older reply 1a",
                                        Some("root-1"),
                                        None,
                                    )]),
                                )
                            ]
                        }))
                        .into_response();
                    }
                    assert!(
                        query.contains("$expand=replies"),
                        "initial channel page must include $expand=replies, got: {query}"
                    );
                    Json(json!({
                        "value": [
                            mock_graph_message(
                                "root-2",
                                "2026-11-10T12:00:00Z",
                                "Latest channel root 2",
                                None,
                                Some(vec![mock_graph_message(
                                    "reply-2a",
                                    "2026-11-10T12:05:00Z",
                                    "Latest reply 2a",
                                    Some("root-2"),
                                    None,
                                )]),
                            )
                        ],
                        "@odata.nextLink": chan_p2
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/v1.0/me/chats",
            get(move |RawQuery(q): RawQuery| {
                let (chats_p2, queries) = (chats_p2_link.clone(), chq_clone.clone());
                async move {
                    let query = q.unwrap_or_default();
                    queries.write().unwrap().push(query.clone());

                    if query.contains("$orderby") {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": {
                                    "code": "BadRequest",
                                    "message": "Query option '$orderby' is not supported for this request."
                                }
                            })),
                        )
                            .into_response();
                    }

                    let make_chat_batch = |start: usize, count: usize| -> Vec<Value> {
                        (start..start + count)
                            .map(|idx| {
                                json!({
                                    "id": format!("19:chat-{idx:03}@thread.v2"),
                                    "chatType": "group",
                                    "topic": format!("Chat {idx:03}"),
                                    "lastUpdatedDateTime": "2026-11-15T05:00:00Z",
                                    "members": [
                                        { "userId": "me-user-1", "displayName": "Me (Tester)" },
                                        { "userId": format!("user-{idx}"), "displayName": format!("User {idx}") }
                                    ]
                                })
                            })
                            .collect()
                    };

                    if query.contains("$skiptoken=chats_p2") {
                        return Json(json!({
                            "value": make_chat_batch(51, 30)
                        }))
                        .into_response();
                    }

                    Json(json!({
                        "value": make_chat_batch(1, 50),
                        "@odata.nextLink": chats_p2
                    }))
                    .into_response()
                }
            }),
        );

    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve channel/chats mock Graph");
    });

    let (_cfg, graph) = build_direct_graph_client(&base_url, &download_dir, token);

    // 1. Channel multi-page pagination with $expand=replies
    let chan_target = ConversationTarget::Channel {
        team_id: "team-dev".to_string(),
        channel_id: "chan-general".to_string(),
    };
    let (chan_msgs, chan_next) = graph
        .fetch_older_messages(&chan_target, None, None, 2)
        .await
        .expect("fetch_older_messages across 2 channel pages");
    assert_eq!(chan_next, None);
    let chan_ids: Vec<&str> = chan_msgs.iter().map(|m| m.msg_id.as_str()).collect();
    assert_eq!(chan_ids, vec!["root-1", "reply-1a", "root-2", "reply-2a"]);
    let reply_1a = chan_msgs.iter().find(|m| m.msg_id == "reply-1a").unwrap();
    assert_eq!(reply_1a.parent_msg_id.as_deref(), Some("root-1"));
    assert_eq!(channel_queries.read().unwrap().len(), 2);

    // 2. /me/chats multi-page list_chats(75) with $orderby 400 fallback
    let chats = graph
        .list_chats(75)
        .await
        .expect("list_chats(75) with $orderby fallback across 2 pages");
    assert_eq!(chats.len(), 75);
    assert_eq!(chats[0].title, "Chat 001");
    assert_eq!(chats[49].title, "Chat 050");
    assert_eq!(chats[74].title, "Chat 075");

    let recorded_chat_queries = chat_queries.read().unwrap();
    assert!(
        recorded_chat_queries.iter().any(|q| q.contains("$orderby")),
        "expected $orderby to be attempted first"
    );
    assert!(
        recorded_chat_queries
            .iter()
            .any(|q| q.contains("$skiptoken=chats_p2")),
        "expected page 2 $skiptoken=chats_p2 to be fetched"
    );

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_04_timestamp_order_and_before_boundary() {
    let fixture_msgs = vec![
        ("a-before", "2026-11-20T23:59:59.900Z", "msg before midnight"),
        ("b-exact", "2026-11-21T00:00:00Z", "msg exact midnight z"),
        ("b-kst", "2026-11-21T09:00:00+09:00", "msg exact midnight kst"),
        ("c-001", "2026-11-21T00:00:00.001Z", "msg 1ms after"),
        ("d-500", "2026-11-21T00:00:00.500Z", "msg 500ms after"),
    ];
    let graph_msgs: Vec<Value> = fixture_msgs
        .iter()
        .rev()
        .map(|(id, ts, body)| {
            json!({
                "id": id,
                "messageType": "message",
                "createdDateTime": ts,
                "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                "body": { "contentType": "html", "content": format!("<p>{body}</p>") },
                "mentions": [],
                "attachments": [],
                "reactions": []
            })
        })
        .collect();
    let graph_msgs_arc = Arc::new(graph_msgs);

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
                        "id": "19:ts@thread.v2",
                        "chatType": "group",
                        "topic": "TsChat",
                        "lastUpdatedDateTime": "2026-11-21T00:00:00.500Z",
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "peer-kim", "displayName": "Alice" }
                        ]
                    }]
                }))
            }),
        )
        .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get({
                let msgs = graph_msgs_arc.clone();
                move |Path(_chat_id): Path<String>| {
                    let msgs = msgs.clone();
                    async move { Json(json!({ "value": *msgs })) }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    let graph_base = format!("http://{addr}/v1.0");
    let auth_base = format!("http://{addr}");
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("data");

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

    let expected_all = vec!["a-before", "b-exact", "b-kst", "c-001", "d-500"];

    // Online read order (with exact tie-break b-exact < b-kst) and preserved raw created_at strings
    let out = run_cli(&["read", "TsChat", "--json"]).await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let j_online: Value = serde_json::from_slice(&out.stdout).unwrap();
    let online_ids: Vec<&str> = j_online["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["msg_id"].as_str().unwrap())
        .collect();
    assert_eq!(online_ids, expected_all, "online read order mismatch");
    let online_ts: HashMap<&str, &str> = j_online["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["msg_id"].as_str().unwrap(), m["created_at"].as_str().unwrap()))
        .collect();
    assert_eq!(online_ts["b-exact"], "2026-11-21T00:00:00Z");
    assert_eq!(online_ts["b-kst"], "2026-11-21T09:00:00+09:00");

    // Offline read order matches online
    let out_off = run_cli(&["read", "TsChat", "--offline", "--json"]).await;
    assert!(out_off.status.success());
    let j_off: Value = serde_json::from_slice(&out_off.stdout).unwrap();
    let off_ids: Vec<&str> = j_off["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["msg_id"].as_str().unwrap())
        .collect();
    assert_eq!(off_ids, expected_all, "offline read order mismatch");

    // --before 2026-11-21 (date boundary = 00:00:00Z) returns ONLY a-before (online and offline)
    for extra in [&[][..], &["--offline"][..]] {
        let mut args = vec!["read", "TsChat", "--before", "2026-11-21", "--json"];
        args.extend_from_slice(extra);
        let r = run_cli(&args).await;
        assert!(r.status.success(), "stderr={}", String::from_utf8_lossy(&r.stderr));
        let j: Value = serde_json::from_slice(&r.stdout).unwrap();
        let got: Vec<&str> = j["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["msg_id"].as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["a-before"], "args={args:?}");
    }

    // --before 2026-11-21T00:00:00.500Z returns a-before, b-exact, b-kst, c-001 (online and offline)
    for extra in [&[][..], &["--offline"][..]] {
        let mut args = vec![
            "read",
            "TsChat",
            "--before",
            "2026-11-21T00:00:00.500Z",
            "--json",
        ];
        args.extend_from_slice(extra);
        let r = run_cli(&args).await;
        assert!(r.status.success(), "stderr={}", String::from_utf8_lossy(&r.stderr));
        let j: Value = serde_json::from_slice(&r.stdout).unwrap();
        let got: Vec<&str> = j["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["msg_id"].as_str().unwrap())
            .collect();
        assert_eq!(got, vec!["a-before", "b-exact", "b-kst", "c-001"], "args={args:?}");
    }

    // Regression control: re-running read does not duplicate DB rows or search hits
    let out2 = run_cli(&["read", "TsChat", "--json"]).await;
    assert!(out2.status.success());
    let search_out = run_cli(&["search", "midnight", "--json"]).await;
    assert!(search_out.status.success());
    let sj: Value = serde_json::from_slice(&search_out.stdout).unwrap();
    assert_eq!(sj["hits"].as_array().unwrap().len(), 3, "no duplicate search hits: {sj}");

    // Pre-upgrade v55 DB without created_at_sort column: opening via Storage::open migrates in one transaction
    let legacy_tmp = tempfile::tempdir().unwrap();
    let legacy_data = legacy_tmp.path().join("data");
    std::fs::create_dir_all(&legacy_data).unwrap();
    let legacy_db = legacy_data.join("cache.db");
    {
        let conn = rusqlite::Connection::open(&legacy_db).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE conversations (
                conv_key TEXT PRIMARY KEY,
                target_json TEXT NOT NULL,
                kind TEXT NOT NULL,
                team_id TEXT,
                team_name TEXT,
                title TEXT NOT NULL,
                topic TEXT,
                unread_count INTEGER NOT NULL DEFAULT 0,
                last_read_at TEXT,
                last_msg_id TEXT,
                last_msg_author TEXT,
                last_msg_preview TEXT,
                last_updated_at TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                peer_user_id TEXT,
                web_url TEXT
            );
            CREATE INDEX idx_conversations_sort_v2 ON conversations(pinned DESC, COALESCE(last_updated_at, '') DESC, title ASC);
            CREATE TABLE teams (team_id TEXT PRIMARY KEY, display_name TEXT NOT NULL, description TEXT, channels_json TEXT NOT NULL);
            CREATE TABLE messages (
                msg_key TEXT PRIMARY KEY,
                conv_key TEXT NOT NULL,
                msg_id TEXT NOT NULL,
                parent_msg_id TEXT,
                reply_to_id TEXT,
                reply_to_author TEXT,
                reply_to_preview TEXT,
                subject TEXT,
                author_id TEXT,
                author_name TEXT NOT NULL,
                is_me INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                last_modified_at TEXT,
                deleted INTEGER NOT NULL DEFAULT 0,
                importance TEXT NOT NULL,
                message_type TEXT NOT NULL,
                body_html TEXT NOT NULL,
                body_plain TEXT NOT NULL,
                attachments_json TEXT NOT NULL,
                mentions_json TEXT NOT NULL,
                reactions_json TEXT NOT NULL,
                reply_count INTEGER NOT NULL DEFAULT 0,
                web_url TEXT,
                has_mentions INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_messages_conv_root_created_id ON messages(conv_key, parent_msg_id, created_at DESC, msg_id DESC);
            CREATE INDEX idx_messages_conv_msg ON messages(conv_key, msg_id);
            CREATE INDEX idx_messages_created ON messages(created_at DESC);
            CREATE INDEX idx_messages_has_mentions ON messages(created_at DESC, msg_id DESC) WHERE deleted = 0 AND has_mentions = 1;
            CREATE TABLE members (conv_key TEXT NOT NULL, user_id TEXT NOT NULL, member_id TEXT, display_name TEXT NOT NULL, email TEXT, roles_json TEXT NOT NULL, PRIMARY KEY (conv_key, user_id));
            CREATE TABLE presences (user_id TEXT PRIMARY KEY, availability TEXT NOT NULL, activity TEXT NOT NULL, updated_at TEXT NOT NULL);
            CREATE TABLE sync_cursors (scope_key TEXT PRIMARY KEY, delta_link TEXT, oldest_msg_ts TEXT, synced_at TEXT NOT NULL);
            CREATE TABLE thread_reply_windows (conv_key TEXT NOT NULL, root_msg_id TEXT NOT NULL, reply_ids_json TEXT NOT NULL, complete INTEGER NOT NULL, PRIMARY KEY (conv_key, root_msg_id));
            CREATE VIRTUAL TABLE messages_fts USING fts5(msg_key UNINDEXED, conv_key UNINDEXED, author_name, body_plain, subject, tokenize='trigram');
            INSERT INTO conversations (conv_key, target_json, kind, title)
            VALUES ('chat:19:ts@thread.v2', '{\"target_type\":\"chat\",\"chat_id\":\"19:ts@thread.v2\"}', 'group', 'TsChat');
            PRAGMA user_version = 55;
            ",
        )
        .unwrap();
        for (id, ts, body) in &fixture_msgs {
            let mk = format!("chat:19:ts@thread.v2|{id}");
            conn.execute(
                "INSERT INTO messages (
                    msg_key, conv_key, msg_id, author_name, created_at, importance,
                    message_type, body_html, body_plain, attachments_json, mentions_json, reactions_json
                ) VALUES (?1, 'chat:19:ts@thread.v2', ?2, 'Alice', ?3, 'normal', 'message', ?4, ?5, '[]', '[]', '[]')",
                rusqlite::params![mk, id, ts, format!("<p>{body}</p>"), body],
            )
            .unwrap();
        }
    }

    let upgraded = Storage::open(&legacy_db).unwrap();
    let up_all: Vec<String> = upgraded
        .list_root_messages("chat:19:ts@thread.v2", 10, None)
        .unwrap()
        .into_iter()
        .map(|m| m.msg_id)
        .collect();
    assert_eq!(up_all, expected_all, "pre-upgrade DB order mismatch");
    let up_before_midnight: Vec<String> = upgraded
        .list_older_root_messages("chat:19:ts@thread.v2", 10, "2026-11-21T00:00:00Z", None)
        .unwrap()
        .into_iter()
        .map(|m| m.msg_id)
        .collect();
    assert_eq!(up_before_midnight, vec!["a-before"]);
    let up_before_500: Vec<String> = upgraded
        .list_older_root_messages("chat:19:ts@thread.v2", 10, "2026-11-21T00:00:00.500Z", None)
        .unwrap()
        .into_iter()
        .map(|m| m.msg_id)
        .collect();
    assert_eq!(up_before_500, vec!["a-before", "b-exact", "b-kst", "c-001"]);
    // Re-opening upgraded DB keeps exact same row count (5)
    drop(upgraded);
    let reopened = Storage::open(&legacy_db).unwrap();
    assert_eq!(
        reopened.list_root_messages("chat:19:ts@thread.v2", 20, None).unwrap().len(),
        5
    );

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_03_history_cursor_no_gap() {
    #[derive(Default)]
    struct Bug03State {
        /// Stored oldest-first (m01, m02, ...); Graph endpoint serves newest-first.
        messages: RwLock<Vec<(String, String)>>,
        fail_latest: RwLock<bool>,
        requests: RwLock<Vec<String>>,
        base_url: RwLock<String>,
    }

    let state = Arc::new(Bug03State::default());
    let mk_ts = |i: usize| format!("2026-11-21T09:00:{i:02}.000Z");

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
                        "id": "19:pag@thread.v2",
                        "chatType": "group",
                        "topic": "PagChat",
                        "lastUpdatedDateTime": "2026-11-21T09:00:15.000Z",
                        "members": [
                            { "userId": "me-user-1", "displayName": "Me (Tester)" },
                            { "userId": "peer-kim", "displayName": "Alice" }
                        ]
                    }]
                }))
            }),
        )
        .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(
                |State(st): State<Arc<Bug03State>>,
                 Path(chat_id): Path<String>,
                 RawQuery(query): RawQuery| async move {
                    let q = query.unwrap_or_default();
                    st.requests.write().unwrap().push(q.clone());

                    let mut top: usize = 5;
                    let mut after_id: Option<String> = None;
                    for pair in q.split('&').filter(|s| !s.is_empty()) {
                        if let Some((k, v)) = pair.split_once('=') {
                            if k == "$top" {
                                if let Ok(n) = v.parse::<usize>() {
                                    top = n.min(5);
                                }
                            } else if k == "$skiptoken" {
                                if let Some(id) = v.strip_prefix("after:") {
                                    after_id = Some(id.to_string());
                                }
                            }
                        }
                    }

                    if after_id.is_none() && *st.fail_latest.read().unwrap() {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": { "code": "InternalServerError", "message": "transient error" } })),
                        )
                            .into_response();
                    }

                    // Newest-first view
                    let all_desc: Vec<(String, String)> =
                        st.messages.read().unwrap().iter().rev().cloned().collect();
                    let start_idx = match after_id.as_deref() {
                        None => 0,
                        Some(anchor) => all_desc
                            .iter()
                            .position(|(id, _)| id == anchor)
                            .map(|pos| pos + 1)
                            .unwrap_or(all_desc.len()),
                    };
                    let slice: Vec<(String, String)> =
                        all_desc.iter().skip(start_idx).take(top).cloned().collect();
                    let has_more = start_idx + slice.len() < all_desc.len();
                    let base = st.base_url.read().unwrap().clone();
                    let next_link = if has_more {
                        slice.last().map(|(last_id, _)| {
                            format!(
                                "{base}/chats/{}/messages?$top={top}&$skiptoken=after:{last_id}",
                                urlencoding::encode(&chat_id)
                            )
                        })
                    } else {
                        None
                    };

                    let values: Vec<Value> = slice
                        .into_iter()
                        .map(|(id, ts)| {
                            json!({
                                "id": id,
                                "messageType": "message",
                                "createdDateTime": ts,
                                "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": format!("<p>body {id}</p>") },
                                "mentions": [],
                                "attachments": [],
                                "reactions": []
                            })
                        })
                        .collect();

                    let mut body = json!({ "value": values });
                    if let Some(nl) = next_link {
                        body["@odata.nextLink"] = Value::String(nl);
                    }
                    Json(body).into_response()
                },
            ),
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

    let parse_ids = |out: &std::process::Output| -> Vec<String> {
        assert!(
            out.status.success(),
            "cmd failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let j: Value = serde_json::from_slice(&out.stdout).expect("parse read json");
        j["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["msg_id"].as_str().unwrap().to_string())
            .collect()
    };

    // Case 1 (e03_gap): m01..m10 -> read limit 3 -> m11..m15 arrive -> read limit 3 -> first --before m13 --limit 5
    *state.messages.write().unwrap() = (1..=10).map(|i| (format!("m{i:02}"), mk_ts(i))).collect();
    let tmp1 = tempfile::tempdir().unwrap();
    assert!(run_cli(&tmp1, &["chats", "--json"]).await.status.success());
    assert_eq!(
        parse_ids(&run_cli(&tmp1, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m08", "m09", "m10"]
    );

    // Regression control 1: if latest page fetch fails, previous cursor is preserved intact
    let db1_path = tmp1.path().join("data").join("me-user-1").join("cache.db");
    let cursor_before_fail = {
        let st = Storage::open(&db1_path).unwrap();
        st.get_sync_cursor("history:chat:19:pag@thread.v2").unwrap().unwrap()
    };
    *state.fail_latest.write().unwrap() = true;
    let fail_res = run_cli(&tmp1, &["read", "PagChat", "--limit", "3", "--json"]).await;
    assert!(!fail_res.status.success());
    let cursor_after_fail = {
        let st = Storage::open(&db1_path).unwrap();
        st.get_sync_cursor("history:chat:19:pag@thread.v2").unwrap().unwrap()
    };
    assert_eq!(cursor_after_fail.delta_link, cursor_before_fail.delta_link);
    assert_eq!(cursor_after_fail.oldest_msg_ts, cursor_before_fail.oldest_msg_ts);
    *state.fail_latest.write().unwrap() = false;

    // Now m11..m15 arrive and user reads small latest window (limit 3)
    {
        let mut msgs = state.messages.write().unwrap();
        for i in 11..=15 {
            msgs.push((format!("m{i:02}"), mk_ts(i)));
        }
    }
    assert_eq!(
        parse_ids(&run_cli(&tmp1, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m13", "m14", "m15"]
    );

    // First --before m13 --limit 5 MUST return m08..m12 with m11 and m12 each included once
    let first_before =
        parse_ids(&run_cli(&tmp1, &["read", "PagChat", "--before", "m13", "--limit", "5", "--json"]).await);
    assert_eq!(
        first_before,
        vec!["m08", "m09", "m10", "m11", "m12"],
        "first --before m13 must not skip m11..m12"
    );
    // Second call returns the exact same window
    let second_before =
        parse_ids(&run_cli(&tmp1, &["read", "PagChat", "--before", "m13", "--limit", "5", "--json"]).await);
    assert_eq!(second_before, vec!["m08", "m09", "m10", "m11", "m12"]);

    // Case 2: Disconnected cursor (cursor stuck at m08 while DB has m08..m10 + m13..m15) -> recovers m08..m12 on first call
    let tmp_disc = tempfile::tempdir().unwrap();
    *state.messages.write().unwrap() = (1..=10).map(|i| (format!("m{i:02}"), mk_ts(i))).collect();
    assert!(run_cli(&tmp_disc, &["chats", "--json"]).await.status.success());
    assert_eq!(
        parse_ids(&run_cli(&tmp_disc, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m08", "m09", "m10"]
    );
    let saved_stale_cursor = {
        let st = Storage::open(&tmp_disc.path().join("data").join("me-user-1").join("cache.db")).unwrap();
        st.get_sync_cursor("history:chat:19:pag@thread.v2").unwrap().unwrap()
    };
    {
        let mut msgs = state.messages.write().unwrap();
        for i in 11..=15 {
            msgs.push((format!("m{i:02}"), mk_ts(i)));
        }
    }
    assert_eq!(
        parse_ids(&run_cli(&tmp_disc, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m13", "m14", "m15"]
    );
    // Force stale cursor back to after:m08 to test disconnected cursor recovery directly
    {
        let st = Storage::open(&tmp_disc.path().join("data").join("me-user-1").join("cache.db")).unwrap();
        st.save_sync_cursor(&saved_stale_cursor).unwrap();
    }
    let recovered =
        parse_ids(&run_cli(&tmp_disc, &["read", "PagChat", "--before", "m13", "--limit", "5", "--json"]).await);
    assert_eq!(
        recovered,
        vec!["m08", "m09", "m10", "m11", "m12"],
        "disconnected cursor must restart from latest page and cover m11..m12"
    );

    // Regression control 2: No intermediate arrivals -> continues via saved nextLink without repeating page 1
    let tmp2 = tempfile::tempdir().unwrap();
    assert!(run_cli(&tmp2, &["chats", "--json"]).await.status.success());
    assert_eq!(
        parse_ids(&run_cli(&tmp2, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m13", "m14", "m15"]
    );
    state.requests.write().unwrap().clear();
    let ctl_before =
        parse_ids(&run_cli(&tmp2, &["read", "PagChat", "--before", "m13", "--limit", "5", "--json"]).await);
    assert_eq!(ctl_before, vec!["m08", "m09", "m10", "m11", "m12"]);
    let reqs = state.requests.read().unwrap().clone();
    assert!(
        reqs.iter().any(|q| q.contains("$skiptoken=after:m13")),
        "expected continuation via saved nextLink, got requests: {reqs:?}"
    );
    assert!(
        reqs.iter().all(|q| q.contains("$skiptoken=")),
        "must not repeat latest page without skiptoken when cursor connects: {reqs:?}"
    );

    // TEAMS-27: a plain read sees only the newest page. It must not pull a cursor that a
    // `--before` walk already moved deeper back up to that page, nor undo EOF.
    let history_cursor = |tmp: &TempDir| {
        let st = Storage::open(&tmp.path().join("data").join("me-user-1").join("cache.db")).unwrap();
        st.get_sync_cursor("history:chat:19:pag@thread.v2").unwrap().unwrap()
    };
    // tmp2 read m13..m15 (pages of 3) and walked `--before m13` once: the cursor is at m07.
    let deep = history_cursor(&tmp2);
    assert!(
        deep.delta_link.as_deref().is_some_and(|l| l.contains("$skiptoken=after:m07")),
        "unexpected mid-history cursor: {deep:?}"
    );
    assert_eq!(
        parse_ids(&run_cli(&tmp2, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m13", "m14", "m15"]
    );
    let after_plain = history_cursor(&tmp2);
    assert_eq!(after_plain.delta_link, deep.delta_link, "plain read replaced a deeper cursor");
    assert_eq!(after_plain.oldest_msg_ts, deep.oldest_msg_ts);

    // The walk resumes at the deep cursor instead of re-walking from the newest page.
    state.requests.write().unwrap().clear();
    assert_eq!(
        parse_ids(&run_cli(&tmp2, &["read", "PagChat", "--before", "m07", "--limit", "5", "--json"]).await),
        vec!["m02", "m03", "m04", "m05", "m06"]
    );
    let reqs = state.requests.read().unwrap().clone();
    assert!(
        reqs.first().is_some_and(|q| q.contains("$skiptoken=after:m07"))
            && reqs.iter().all(|q| q.contains("$skiptoken=") && !q.contains("after:m13")),
        "walk must resume at the deep cursor: {reqs:?}"
    );

    // That walk reached the end. Reading the newest page again leaves EOF in place.
    assert!(history_cursor(&tmp2).is_history_eof());
    assert_eq!(
        parse_ids(&run_cli(&tmp2, &["read", "PagChat", "--limit", "3", "--json"]).await),
        vec!["m13", "m14", "m15"]
    );
    assert!(history_cursor(&tmp2).is_history_eof(), "plain read undid EOF");

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_12_cache_sufficiency_and_null_topic() {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TopicMode {
        Present,
        ExplicitNull,
        OmittedPartial,
    }

    struct Bug12State {
        chats_count: RwLock<usize>,
        small_msgs_count: RwLock<usize>,
        topic_mode: RwLock<TopicMode>,
        requests: RwLock<Vec<String>>,
        base_url: RwLock<String>,
    }

    let state = Arc::new(Bug12State {
        chats_count: RwLock::new(2),
        small_msgs_count: RwLock::new(5),
        topic_mode: RwLock::new(TopicMode::Present),
        requests: RwLock::new(Vec::new()),
        base_url: RwLock::new(String::new()),
    });

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
            get(
                |State(st): State<Arc<Bug12State>>, RawQuery(query): RawQuery| async move {
                    let q = query.unwrap_or_default();
                    st.requests.write().unwrap().push(format!("/me/chats?{q}"));
                    let total = *st.chats_count.read().unwrap();
                    let mode = *st.topic_mode.read().unwrap();
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

                    let mut all_chats = Vec::new();
                    for i in 1..=total {
                        if i == 1 {
                            let mut obj = json!({
                                "id": "19:small@thread.v2",
                                "chatType": "group",
                                "lastUpdatedDateTime": "2026-11-21T09:00:10Z",
                                "members": [
                                    { "userId": "me-user-1", "displayName": "Me (Tester)" },
                                    { "userId": "peer-kim", "displayName": "Alice" },
                                    { "userId": "peer-lee", "displayName": "Bob" }
                                ]
                            });
                            match mode {
                                TopicMode::Present => {
                                    obj["topic"] = json!("InitialTopic");
                                }
                                TopicMode::ExplicitNull => {
                                    obj["topic"] = Value::Null;
                                }
                                TopicMode::OmittedPartial => {
                                    // topic key intentionally omitted (partial chat payload)
                                }
                            }
                            all_chats.push(obj);
                        } else {
                            all_chats.push(json!({
                                "id": format!("19:c{i}@thread.v2"),
                                "chatType": "group",
                                "topic": format!("Chat {i}"),
                                "lastUpdatedDateTime": format!("2026-11-21T09:00:{i:02}Z"),
                                "members": [
                                    { "userId": "me-user-1", "displayName": "Me (Tester)" },
                                    { "userId": "peer-kim", "displayName": "Alice" }
                                ]
                            }));
                        }
                    }

                    let page: Vec<Value> = all_chats.into_iter().skip(skip).take(top).collect();
                    let next_skip = skip + page.len();
                    let mut body = json!({ "value": page });
                    if next_skip < total {
                        let base = st.base_url.read().unwrap().clone();
                        body["@odata.nextLink"] = Value::String(format!(
                            "{base}/me/chats?$top={top}&$skiptoken={next_skip}"
                        ));
                    }
                    Json(body)
                },
            ),
        )
        .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(
                |State(st): State<Arc<Bug12State>>,
                 Path(chat_id): Path<String>,
                 RawQuery(query): RawQuery| async move {
                    let q = query.unwrap_or_default();
                    st.requests
                        .write()
                        .unwrap()
                        .push(format!("/chats/{chat_id}/messages?{q}"));
                    let total = *st.small_msgs_count.read().unwrap();
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
                    // Serve newest-first
                    let all_desc: Vec<Value> = (1..=total)
                        .rev()
                        .map(|i| {
                            json!({
                                "id": format!("k{i}"),
                                "messageType": "message",
                                "createdDateTime": format!("2026-11-21T09:00:{i:02}.000Z"),
                                "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": format!("<p>msg {i}</p>") },
                                "mentions": [],
                                "attachments": [],
                                "reactions": []
                            })
                        })
                        .collect();
                    let page: Vec<Value> = all_desc.into_iter().skip(skip).take(top).collect();
                    let next_skip = skip + page.len();
                    let mut body = json!({ "value": page });
                    if next_skip < total {
                        let base = st.base_url.read().unwrap().clone();
                        body["@odata.nextLink"] = Value::String(format!(
                            "{base}/chats/{}/messages?$top={top}&$skiptoken={next_skip}",
                            urlencoding::encode(&chat_id)
                        ));
                    }
                    Json(body)
                },
            ),
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

    let run_cli = |tmp: &TempDir, args: &[&str], cache_secs: Option<&str>| {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let graph_base = graph_base.clone();
        let auth_base = auth_base.clone();
        let jwt = jwt.clone();
        let cfg_dir = tmp.path().join("config");
        let data_dir = tmp.path().join("data");
        let cache_secs = cache_secs.map(str::to_string);
        async move {
            let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_teams"));
            cmd.args(&owned)
                .env("TEAMS_GRAPH_BASE_URL", &graph_base)
                .env("TEAMS_AUTH_BASE_URL", &auth_base)
                .env("TEAMS_ACCESS_TOKEN", &jwt)
                .env("TEAMS_DISABLE_KEYRING", "1")
                .env("TEAMS_CONFIG_DIR", &cfg_dir)
                .env("TEAMS_DATA_DIR", &data_dir)
                .env("TEAMS_AUTO_DAEMON", "0")
                .env_remove("TEAMS_TOKEN_FILE")
                .env_remove("TEAMS_TOKEN_REFRESH_CMD");
            if let Some(secs) = cache_secs {
                cmd.env("TEAMS_CLI_CACHE_SECS", secs);
            } else {
                cmd.env_remove("TEAMS_CLI_CACHE_SECS");
            }
            cmd.output().await.expect("run teams CLI")
        }
    };

    // Case A: 2 cached messages, 5 on server, --cached --limit 5 -> hits Graph and returns 5
    let tmp_a = tempfile::tempdir().unwrap();
    assert!(run_cli(&tmp_a, &["chats", "--json"], None).await.status.success());
    let r2 = run_cli(&tmp_a, &["read", "InitialTopic", "--limit", "2", "--json"], None).await;
    assert!(r2.status.success());
    let j2: Value = serde_json::from_slice(&r2.stdout).unwrap();
    assert_eq!(j2["messages"].as_array().unwrap().len(), 2);

    // Control: --offline with insufficient cache makes 0 Graph requests
    state.requests.write().unwrap().clear();
    let r_off = run_cli(&tmp_a, &["read", "InitialTopic", "--offline", "--limit", "5", "--json"], None).await;
    assert!(r_off.status.success());
    let j_off: Value = serde_json::from_slice(&r_off.stdout).unwrap();
    assert_eq!(j_off["messages"].as_array().unwrap().len(), 2);
    assert!(state.requests.read().unwrap().is_empty(), "--offline must make 0 Graph requests");

    // Now --cached --limit 5 must fall back to Graph and return 5
    state.requests.write().unwrap().clear();
    let r5 = run_cli(&tmp_a, &["read", "InitialTopic", "--cached", "--limit", "5", "--json"], None).await;
    assert!(r5.status.success(), "stderr={}", String::from_utf8_lossy(&r5.stderr));
    let j5: Value = serde_json::from_slice(&r5.stdout).unwrap();
    assert_eq!(
        j5["messages"].as_array().unwrap().len(),
        5,
        "--cached --limit 5 must fetch missing messages from Graph: {j5}"
    );
    assert!(!state.requests.read().unwrap().is_empty());

    // Case B: Server has only 2 messages & 2 chats (EOF reached) -> 2nd read/chats within TTL makes 0 requests
    *state.small_msgs_count.write().unwrap() = 2;
    *state.chats_count.write().unwrap() = 2;
    let tmp_b = tempfile::tempdir().unwrap();
    let c1 = run_cli(&tmp_b, &["chats", "--limit", "50", "--json"], Some("60")).await;
    assert!(c1.status.success());
    state.requests.write().unwrap().clear();
    let c2 = run_cli(&tmp_b, &["chats", "--limit", "50", "--json"], Some("60")).await;
    assert!(c2.status.success());
    assert!(
        state.requests.read().unwrap().is_empty(),
        "2nd chats within TTL after EOF must make 0 Graph requests, got: {:?}",
        *state.requests.read().unwrap()
    );
    let m1 = run_cli(&tmp_b, &["read", "InitialTopic", "--limit", "30", "--json"], Some("60")).await;
    assert!(m1.status.success());
    state.requests.write().unwrap().clear();
    let m2 = run_cli(&tmp_b, &["read", "InitialTopic", "--limit", "30", "--json"], Some("60")).await;
    assert!(m2.status.success());
    assert!(
        state.requests.read().unwrap().is_empty(),
        "2nd read within TTL after EOF must make 0 Graph requests, got: {:?}",
        *state.requests.read().unwrap()
    );
    // Also --cached after EOF makes 0 requests
    let m3 = run_cli(&tmp_b, &["read", "InitialTopic", "--cached", "--limit", "30", "--json"], None).await;
    assert!(m3.status.success());
    assert!(
        state.requests.read().unwrap().is_empty(),
        "--cached after EOF must make 0 Graph requests"
    );

    // Case C: Partial chat response omitting `topic` preserves stored topic; explicit `topic: null` clears it
    let tmp_c = tempfile::tempdir().unwrap();
    *state.topic_mode.write().unwrap() = TopicMode::Present;
    assert!(run_cli(&tmp_c, &["chats", "--refresh", "--json"], None).await.status.success());

    // Partial response omitting `topic` must NOT clear existing topic
    *state.topic_mode.write().unwrap() = TopicMode::OmittedPartial;
    assert!(run_cli(&tmp_c, &["chats", "--refresh", "--json"], None).await.status.success());
    let after_partial = run_cli(&tmp_c, &["chats", "--cached", "--json"], None).await;
    assert!(after_partial.status.success());
    let jp: Value = serde_json::from_slice(&after_partial.stdout).unwrap();
    let small_partial = jp["chats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["conv_key"] == "chat:19:small@thread.v2")
        .unwrap();
    assert_eq!(
        small_partial["topic"], "InitialTopic",
        "partial response omitting topic must preserve stored topic: {small_partial}"
    );

    // Full response with explicit `topic: null` MUST clear SQLite topic to NULL
    *state.topic_mode.write().unwrap() = TopicMode::ExplicitNull;
    assert!(run_cli(&tmp_c, &["chats", "--refresh", "--json"], None).await.status.success());
    let after_null = run_cli(&tmp_c, &["chats", "--cached", "--json"], None).await;
    assert!(after_null.status.success());
    let jn: Value = serde_json::from_slice(&after_null.stdout).unwrap();
    let small_null = jn["chats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["conv_key"] == "chat:19:small@thread.v2")
        .unwrap();
    assert!(
        small_null["topic"].is_null(),
        "explicit topic=null must clear SQLite topic: {small_null}"
    );
    assert_eq!(small_null["title"], "Alice, Bob");

    server.abort();
}
