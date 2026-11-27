//! Soft-delete cache/export, channel thread windows, reply counts, and thread export error reporting E2E tests.

use super::server::*;
use tempfile::TempDir;
use std::sync::{Arc, RwLock};
use axum::{response::IntoResponse, extract::{Path, RawQuery, State}, http::StatusCode, routing::{get, post}, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use teams_cli::{mcp, models::MessageItem, storage::Storage};

#[tokio::test]
async fn regression_teams_bug_06_soft_delete_cache_and_export() {
    #[derive(Default)]
    struct Bug06State {
        fail_delete: RwLock<bool>,
        deleted_ids: RwLock<Vec<String>>,
    }

    let state = Arc::new(Bug06State::default());

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
        .route("/v1.0/me/chats", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async {
                Json(json!({
                    "value": [{ "id": "t1", "displayName": "CoreEng" }]
                }))
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
            "/v1.0/teams/t1/channels/ch1/messages",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "m-normal",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T09:00:01.000Z",
                            "subject": "Normal Notice",
                            "importance": "normal",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": "<p>Keep this normal body</p><attachment id=\"att-n\"></attachment>" },
                            "attachments": [{
                                "id": "att-n",
                                "contentType": "reference",
                                "contentUrl": "https://contoso.sharepoint.com/normal-doc.pdf",
                                "name": "normal-doc.pdf"
                            }],
                            "mentions": [],
                            "reactions": [],
                            "replies": []
                        },
                        {
                            "id": "m-del",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T09:00:02.000Z",
                            "subject": "Confidential Subject",
                            "importance": "high",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": "<p>Budget attached secret-token-06 <at id=\"0\">Alice Kim</at></p><attachment id=\"att9\"></attachment>" },
                            "attachments": [{
                                "id": "att9",
                                "contentType": "reference",
                                "contentUrl": "https://contoso.sharepoint.com/secret-budget.pdf",
                                "name": "secret-budget.pdf"
                            }],
                            "mentions": [{
                                "id": 0,
                                "mentionText": "Alice Kim",
                                "mentioned": { "user": { "id": "u-alice", "displayName": "Alice Kim" } }
                            }],
                            "reactions": [{
                                "reactionType": "like",
                                "createdDateTime": "2026-11-21T09:00:03.000Z",
                                "user": { "user": { "id": "u-alice", "displayName": "Alice Kim" } }
                            }],
                            "replies": []
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/teams/t1/channels/ch1/messages/{msg_id}/softDelete",
            post(
                |State(st): State<Arc<Bug06State>>, Path(msg_id): Path<String>| async move {
                    if *st.fail_delete.read().unwrap() {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": { "code": "InternalServerError", "message": "server boom" } })),
                        )
                            .into_response();
                    }
                    st.deleted_ids.write().unwrap().push(msg_id);
                    StatusCode::NO_CONTENT.into_response()
                },
            ),
        )
        .with_state(state.clone());

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

    // Populate cache via read
    let out = run_cli(&["read", "CoreEng/General", "--json"]).await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    // Control 1: server error on delete keeps original message and search hit intact
    *state.fail_delete.write().unwrap() = true;
    let failed_del = run_cli(&["delete", "CoreEng/General", "m-del", "--yes", "--json"]).await;
    assert!(!failed_del.status.success());
    let search_before = run_cli(&["search", "secret-token-06", "--json"]).await;
    assert!(search_before.status.success());
    let sb_json: Value = serde_json::from_slice(&search_before.stdout).unwrap();
    assert_eq!(sb_json["hits"].as_array().unwrap().len(), 1);

    // Now succeed delete
    *state.fail_delete.write().unwrap() = false;
    let del_out = run_cli(&["delete", "CoreEng/General", "m-del", "--yes", "--json"]).await;
    assert!(del_out.status.success(), "stderr={}", String::from_utf8_lossy(&del_out.stderr));

    // Offline read (JSON --full): deleted=true, IDs & created_at preserved, metadata cleared
    let read_json_out = run_cli(&["read", "CoreEng/General", "--offline", "--json", "--full"]).await;
    assert!(read_json_out.status.success());
    let rj: Value = serde_json::from_slice(&read_json_out.stdout).unwrap();
    let del_msg = rj["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["msg_id"] == "m-del")
        .expect("deleted row preserved");
    assert_eq!(del_msg["deleted"], true);
    assert_eq!(del_msg["created_at"], "2026-11-21T09:00:02.000Z");
    assert_eq!(del_msg["body_plain"], "(deleted message)");
    assert!(del_msg["subject"].is_null(), "subject must be cleared: {del_msg}");
    assert!(del_msg["attachments"].as_array().unwrap().is_empty(), "attachments must be cleared: {del_msg}");
    assert!(del_msg["mentions"].as_array().unwrap().is_empty(), "mentions must be cleared: {del_msg}");
    assert!(del_msg["reactions"].as_array().unwrap().is_empty(), "reactions must be cleared: {del_msg}");

    // Offline read (text): has "(deleted message)" and no old subject/file/reaction
    let read_txt_out = run_cli(&["read", "CoreEng/General", "--offline"]).await;
    assert!(read_txt_out.status.success());
    let read_txt = String::from_utf8_lossy(&read_txt_out.stdout);
    let del_line = read_txt
        .lines()
        .find(|l| l.contains("m-del"))
        .expect("deleted message line present");
    assert!(del_line.contains("(deleted message)"), "{del_line}");
    assert!(!del_line.contains("Confidential Subject"), "{del_line}");
    assert!(!del_line.contains("secret-budget.pdf"), "{del_line}");
    assert!(!del_line.contains("👍"), "{del_line}");

    // Offline export md: deleted marker present, old metadata absent, normal message intact
    let md_path = tmp.path().join("off.md");
    let exp_out = run_cli(&[
        "export",
        "CoreEng/General",
        "--offline",
        "--format",
        "md",
        "--out",
        md_path.to_str().unwrap(),
    ])
    .await;
    assert!(exp_out.status.success(), "stderr={}", String::from_utf8_lossy(&exp_out.stderr));
    let md_text = std::fs::read_to_string(&md_path).unwrap();
    assert!(md_text.contains("(deleted message)"), "{md_text}");
    assert!(!md_text.contains("Confidential Subject"), "{md_text}");
    assert!(!md_text.contains("secret-budget.pdf"), "{md_text}");
    // Regression control: normal message keeps subject & attachment
    assert!(md_text.contains("## Normal Notice"), "{md_text}");
    assert!(md_text.contains("normal-doc.pdf"), "{md_text}");

    // Search must not match deleted body or old subject
    for q in ["secret-token-06", "Confidential Subject"] {
        let s_out = run_cli(&["search", q, "--json"]).await;
        assert!(s_out.status.success());
        let sj: Value = serde_json::from_slice(&s_out.stdout).unwrap();
        assert!(
            sj["hits"].as_array().unwrap().is_empty(),
            "deleted message must not appear in search for '{q}': {sj}"
        );
    }

    // Legacy pre-upgrade deleted row in SQLite (deleted=1 with stale subject/attachments/reactions)
    let db_path = data_dir.join("me-user-1").join("cache.db");
    assert!(db_path.exists(), "expected user cache db at {}", db_path.display());
    let storage = Storage::open(&db_path).unwrap();
    {
        let conn = storage.conn().lock().unwrap();
        conn.execute(
            "INSERT INTO messages (
                msg_key, conv_key, msg_id, parent_msg_id, subject, author_id, author_name,
                is_me, created_at, deleted, importance, message_type, body_html, body_plain,
                attachments_json, mentions_json, reactions_json, reply_count, has_mentions
            ) VALUES (
                'chan:t1:ch1|m-legacy-del', 'chan:t1:ch1', 'm-legacy-del', NULL,
                'Legacy Stale Subject', 'me-user-1', 'Me (Tester)',
                1, '2026-11-21T09:00:03.000Z', 1, 'high', 'message', '', '(deleted message)',
                '[{\"id\":\"att-leg\",\"content_type\":\"reference\",\"name\":\"legacy-file.pdf\"}]',
                '[]',
                '[{\"reaction_type\":\"like\",\"count\":2,\"me_reacted\":true,\"users\":[\"Alice\"]}]',
                0, 0
            )",
            [],
        )
        .unwrap();
    }
    drop(storage);

    let legacy_read = run_cli(&["read", "CoreEng/General", "--offline"]).await;
    assert!(legacy_read.status.success());
    let legacy_read_txt = String::from_utf8_lossy(&legacy_read.stdout);
    let legacy_line = legacy_read_txt
        .lines()
        .find(|l| l.contains("m-legacy-del"))
        .expect("legacy deleted line present");
    assert!(legacy_line.contains("(deleted message)"), "{legacy_line}");
    assert!(!legacy_line.contains("Legacy Stale Subject"), "{legacy_line}");
    assert!(!legacy_line.contains("legacy-file.pdf"), "{legacy_line}");
    assert!(!legacy_line.contains("👍"), "{legacy_line}");

    let legacy_md_path = tmp.path().join("legacy.md");
    let legacy_exp = run_cli(&[
        "export",
        "CoreEng/General",
        "--offline",
        "--format",
        "md",
        "--out",
        legacy_md_path.to_str().unwrap(),
    ])
    .await;
    assert!(legacy_exp.status.success());
    let legacy_md = std::fs::read_to_string(&legacy_md_path).unwrap();
    assert!(!legacy_md.contains("Legacy Stale Subject"), "{legacy_md}");
    assert!(!legacy_md.contains("legacy-file.pdf"), "{legacy_md}");
    assert_eq!(legacy_md.matches("(deleted message)").count(), 2, "{legacy_md}");

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_16_read_thread_before_conflict() {
    #[derive(Default)]
    struct Bug16State {
        request_count: RwLock<usize>,
    }

    let state = Arc::new(Bug16State::default());

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|State(st): State<Arc<Bug16State>>| async move {
                *st.request_count.write().unwrap() += 1;
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
            get(|State(st): State<Arc<Bug16State>>| async move {
                *st.request_count.write().unwrap() += 1;
                Json(json!({ "value": [] }))
            }),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|State(st): State<Arc<Bug16State>>| async move {
                *st.request_count.write().unwrap() += 1;
                Json(json!({ "value": [{ "id": "t1", "displayName": "CoreEng" }] }))
            }),
        )
        .route(
            "/v1.0/$batch",
            post(|State(st): State<Arc<Bug16State>>, Json(payload): Json<Value>| async move {
                *st.request_count.write().unwrap() += 1;
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
            "/v1.0/teams/t1/channels/ch1/messages",
            get(|State(st): State<Arc<Bug16State>>| async move {
                *st.request_count.write().unwrap() += 1;
                Json(json!({
                    "value": [{
                        "id": "root-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-20T09:00:00Z",
                        "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                        "body": { "contentType": "html", "content": "<p>root</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": [],
                        "replies": []
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/teams/t1/channels/ch1/messages/root-1",
            get(|State(st): State<Arc<Bug16State>>| async move {
                *st.request_count.write().unwrap() += 1;
                Json(json!({
                    "id": "root-1",
                    "messageType": "message",
                    "createdDateTime": "2026-11-20T09:00:00Z",
                    "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                    "body": { "contentType": "html", "content": "<p>root</p>" },
                    "mentions": [],
                    "attachments": [],
                    "reactions": []
                }))
            }),
        )
        .route(
            "/v1.0/teams/t1/channels/ch1/messages/root-1/replies",
            get(|State(st): State<Arc<Bug16State>>| async move {
                *st.request_count.write().unwrap() += 1;
                Json(json!({
                    "value": [{
                        "id": "rep-1",
                        "replyToId": "root-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-21T09:00:12Z",
                        "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                        "body": { "contentType": "html", "content": "<p>reply 1</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }]
                }))
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

    // Both flag orders must exit with code 2, 0 Graph requests, and mention both --thread and --before in stderr
    for bad_args in [
        &["read", "CoreEng/General", "--thread", "root-1", "--before", "2026-11-21"][..],
        &["read", "CoreEng/General", "--before", "2026-11-21", "--thread", "root-1"][..],
    ] {
        let out = run_cli(bad_args).await;
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit code 2 for {bad_args:?}, got {:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--thread") && stderr.contains("--before"),
            "expected both conflicting option names in stderr, got: {stderr}"
        );
    }
    assert_eq!(
        *state.request_count.read().unwrap(),
        0,
        "conflicting flags must make 0 Graph requests"
    );

    // Help documents both query forms
    let help = run_cli(&["read", "--help"]).await;
    assert!(help.status.success());
    let help_txt = String::from_utf8_lossy(&help.stdout);
    assert!(
        help_txt.contains("--thread") && help_txt.contains("--before"),
        "{help_txt}"
    );

    // Regression controls: --before alone and --thread alone both succeed
    let before_only = run_cli(&["read", "CoreEng/General", "--before", "2026-11-21", "--json"]).await;
    assert!(
        before_only.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&before_only.stderr)
    );
    let bj: Value = serde_json::from_slice(&before_only.stdout).unwrap();
    assert_eq!(bj["messages"][0]["msg_id"], "root-1");

    let thread_only = run_cli(&["read", "CoreEng/General", "--thread", "root-1", "--json"]).await;
    assert!(
        thread_only.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&thread_only.stderr)
    );
    let tj: Value = serde_json::from_slice(&thread_only.stdout).unwrap();
    assert_eq!(tj["thread"]["replies"][0]["msg_id"], "rep-1");

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_18_thread_window_reply_count() {
    #[derive(Default)]
    struct Bug18State {
        base_url: RwLock<String>,
    }

    let state = Arc::new(Bug18State::default());

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
        .route("/v1.0/me/chats", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async {
                Json(json!({
                    "value": [{ "id": "team-a", "displayName": "Engineering" }]
                }))
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
                            "body": { "value": [{ "id": "chan-general", "displayName": "General", "membershipType": "standard" }] }
                        })
                    })
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/teams/team-a/channels/chan-general/messages/root-1",
            get(|| async {
                Json(json!({
                    "id": "root-1",
                    "messageType": "message",
                    "createdDateTime": "2026-11-20T09:30:00Z",
                    "subject": "Q4 Rollout Plan",
                    "importance": "high",
                    "from": { "user": { "id": "u-alice", "displayName": "Alice Kim" } },
                    "body": { "contentType": "html", "content": "<p>Rollout plan body</p>" },
                    "mentions": [],
                    "attachments": [],
                    "reactions": []
                }))
            }),
        )
        .route(
            "/v1.0/teams/team-a/channels/chan-general/messages/root-1/replies",
            get(|State(st): State<Arc<Bug18State>>, RawQuery(query): RawQuery| async move {
                let q = query.unwrap_or_default();
                let base = st.base_url.read().unwrap().clone();
                let all_newest_first = vec![
                    json!({
                        "id": "reply-3",
                        "replyToId": "root-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-20T10:03:00Z",
                        "from": { "user": { "id": "u-alice", "displayName": "Alice Kim" } },
                        "body": { "contentType": "html", "content": "<p>reply number 3</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }),
                    json!({
                        "id": "reply-2",
                        "replyToId": "root-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-20T10:02:00Z",
                        "from": { "user": { "id": "u-alice", "displayName": "Alice Kim" } },
                        "body": { "contentType": "html", "content": "<p>reply number 2</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }),
                    json!({
                        "id": "reply-1",
                        "replyToId": "root-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-20T10:01:00Z",
                        "from": { "user": { "id": "u-alice", "displayName": "Alice Kim" } },
                        "body": { "contentType": "html", "content": "<p>reply number 1</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }),
                ];

                if q.contains("page=2") {
                    return Json(json!({ "value": [all_newest_first[2]] }));
                }
                let top = q
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("$top=").or_else(|| kv.strip_prefix("%24top=")))
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(50);
                if top < all_newest_first.len() {
                    Json(json!({
                        "value": all_newest_first[..top],
                        "@odata.nextLink": format!("{base}/teams/team-a/channels/chan-general/messages/root-1/replies?page=2")
                    }))
                } else {
                    Json(json!({ "value": all_newest_first }))
                }
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

    // limit 2 -> truncated: reply_count null, returned 2, has_more true
    let out2 = run_cli(&tmp, &["thread", "Engineering/General", "root-1", "--limit", "2", "--json"]).await;
    assert!(out2.status.success(), "stderr={}", String::from_utf8_lossy(&out2.stderr));
    let j2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert!(j2["thread"]["root"]["reply_count"].is_null(), "{j2}");
    assert_eq!(j2["thread"]["returned"], 2);
    assert_eq!(j2["thread"]["has_more"], true);
    assert_eq!(j2["thread"]["replies"][0]["msg_id"], "reply-2");
    assert_eq!(j2["thread"]["replies"][1]["msg_id"], "reply-3");

    // limit 3 and limit 50 -> complete: reply_count 3, has_more false
    for lim in ["3", "50"] {
        let out = run_cli(&tmp, &["thread", "Engineering/General", "root-1", "--limit", lim, "--json"]).await;
        assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
        let j: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(j["thread"]["root"]["reply_count"], 3, "limit={lim} {j}");
        assert_eq!(j["thread"]["returned"], 3);
        assert_eq!(j["thread"]["has_more"], false);
    }

    // TEAMS-23: sync caches a newer reply after the complete online read above. The offline
    // read must return it and count it, instead of answering from the stale reply window.
    {
        let conv_key = "chan:team-a:chan-general";
        let storage =
            Storage::open(&tmp.path().join("data").join("me-user-1").join("cache.db")).unwrap();
        storage
            .upsert_messages(&[MessageItem {
                msg_key: MessageItem::make_msg_key(conv_key, "reply-4"),
                conv_key: conv_key.to_string(),
                msg_id: "reply-4".to_string(),
                parent_msg_id: Some("root-1".to_string()),
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u-alice".to_string()),
                author_name: "Alice Kim".to_string(),
                is_me: false,
                created_at: "2026-11-20T10:04:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>reply number 4</p>".to_string(),
                body_plain: "reply number 4".to_string(),
                attachments: Vec::new(),
                mentions: Vec::new(),
                reactions: Vec::new(),
                reply_count: 0,
                web_url: None,
            }])
            .unwrap();
    }
    let off_synced = run_cli(
        &tmp,
        &["thread", "Engineering/General", "root-1", "--offline", "--limit", "50", "--json"],
    )
    .await;
    assert!(off_synced.status.success(), "stderr={}", String::from_utf8_lossy(&off_synced.stderr));
    let j_synced: Value = serde_json::from_slice(&off_synced.stdout).unwrap();
    assert_eq!(j_synced["thread"]["root"]["reply_count"], 4, "{j_synced}");
    assert_eq!(j_synced["thread"]["returned"], 4);
    assert_eq!(j_synced["thread"]["has_more"], false);
    assert_eq!(j_synced["thread"]["replies"][3]["msg_id"], "reply-4");

    // Text output with --limit 2 -> includes "more exist", no "[💬"
    let txt_out = run_cli(&tmp, &["thread", "Engineering/General", "root-1", "--limit", "2"]).await;
    assert!(txt_out.status.success());
    let txt = String::from_utf8_lossy(&txt_out.stdout);
    assert!(txt.contains("more exist"), "{txt}");
    assert!(!txt.contains("[💬"), "{txt}");

    // Fresh profile: online --limit 2 -> offline --limit 2 (ok) and offline --limit 3 (exit 1)
    let tmp2 = tempfile::tempdir().unwrap();
    assert!(
        run_cli(&tmp2, &["thread", "Engineering/General", "root-1", "--limit", "2", "--json"])
            .await
            .status
            .success()
    );
    let off2 = run_cli(
        &tmp2,
        &["thread", "Engineering/General", "root-1", "--offline", "--limit", "2", "--json"],
    )
    .await;
    assert!(off2.status.success(), "stderr={}", String::from_utf8_lossy(&off2.stderr));
    let j_off2: Value = serde_json::from_slice(&off2.stdout).unwrap();
    assert!(j_off2["thread"]["root"]["reply_count"].is_null());
    assert_eq!(j_off2["thread"]["returned"], 2);
    assert_eq!(j_off2["thread"]["has_more"], true);

    let off3 = run_cli(
        &tmp2,
        &["thread", "Engineering/General", "root-1", "--offline", "--limit", "3", "--json"],
    )
    .await;
    assert_eq!(off3.status.code(), Some(1));
    let j_off3: Value = serde_json::from_slice(&off3.stdout)
        .expect("failed --json command must emit structured error JSON on stdout");
    assert_eq!(j_off3["ok"], false);
    assert_eq!(j_off3["error"]["code"], "error");
    assert!(
        j_off3["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("reply limit 3"),
        "{j_off3}"
    );
    assert!(String::from_utf8_lossy(&off3.stderr).contains("reply limit 3"));

    // MCP get_thread with limit 2
    let dl_tmp = tempfile::tempdir().unwrap();
    let (cfg, graph) = build_direct_graph_client(&auth_base, dl_tmp.path(), &jwt);
    let storage = Storage::open_in_memory().unwrap();
    let mcp_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "get_thread",
                "arguments": { "target": "Engineering/General", "root_msg_id": "root-1", "limit": 2 }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    let mcp_payload: Value =
        serde_json::from_str(mcp_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(mcp_payload["thread"]["root"]["reply_count"].is_null());
    assert_eq!(mcp_payload["thread"]["returned"], 2);
    assert_eq!(mcp_payload["thread"]["has_more"], true);
    assert!(mcp_payload["hint"].as_str().unwrap().contains("get_thread"));

    server.abort();
}

#[tokio::test]
async fn regression_teams_21_export_include_threads_reports_failed_replies() {
    let fail_replies = Arc::new(RwLock::new(true));
    let fail = fail_replies.clone();

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|| async {
                Json(json!({ "id": "me-user-1", "displayName": "Me (Tester)", "mail": "tester@contoso.com", "userPrincipalName": "tester@contoso.com" }))
            }),
        )
        .route("/v1.0/me/chats", get(|| async { Json(json!({ "value": [] })) }))
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async { Json(json!({ "value": [{ "id": "t1", "displayName": "CoreEng" }] })) }),
        )
        .route(
            "/v1.0/$batch",
            post(|Json(payload): Json<Value>| async move {
                let responses: Vec<Value> = payload["requests"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|req| json!({
                        "id": req["id"].as_str().unwrap_or("0"),
                        "status": 200,
                        "body": { "value": [{ "id": "ch1", "displayName": "General", "membershipType": "standard" }] }
                    }))
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/teams/t1/channels/ch1/messages",
            get(|| async {
                Json(json!({ "value": [
                    mock_graph_message("root-good", "2026-11-21T09:00:01.000Z", "good root", None,
                        Some(vec![mock_graph_message("good-r1", "2026-11-21T09:01:00.000Z", "inline good reply", Some("root-good"), None)])),
                    mock_graph_message("root-bad", "2026-11-21T09:00:02.000Z", "bad root", None,
                        Some(vec![mock_graph_message("bad-r1", "2026-11-21T09:02:00.000Z", "inline bad reply", Some("root-bad"), None)])),
                ] }))
            }),
        )
        .route(
            "/v1.0/teams/t1/channels/ch1/messages/{root_id}/replies",
            get(move |Path(root_id): Path<String>| {
                let fail = fail.clone();
                async move {
                    if root_id == "root-bad" && *fail.read().unwrap() {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": { "code": "generalException", "message": "replies backend down" } })),
                        )
                            .into_response();
                    }
                    let reply = mock_graph_message(
                        &format!("{root_id}-fetched"),
                        "2026-11-21T09:05:00.000Z",
                        &format!("fetched reply of {root_id}"),
                        Some(&root_id),
                        None,
                    );
                    Json(json!({ "value": [reply] })).into_response()
                }
            }),
        );
    let (base_url, server) = spawn_test_router(app).await;
    let cli = CliEnv::new(&base_url);

    let out_path = cli.path("partial.json");
    let out = cli
        .run(&["export", "CoreEng/General", "--format", "json", "--include-threads", "--out", out_path.to_str().unwrap()])
        .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a partial export must not exit 0; stderr={stderr}");
    assert!(stderr.contains("root-bad"), "stderr names the failed root: {stderr}");
    assert!(stderr.contains("export incomplete: 1 of 2 threads could not be fetched"), "{stderr}");
    // The partial export is still written, with the healthy thread and an explicit gap list.
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&out_path).unwrap()).unwrap();
    let text = doc.to_string();
    assert!(text.contains("fetched reply of root-good"), "{text}");
    let incomplete = doc["incomplete_threads"].as_array().expect("incomplete_threads present");
    assert_eq!(incomplete.len(), 1, "{incomplete:?}");
    assert_eq!(incomplete[0]["root_msg_id"], "root-bad");
    assert!(incomplete[0]["error"].as_str().unwrap().contains("500"), "{incomplete:?}");

    // Control: with every thread fetched the export is clean — exit 0 and no extra key.
    *fail_replies.write().unwrap() = false;
    let ok_path = cli.path("complete.json");
    let ok = cli
        .run(&["export", "CoreEng/General", "--format", "json", "--include-threads", "--out", ok_path.to_str().unwrap()])
        .await;
    assert!(ok.status.success(), "stderr={}", String::from_utf8_lossy(&ok.stderr));
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&ok_path).unwrap()).unwrap();
    assert!(doc.get("incomplete_threads").is_none(), "{doc}");
    assert!(doc.to_string().contains("fetched reply of root-bad"));

    server.abort();
}
