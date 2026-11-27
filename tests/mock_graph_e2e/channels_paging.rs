//! Team/channel slash resolution, joined-teams 403, team/channel/member pagination, and batch failure E2E tests.

use super::server::*;
use tempfile::TempDir;
use std::{collections::HashMap, sync::{Arc, RwLock}};
use axum::{response::IntoResponse, extract::{Path, RawQuery, State}, http::StatusCode, routing::{get, post}, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use teams_cli::{mcp, storage::Storage};

#[tokio::test]
async fn regression_teams_bug_11_slash_resolution_and_joined_teams_403() {
    #[derive(Default)]
    struct Bug11State {
        include_exact_ui_ux_channel: RwLock<bool>,
        joined_teams_403: RwLock<bool>,
        posted_chats: RwLock<Vec<(String, Value)>>,
        posted_channels: RwLock<Vec<(String, String, Value)>>,
    }

    let state = Arc::new(Bug11State::default());

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
                        "id": "19:uiux@thread.v2",
                        "chatType": "group",
                        "topic": "UI/UX",
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
            "/v1.0/chats/{chat_id}/messages",
            get(|Path(_chat_id): Path<String>| async {
                Json(json!({
                    "value": [{
                        "id": "chat-seed-1",
                        "messageType": "message",
                        "createdDateTime": "2026-11-21T05:00:00Z",
                        "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                        "body": { "contentType": "html", "content": "<p>chat seed</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": []
                    }]
                }))
            })
            .post(
                |State(st): State<Arc<Bug11State>>,
                 Path(chat_id): Path<String>,
                 Json(payload): Json<Value>| async move {
                    st.posted_chats
                        .write()
                        .unwrap()
                        .push((chat_id.clone(), payload.clone()));
                    let content = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "chat-sent-1",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T05:10:00Z",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": content },
                            "mentions": [],
                            "attachments": [],
                            "reactions": []
                        })),
                    )
                },
            ),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|State(st): State<Arc<Bug11State>>| async move {
                if *st.joined_teams_403.read().unwrap() {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "error": {
                                "code": "Authorization_RequestDenied",
                                "message": "Insufficient privileges to complete the operation."
                            }
                        })),
                    )
                        .into_response();
                }
                let mut teams = vec![
                    json!({ "id": "t-ui", "displayName": "UI Team" }),
                    json!({ "id": "t-org", "displayName": "Org/Platform" }),
                ];
                if *st.include_exact_ui_ux_channel.read().unwrap() {
                    teams.push(json!({ "id": "t-exact", "displayName": "UI" }));
                }
                Json(json!({ "value": teams })).into_response()
            }),
        )
        .route(
            "/v1.0/$batch",
            post(
                |State(st): State<Arc<Bug11State>>, Json(payload): Json<Value>| async move {
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
                            if url.contains("/teams/t-ui/channels") {
                                json!({
                                    "id": req_id,
                                    "status": 200,
                                    "body": { "value": [{ "id": "ch-ux", "displayName": "UX Review", "membershipType": "standard" }] }
                                })
                            } else if url.contains("/teams/t-org/channels") {
                                json!({
                                    "id": req_id,
                                    "status": 200,
                                    "body": {
                                        "value": [
                                            { "id": "ch-g", "displayName": "General", "membershipType": "standard" },
                                            { "id": "ch-g2", "displayName": "General 2", "membershipType": "standard" }
                                        ]
                                    }
                                })
                            } else if url.contains("/teams/t-exact/channels")
                                && *st.include_exact_ui_ux_channel.read().unwrap()
                            {
                                json!({
                                    "id": req_id,
                                    "status": 200,
                                    "body": { "value": [{ "id": "ch-exact-ux", "displayName": "UX", "membershipType": "standard" }] }
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
                },
            ),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            get(|| async { Json(json!({ "value": [] })) }).post(
                |State(st): State<Arc<Bug11State>>,
                 Path((team_id, channel_id)): Path<(String, String)>,
                 Json(payload): Json<Value>| async move {
                    st.posted_channels.write().unwrap().push((
                        team_id,
                        channel_id,
                        payload.clone(),
                    ));
                    let content = payload
                        .pointer("/body/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "chan-sent-1",
                            "messageType": "message",
                            "createdDateTime": "2026-11-21T05:11:00Z",
                            "from": { "user": { "id": "me-user-1", "displayName": "Me (Tester)" } },
                            "body": { "contentType": "html", "content": content },
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

    // Case 1: Chat "UI/UX" coexists with Channel "UI Team/UX Review" -> send "UI/UX" posts to chat (1) and channel (0)
    let tmp1 = tempfile::tempdir().unwrap();
    assert!(run_cli(&tmp1, &["chats", "--refresh", "--json"]).await.status.success());
    assert!(run_cli(&tmp1, &["teams", "--refresh", "--json"]).await.status.success());

    let out = run_cli(&tmp1, &["send", "UI/UX", "hello uiux", "--json"]).await;
    assert!(
        out.status.success(),
        "send UI/UX failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(state.posted_chats.read().unwrap().len(), 1);
    assert_eq!(state.posted_chats.read().unwrap()[0].0, "19:uiux@thread.v2");
    assert_eq!(state.posted_channels.read().unwrap().len(), 0);

    // Case 2: Team name with slash "Org/Platform" + channel "General" -> routes to chan:t-org:ch-g
    let out = run_cli(&tmp1, &["send", "Org/Platform/General", "hello org", "--json"]).await;
    assert!(
        out.status.success(),
        "send Org/Platform/General failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(state.posted_channels.read().unwrap().len(), 1);
    assert_eq!(state.posted_channels.read().unwrap()[0].0, "t-org");
    assert_eq!(state.posted_channels.read().unwrap()[0].1, "ch-g");

    // Control: explicit conv_key still sends directly to the specified target
    let out = run_cli(&tmp1, &["send", "chan:t-ui:ch-ux", "explicit chan", "--json"]).await;
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(state.posted_channels.read().unwrap().len(), 2);
    assert_eq!(state.posted_channels.read().unwrap()[1].0, "t-ui");
    assert_eq!(state.posted_channels.read().unwrap()[1].1, "ch-ux");

    // Case 3: Two exact matches (chat "UI/UX" + channel "UI"/"UX") -> fails with candidate keys and 0 POSTs
    *state.include_exact_ui_ux_channel.write().unwrap() = true;
    state.posted_chats.write().unwrap().clear();
    state.posted_channels.write().unwrap().clear();
    let tmp2 = tempfile::tempdir().unwrap();
    assert!(run_cli(&tmp2, &["chats", "--refresh", "--json"]).await.status.success());
    assert!(run_cli(&tmp2, &["teams", "--refresh", "--json"]).await.status.success());
    let out = run_cli(&tmp2, &["send", "UI/UX", "ambiguous", "--json"]).await;
    assert!(
        !out.status.success(),
        "expected ambiguity failure for two exact matches, got stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("chat:19:uiux@thread.v2") && stderr.contains("chan:t-exact:ch-exact-ux"),
        "expected candidate conv_keys in stderr, got: {stderr}"
    );
    assert_eq!(state.posted_chats.read().unwrap().len(), 0);
    assert_eq!(state.posted_channels.read().unwrap().len(), 0);

    // Case 4: joinedTeams returns 403 -> channel lookup error and sync summary both expose 403 while chat sync still succeeds
    *state.joined_teams_403.write().unwrap() = true;
    let tmp3 = tempfile::tempdir().unwrap();
    let read_out = run_cli(&tmp3, &["read", "UI Team/UX Review", "--json"]).await;
    assert!(!read_out.status.success());
    let read_err = String::from_utf8_lossy(&read_out.stderr);
    assert!(
        read_err.contains("403"),
        "expected 403 in channel lookup error, got: {read_err}"
    );

    let sync_out = run_cli(&tmp3, &["sync", "--json"]).await;
    assert!(
        sync_out.status.success(),
        "sync should still finish partial chat sync: stderr={}",
        String::from_utf8_lossy(&sync_out.stderr)
    );
    let sync_json: Value = serde_json::from_slice(&sync_out.stdout).expect("parse sync json");
    assert_eq!(sync_json["ok"], false, "sync ok must be false on joinedTeams 403: {sync_json}");
    let sync_errors = sync_json["errors"].as_array().unwrap();
    assert!(
        sync_errors
            .iter()
            .any(|e| e.as_str().unwrap_or("").contains("403")),
        "expected 403 in sync errors, got: {sync_json}"
    );
    assert!(
        sync_json["conversations_synced"].as_u64().unwrap_or(0) >= 1
            && sync_json["messages_synced"].as_u64().unwrap_or(0) >= 1,
        "expected successful chat sync results alongside teams error: {sync_json}"
    );

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_15_team_channels_pagination() {
    #[derive(Default)]
    struct Bug15State {
        fail_big_page2: RwLock<bool>,
        channel_get_requests: RwLock<Vec<String>>,
        base_url: RwLock<String>,
    }

    let state = Arc::new(Bug15State::default());

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
                    "value": [
                        { "id": "t-many", "displayName": "BigTeam" },
                        { "id": "t-small", "displayName": "SmallTeam" },
                        { "id": "t-other", "displayName": "OtherTeam" }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/$batch",
            post(|State(st): State<Arc<Bug15State>>, Json(payload): Json<Value>| async move {
                let base = st.base_url.read().unwrap().clone();
                let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                let responses: Vec<Value> = reqs
                    .into_iter()
                    .map(|req| {
                        let req_id = req["id"].as_str().unwrap_or("0");
                        let url = req["url"].as_str().unwrap_or("")
                            // Channel listings are requested from `/allChannels`; answer them like `/channels`.
                            .replace("/allChannels", "/channels");
                        if url.contains("/teams/t-many/channels") {
                            json!({
                                "id": req_id,
                                "status": 200,
                                "body": {
                                    "value": [
                                        { "id": "ch-01", "displayName": "Channel-01", "membershipType": "standard" },
                                        { "id": "ch-02", "displayName": "Channel-02", "membershipType": "standard" }
                                    ],
                                    "@odata.nextLink": format!("{base}/teams/t-many/channels?page=2")
                                }
                            })
                        } else if url.contains("/teams/t-small/channels") {
                            json!({
                                "id": req_id,
                                "status": 200,
                                "body": {
                                    "value": [
                                        { "id": "sm-01", "displayName": "Small-01", "membershipType": "standard" },
                                        { "id": "sm-02", "displayName": "Small-02", "membershipType": "standard" }
                                    ]
                                }
                            })
                        } else if url.contains("/teams/t-other/channels") {
                            json!({
                                "id": req_id,
                                "status": 200,
                                "body": {
                                    "value": [
                                        { "id": "ot-01", "displayName": "Other-01", "membershipType": "standard" },
                                        { "id": "ot-02", "displayName": "Other-02", "membershipType": "standard" }
                                    ],
                                    "@odata.nextLink": format!("{base}/teams/t-other/channels?page=2")
                                }
                            })
                        } else {
                            json!({ "id": req_id, "status": 404, "body": {} })
                        }
                    })
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/teams/{team_id}/channels",
            get(
                |State(st): State<Arc<Bug15State>>,
                 Path(team_id): Path<String>,
                 RawQuery(query): RawQuery| async move {
                    let q = query.unwrap_or_default();
                    st.channel_get_requests
                        .write()
                        .unwrap()
                        .push(format!("/teams/{team_id}/channels?{q}"));
                    let base = st.base_url.read().unwrap().clone();
                    match (team_id.as_str(), q.as_str()) {
                        ("t-many", "page=2") => {
                            if *st.fail_big_page2.read().unwrap() {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(json!({ "error": { "code": "InternalServerError", "message": "page 2 failure" } })),
                                )
                                    .into_response();
                            }
                            Json(json!({
                                "value": [
                                    { "id": "ch-03", "displayName": "Channel-03", "membershipType": "standard" },
                                    { "id": "ch-04", "displayName": "Channel-04", "membershipType": "standard" }
                                ],
                                "@odata.nextLink": format!("{base}/teams/t-many/channels?page=3")
                            }))
                            .into_response()
                        }
                        ("t-many", "page=3") => Json(json!({
                            "value": [
                                { "id": "ch-05", "displayName": "Channel-05", "membershipType": "standard" }
                            ]
                        }))
                        .into_response(),
                        ("t-other", "page=2") => Json(json!({
                            "value": [
                                { "id": "ot-03", "displayName": "Other-03", "membershipType": "standard" }
                            ]
                        }))
                        .into_response(),
                        _ => Json(json!({ "value": [] })).into_response(),
                    }
                },
            ),
        )
        .route(
            "/v1.0/teams/t-many/channels/ch-05/messages",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "m-ch05",
                        "messageType": "message",
                        "createdDateTime": "2026-11-21T09:00:01Z",
                        "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                        "body": { "contentType": "html", "content": "<p>from channel 5</p>" },
                        "mentions": [],
                        "attachments": [],
                        "reactions": [],
                        "replies": []
                    }]
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

    // 1. Mid-page failure does NOT record truncated channel list as complete
    *state.fail_big_page2.write().unwrap() = true;
    let tmp_fail = tempfile::tempdir().unwrap();
    let fail_out = run_cli(&tmp_fail, &["teams", "--refresh", "--json"]).await;
    assert!(!fail_out.status.success(), "mid-page failure must fail teams --refresh");
    let fail_db = tmp_fail.path().join("data").join("me-user-1").join("cache.db");
    if fail_db.exists() {
        let st = Storage::open(&fail_db).unwrap();
        assert!(
            st.list_teams().unwrap().is_empty(),
            "failed paginated refresh must not persist incomplete team channel list"
        );
        assert!(st.get_sync_cursor("cli:teams").unwrap().is_none());
    }

    // 2. Full pagination (2/2/1 for BigTeam, 2 for SmallTeam with no nextLink, 2/1 for OtherTeam)
    *state.fail_big_page2.write().unwrap() = false;
    state.channel_get_requests.write().unwrap().clear();
    let tmp = tempfile::tempdir().unwrap();
    let t_out = run_cli(&tmp, &["teams", "--refresh", "--json"]).await;
    assert!(t_out.status.success(), "stderr={}", String::from_utf8_lossy(&t_out.stderr));
    let tj: Value = serde_json::from_slice(&t_out.stdout).unwrap();
    let teams_arr = tj["teams"].as_array().unwrap();
    let big = teams_arr.iter().find(|t| t["id"] == "t-many").unwrap();
    let small = teams_arr.iter().find(|t| t["id"] == "t-small").unwrap();
    let other = teams_arr.iter().find(|t| t["id"] == "t-other").unwrap();
    let big_names: Vec<&str> = big["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(
        big_names,
        vec!["Channel-01", "Channel-02", "Channel-03", "Channel-04", "Channel-05"]
    );
    let small_names: Vec<&str> = small["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(small_names, vec!["Small-01", "Small-02"]);
    let other_names: Vec<&str> = other["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(other_names, vec!["Other-01", "Other-02", "Other-03"]);

    // Regression control: SmallTeam (no nextLink) has 0 follow-up channel GETs
    let followups = state.channel_get_requests.read().unwrap().clone();
    assert!(
        !followups.iter().any(|u| u.contains("/teams/t-small/")),
        "team without nextLink must have 0 follow-up requests: {followups:?}"
    );

    // channels --refresh also returns all 5 channels for BigTeam
    let c_out = run_cli(&tmp, &["channels", "BigTeam", "--refresh", "--json"]).await;
    assert!(c_out.status.success(), "stderr={}", String::from_utf8_lossy(&c_out.stderr));
    let cj: Value = serde_json::from_slice(&c_out.stdout).unwrap();
    assert_eq!(cj["channels"].as_array().unwrap().len(), 5);

    // Target resolution for Channel-05 succeeds
    let r_out = run_cli(&tmp, &["read", "BigTeam/Channel-05", "--json"]).await;
    assert!(r_out.status.success(), "stderr={}", String::from_utf8_lossy(&r_out.stderr));
    let rj: Value = serde_json::from_slice(&r_out.stdout).unwrap();
    assert_eq!(rj["messages"][0]["msg_id"], "m-ch05");

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_17_batch_channel_failure_keeps_cache() {
    #[derive(Default)]
    struct Bug17State {
        batch_fail_status: RwLock<Option<u16>>,
        direct_fail_status: RwLock<Option<u16>>,
        direct_get_count: RwLock<usize>,
    }

    let state = Arc::new(Bug17State::default());

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
                    "value": [
                        { "id": "team-a", "displayName": "Engineering", "description": "eng" },
                        { "id": "team-b", "displayName": "Restricted", "description": "403 team" },
                        { "id": "team-c", "displayName": "EmptyTeam", "description": "really empty" }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/$batch",
            post(|State(st): State<Arc<Bug17State>>, Json(payload): Json<Value>| async move {
                let fail_code = *st.batch_fail_status.read().unwrap();
                let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                let responses: Vec<Value> = reqs
                    .into_iter()
                    .map(|req| {
                        let req_id = req["id"].as_str().unwrap_or("0");
                        let url = req["url"].as_str().unwrap_or("")
                            // Channel listings are requested from `/allChannels`; answer them like `/channels`.
                            .replace("/allChannels", "/channels");
                        if url.contains("/teams/team-a/channels") {
                            json!({
                                "id": req_id,
                                "status": 200,
                                "body": { "value": [{ "id": "chan-general", "displayName": "General", "membershipType": "standard" }] }
                            })
                        } else if url.contains("/teams/team-b/channels") {
                            if let Some(code) = fail_code {
                                json!({
                                    "id": req_id,
                                    "status": code,
                                    "body": { "error": { "code": if code == 429 { "TooManyRequests" } else { "Forbidden" }, "message": "denied" } }
                                })
                            } else {
                                json!({
                                    "id": req_id,
                                    "status": 200,
                                    "body": { "value": [{ "id": "chan-secret", "displayName": "Secret", "membershipType": "private" }] }
                                })
                            }
                        } else {
                            json!({
                                "id": req_id,
                                "status": 200,
                                "body": { "value": [] }
                            })
                        }
                    })
                    .collect();
                Json(json!({ "responses": responses }))
            }),
        )
        .route(
            "/v1.0/teams/{team_id}/channels",
            get(|State(st): State<Arc<Bug17State>>, Path(team_id): Path<String>| async move {
                if team_id == "team-b" {
                    *st.direct_get_count.write().unwrap() += 1;
                    if let Some(code) = *st.direct_fail_status.read().unwrap() {
                        let status =
                            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                        let err_code = if code == 429 { "TooManyRequests" } else { "Forbidden" };
                        let mut resp = (
                            status,
                            Json(json!({ "error": { "code": err_code, "message": "denied" } })),
                        )
                            .into_response();
                        if code == 429 {
                            resp.headers_mut().insert("retry-after", "1".parse().unwrap());
                        }
                        return resp;
                    }
                    return Json(json!({
                        "value": [{ "id": "chan-secret", "displayName": "Secret", "membershipType": "private" }]
                    }))
                    .into_response();
                }
                Json(json!({ "value": [] })).into_response()
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

    // 1. Healthy teams --refresh --json
    let out1 = run_cli(&tmp, &["teams", "--refresh", "--json"]).await;
    assert!(out1.status.success(), "stderr={}", String::from_utf8_lossy(&out1.stderr));
    let j1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(j1["channel_errors"], json!([]));
    let tb1 = j1["teams"].as_array().unwrap().iter().find(|t| t["id"] == "team-b").unwrap();
    assert_eq!(tb1["channels"][0]["id"], "chan-secret");

    // 2. Inject 429 on both $batch and direct GET
    *state.batch_fail_status.write().unwrap() = Some(429);
    *state.direct_fail_status.write().unwrap() = Some(429);
    let out2 = run_cli(&tmp, &["teams", "--refresh", "--json"]).await;
    assert!(out2.status.success(), "stderr={}", String::from_utf8_lossy(&out2.stderr));
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(stderr2.contains("Warning"), "expected warning on stderr: {stderr2}");
    let j2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    let tb2 = j2["teams"].as_array().unwrap().iter().find(|t| t["id"] == "team-b").unwrap();
    assert_eq!(tb2["channels"][0]["id"], "chan-secret");
    assert_eq!(j2["channel_errors"][0]["team_id"], "team-b");
    assert_eq!(j2["channel_errors"][0]["status"], 429);
    assert!(j2["channel_errors"][0]["error"].as_str().unwrap().contains("429"));

    // 3. Cached read preserves chan-secret and omits channel_errors key
    let out3 = run_cli(&tmp, &["teams", "--cached", "--json"]).await;
    assert!(out3.status.success());
    let j3: Value = serde_json::from_slice(&out3.stdout).unwrap();
    let tb3 = j3["teams"].as_array().unwrap().iter().find(|t| t["id"] == "team-b").unwrap();
    assert_eq!(tb3["channels"][0]["id"], "chan-secret");
    assert!(j3.get("channel_errors").is_none(), "cached response must omit channel_errors: {j3}");

    // 4. channels Restricted --json fails with exit 1 and structured JSON error on stdout
    let out4 = run_cli(&tmp, &["channels", "Restricted", "--json"]).await;
    assert_eq!(out4.status.code(), Some(1));
    let j4: Value = serde_json::from_slice(&out4.stdout)
        .expect("failed --json command must emit structured error JSON on stdout");
    assert_eq!(j4["ok"], false);
    assert!(
        j4["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains(r#"Could not list channels for team "Restricted" (team-b)"#),
        "{j4}"
    );
    let stderr4 = String::from_utf8_lossy(&out4.stderr);
    assert!(
        stderr4.contains(r#"Could not list channels for team "Restricted" (team-b)"#),
        "{stderr4}"
    );

    // 5. channels EmptyTeam --json succeeds with total 0
    let out5 = run_cli(&tmp, &["channels", "EmptyTeam", "--json"]).await;
    assert!(out5.status.success(), "stderr={}", String::from_utf8_lossy(&out5.stderr));
    let j5: Value = serde_json::from_slice(&out5.stdout).unwrap();
    assert_eq!(j5["channels"], json!([]));
    assert_eq!(j5["total"], 0);
    assert_eq!(j5["returned"], 0);
    assert_eq!(j5["has_more"], false);
    assert!(j5["next_cursor"].is_null());

    // 6. Fresh profile with 403 -> Restricted has channels: [] and is in channel_errors; EmptyTeam is not
    *state.batch_fail_status.write().unwrap() = Some(403);
    *state.direct_fail_status.write().unwrap() = Some(403);
    let tmp_fresh = tempfile::tempdir().unwrap();
    let out6 = run_cli(&tmp_fresh, &["teams", "--refresh", "--json"]).await;
    assert!(out6.status.success());
    let j6: Value = serde_json::from_slice(&out6.stdout).unwrap();
    let tb6 = j6["teams"].as_array().unwrap().iter().find(|t| t["id"] == "team-b").unwrap();
    assert_eq!(tb6["channels"], json!([]));
    let errs6 = j6["channel_errors"].as_array().unwrap();
    assert_eq!(errs6.len(), 1);
    assert_eq!(errs6[0]["team_id"], "team-b");
    assert_eq!(errs6[0]["status"], 403);
    assert!(!errs6.iter().any(|e| e["team_id"] == "team-c"));

    // 7. $batch returns 429, direct GET succeeds -> channel_errors empty, chan-secret returned, direct_get_count >= 1
    *state.batch_fail_status.write().unwrap() = Some(429);
    *state.direct_fail_status.write().unwrap() = None;
    *state.direct_get_count.write().unwrap() = 0;
    let out7 = run_cli(&tmp_fresh, &["teams", "--refresh", "--json"]).await;
    assert!(out7.status.success(), "stderr={}", String::from_utf8_lossy(&out7.stderr));
    let j7: Value = serde_json::from_slice(&out7.stdout).unwrap();
    assert_eq!(j7["channel_errors"], json!([]));
    let tb7 = j7["teams"].as_array().unwrap().iter().find(|t| t["id"] == "team-b").unwrap();
    assert_eq!(tb7["channels"][0]["id"], "chan-secret");
    assert!(*state.direct_get_count.read().unwrap() >= 1);

    server.abort();
}

#[tokio::test]
async fn regression_teams_bug_19_members_and_channels_paging() {
    #[derive(Default)]
    struct Bug19State {
        base_url: RwLock<String>,
        member_gets: RwLock<HashMap<String, usize>>,
        batch_calls: RwLock<usize>,
    }

    let state = Arc::new(Bug19State::default());

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
                    "value": [
                        { "id": "19:release-room@thread.v2", "chatType": "group", "topic": "Release Room", "lastUpdatedDateTime": "2026-11-21T05:00:00Z", "members": [] },
                        { "id": "19:chat-1@thread.v2", "chatType": "group", "topic": "Chat 1", "lastUpdatedDateTime": "2026-11-21T04:00:00Z", "members": [] },
                        { "id": "19:empty-room@thread.v2", "chatType": "group", "topic": "Empty Room", "lastUpdatedDateTime": "2026-11-21T03:00:00Z", "members": [] },
                        { "id": "19:chat-2@thread.v2", "chatType": "group", "topic": "Chat 2", "lastUpdatedDateTime": "2026-11-21T02:00:00Z", "members": [] },
                        { "id": "19:broken-room@thread.v2", "chatType": "group", "topic": "Broken Room", "lastUpdatedDateTime": "2026-11-21T01:00:00Z", "members": [] }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/chats/{chat_id}/members",
            get(
                |State(st): State<Arc<Bug19State>>,
                 Path(chat_id): Path<String>,
                 RawQuery(query): RawQuery| async move {
                    *st.member_gets.write().unwrap().entry(chat_id.clone()).or_insert(0) += 1;
                    let base = st.base_url.read().unwrap().clone();
                    let q = query.unwrap_or_default();
                    let page = q
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("page="))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1);

                    match chat_id.as_str() {
                        "19:release-room@thread.v2" => {
                            let start = (page - 1) * 50;
                            let end = (start + 50).min(120);
                            let vals: Vec<Value> = (start..end)
                                .map(|i| {
                                    json!({
                                        "id": format!("mem-{i:04}"),
                                        "userId": format!("u-{i:04}"),
                                        "displayName": format!("Member {i:04}"),
                                        "email": format!("m{i:04}@contoso.com"),
                                        "roles": ["member"]
                                    })
                                })
                                .collect();
                            if end < 120 {
                                Json(json!({
                                    "value": vals,
                                    "@odata.nextLink": format!("{base}/chats/{}/members?page={}", urlencoding::encode(&chat_id), page + 1)
                                }))
                                .into_response()
                            } else {
                                Json(json!({ "value": vals })).into_response()
                            }
                        }
                        "19:chat-1@thread.v2" => Json(json!({
                            "value": [
                                { "id": "m1", "userId": "u-a", "displayName": "Alice", "email": "a@contoso.com", "roles": [] },
                                { "id": "m2", "userId": "u-b", "displayName": "Bob", "email": "b@contoso.com", "roles": [] },
                                { "id": "m3", "userId": "u-c", "displayName": "Carol", "email": "c@contoso.com", "roles": [] }
                            ]
                        }))
                        .into_response(),
                        "19:empty-room@thread.v2" => Json(json!({ "value": [] })).into_response(),
                        "19:broken-room@thread.v2" => {
                            if page == 1 {
                                Json(json!({
                                    "value": [{ "id": "mb1", "userId": "u-b1", "displayName": "Partial User", "roles": [] }],
                                    "@odata.nextLink": format!("{base}/chats/{}/members?page=2", urlencoding::encode(&chat_id))
                                }))
                                .into_response()
                            } else {
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(json!({ "error": { "code": "InternalServerError", "message": "boom" } })),
                                )
                                    .into_response()
                            }
                        }
                        _ => Json(json!({ "value": [] })).into_response(),
                    }
                },
            ),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|| async {
                Json(json!({
                    "value": [
                        { "id": "team-big", "displayName": "Big" },
                        { "id": "team-zero", "displayName": "Zero" }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/$batch",
            post(|State(st): State<Arc<Bug19State>>, Json(payload): Json<Value>| async move {
                *st.batch_calls.write().unwrap() += 1;
                let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                let responses: Vec<Value> = reqs
                    .into_iter()
                    .map(|req| {
                        let req_id = req["id"].as_str().unwrap_or("0");
                        let url = req["url"].as_str().unwrap_or("")
                            // Channel listings are requested from `/allChannels`; answer them like `/channels`.
                            .replace("/allChannels", "/channels");
                        if url.contains("/teams/team-big/channels") {
                            let chs: Vec<Value> = (0..55)
                                .map(|i| {
                                    json!({
                                        "id": format!("ch-{i:03}"),
                                        "displayName": format!("Channel {i:03}"),
                                        "membershipType": "standard"
                                    })
                                })
                                .collect();
                            json!({ "id": req_id, "status": 200, "body": { "value": chs } })
                        } else {
                            json!({ "id": req_id, "status": 200, "body": { "value": [] } })
                        }
                    })
                    .collect();
                Json(json!({ "responses": responses }))
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
    assert!(run_cli(&tmp, &["chats", "--json"]).await.status.success());

    // 1. Release Room 3 pages via cursor with 0 Graph calls on pages 2 & 3
    let p1_out = run_cli(&tmp, &["members", "Release Room", "--json"]).await;
    assert!(p1_out.status.success(), "stderr={}", String::from_utf8_lossy(&p1_out.stderr));
    let p1: Value = serde_json::from_slice(&p1_out.stdout).unwrap();
    assert_eq!(p1["returned"], 50);
    assert_eq!(p1["has_more"], true);
    assert_eq!(p1["total"], 120);
    let c1 = p1["next_cursor"].as_str().unwrap().to_string();
    assert!(c1.starts_with("50-") && c1[3..].chars().all(|c| c.is_ascii_digit()), "{c1}");
    let fetched_at = p1["fetched_at"].as_str().unwrap().to_string();
    assert_eq!(p1["members"][0]["display_name"], "Member 0000");
    assert_eq!(
        state.member_gets.read().unwrap().get("19:release-room@thread.v2").copied(),
        Some(3)
    );

    let p2_out = run_cli(&tmp, &["members", "Release Room", "--json", "--cursor", &c1]).await;
    assert!(p2_out.status.success(), "stderr={}", String::from_utf8_lossy(&p2_out.stderr));
    let p2: Value = serde_json::from_slice(&p2_out.stdout).unwrap();
    assert_eq!(p2["returned"], 50);
    assert_eq!(p2["has_more"], true);
    assert_eq!(p2["total"], 120);
    assert_eq!(p2["fetched_at"], fetched_at);
    assert_eq!(p2["members"][0]["display_name"], "Member 0050");
    let c2 = p2["next_cursor"].as_str().unwrap().to_string();
    assert!(c2.starts_with("100-"));
    assert_eq!(
        state.member_gets.read().unwrap().get("19:release-room@thread.v2").copied(),
        Some(3)
    );

    let p3_out = run_cli(&tmp, &["members", "Release Room", "--json", "--cursor", &c2]).await;
    assert!(p3_out.status.success(), "stderr={}", String::from_utf8_lossy(&p3_out.stderr));
    let p3: Value = serde_json::from_slice(&p3_out.stdout).unwrap();
    assert_eq!(p3["returned"], 20);
    assert_eq!(p3["has_more"], false);
    assert!(p3["next_cursor"].is_null());
    assert_eq!(p3["total"], 120);
    assert_eq!(p3["fetched_at"], fetched_at);
    assert_eq!(p3["members"][0]["display_name"], "Member 0100");
    assert_eq!(
        state.member_gets.read().unwrap().get("19:release-room@thread.v2").copied(),
        Some(3)
    );

    let mut all_ids = std::collections::HashSet::new();
    let mut all_names = Vec::new();
    for page in [&p1, &p2, &p3] {
        for m in page["members"].as_array().unwrap() {
            all_ids.insert(m["user_id"].as_str().unwrap().to_string());
            all_names.push(m["display_name"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(all_ids.len(), 120);
    let mut sorted_names = all_names.clone();
    sorted_names.sort();
    assert_eq!(all_names, sorted_names);

    // 2. Re-fetch advances stamp -> old cursor is rejected as Stale cursor
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let refetch_out = run_cli(&tmp, &["members", "Release Room", "--json"]).await;
    assert!(refetch_out.status.success());
    let refetch: Value = serde_json::from_slice(&refetch_out.stdout).unwrap();
    let current_cursor = refetch["next_cursor"].as_str().unwrap().to_string();
    assert_eq!(
        state.member_gets.read().unwrap().get("19:release-room@thread.v2").copied(),
        Some(6)
    );
    let stale_out = run_cli(&tmp, &["members", "Release Room", "--json", "--cursor", &c1]).await;
    assert_eq!(stale_out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&stale_out.stderr).contains("Stale cursor"));

    // 3. Cursor from Release Room rejected on Chat 1
    let chat1_out = run_cli(&tmp, &["members", "Chat 1", "--json"]).await;
    assert!(chat1_out.status.success());
    let chat1: Value = serde_json::from_slice(&chat1_out.stdout).unwrap();
    assert_eq!(chat1["returned"], 3);
    assert_eq!(chat1["total"], 3);
    assert_eq!(chat1["has_more"], false);
    let cross_cursor = format!("1-{}", current_cursor.split_once('-').unwrap().1);
    let cross_out = run_cli(&tmp, &["members", "Chat 1", "--json", "--cursor", &cross_cursor]).await;
    assert_eq!(cross_out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cross_out.stderr).contains("Stale cursor"));

    // 4. Empty Room online -> offline succeeds with total 0 and 0 Graph calls offline
    let empty_on = run_cli(&tmp, &["members", "Empty Room", "--json"]).await;
    assert!(empty_on.status.success());
    let j_empty_on: Value = serde_json::from_slice(&empty_on.stdout).unwrap();
    assert_eq!(j_empty_on["members"], json!([]));
    assert_eq!(j_empty_on["total"], 0);
    assert_eq!(j_empty_on["has_more"], false);
    assert!(j_empty_on["fetched_at"].is_string());
    let empty_count_before = state
        .member_gets
        .read()
        .unwrap()
        .get("19:empty-room@thread.v2")
        .copied()
        .unwrap_or(0);
    let empty_off = run_cli(&tmp, &["members", "Empty Room", "--json", "--offline"]).await;
    assert!(empty_off.status.success());
    let j_empty_off: Value = serde_json::from_slice(&empty_off.stdout).unwrap();
    assert_eq!(j_empty_off["members"], json!([]));
    assert_eq!(j_empty_off["total"], 0);
    assert!(j_empty_off["fetched_at"].is_string());
    assert_eq!(
        state
            .member_gets
            .read()
            .unwrap()
            .get("19:empty-room@thread.v2")
            .copied()
            .unwrap_or(0),
        empty_count_before
    );

    // 5. Chat 2 offline without prior fetch -> exit 1
    let chat2_off = run_cli(&tmp, &["members", "Chat 2", "--json", "--offline"]).await;
    assert_eq!(chat2_off.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&chat2_off.stderr).contains("have not been fetched yet"));

    // 6. Broken Room fails on page 2 -> offline still fails with "have not been fetched yet"
    let broken_on = run_cli(&tmp, &["members", "Broken Room", "--json"]).await;
    assert_eq!(broken_on.status.code(), Some(1));
    let broken_off = run_cli(&tmp, &["members", "Broken Room", "--json", "--offline"]).await;
    assert_eq!(broken_off.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&broken_off.stderr).contains("have not been fetched yet"));

    // 7. Text members: no flags -> 120 lines; --limit 50 -> 50 lines + footer
    let txt_all = run_cli(&tmp, &["members", "Release Room", "--offline"]).await;
    assert!(txt_all.status.success());
    let txt_all_str = String::from_utf8_lossy(&txt_all.stdout);
    let member_lines_all = txt_all_str.lines().filter(|l| l.starts_with("  Member ")).count();
    assert_eq!(member_lines_all, 120);

    let txt_50 = run_cli(&tmp, &["members", "Release Room", "--offline", "--limit", "50"]).await;
    assert!(txt_50.status.success());
    let txt_50_str = String::from_utf8_lossy(&txt_50.stdout);
    let member_lines_50 = txt_50_str.lines().filter(|l| l.starts_with("  Member ")).count();
    assert_eq!(member_lines_50, 50);
    assert!(txt_50_str.contains("Next page: --cursor 50-"), "{txt_50_str}");

    // 8. channels Big paging (0 $batch on page 2) + teams paging
    let ch1_out = run_cli(&tmp, &["channels", "Big", "--json"]).await;
    assert!(ch1_out.status.success(), "stderr={}", String::from_utf8_lossy(&ch1_out.stderr));
    let ch1: Value = serde_json::from_slice(&ch1_out.stdout).unwrap();
    assert_eq!(ch1["returned"], 50);
    assert_eq!(ch1["has_more"], true);
    assert_eq!(ch1["total"], 55);
    let ch_cursor = ch1["next_cursor"].as_str().unwrap().to_string();
    let batch_before = *state.batch_calls.read().unwrap();
    let ch2_out = run_cli(&tmp, &["channels", "Big", "--json", "--cursor", &ch_cursor]).await;
    assert!(ch2_out.status.success(), "stderr={}", String::from_utf8_lossy(&ch2_out.stderr));
    let ch2: Value = serde_json::from_slice(&ch2_out.stdout).unwrap();
    assert_eq!(ch2["returned"], 5);
    assert_eq!(ch2["has_more"], false);
    assert!(ch2["next_cursor"].is_null());
    assert_eq!(*state.batch_calls.read().unwrap(), batch_before);

    let teams_out = run_cli(&tmp, &["teams", "--json"]).await;
    assert!(teams_out.status.success());
    let tj: Value = serde_json::from_slice(&teams_out.stdout).unwrap();
    assert_eq!(tj["returned"], tj["total"]);

    // 9. Fresh profile via MCP: list_members paging, Empty Room, Chat 2 offline
    state.member_gets.write().unwrap().clear();
    let dl_tmp = tempfile::tempdir().unwrap();
    let (cfg, graph) = build_direct_graph_client(&auth_base, dl_tmp.path(), &jwt);
    let storage = Storage::open_in_memory().unwrap();
    let list_chats = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_chats","arguments":{"refresh":true}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(list_chats["result"]["isError"], false);

    let m1_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_members","arguments":{"target":"Release Room","limit":50}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(m1_resp["result"]["isError"], false, "{m1_resp}");
    let mp1: Value =
        serde_json::from_str(m1_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mp1["returned"], 50);
    assert_eq!(mp1["total"], 120);
    let mc1 = mp1["next_cursor"].as_str().unwrap().to_string();
    assert_eq!(
        state.member_gets.read().unwrap().get("19:release-room@thread.v2").copied(),
        Some(3)
    );

    let m2_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_members","arguments":{"target":"Release Room","limit":50,"cursor":mc1}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(m2_resp["result"]["isError"], false, "{m2_resp}");
    let mp2: Value =
        serde_json::from_str(m2_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mp2["returned"], 50);
    assert_eq!(mp2["members"][0]["display_name"], "Member 0050");
    assert_eq!(
        state.member_gets.read().unwrap().get("19:release-room@thread.v2").copied(),
        Some(3)
    );

    let m_empty = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_members","arguments":{"target":"Empty Room"}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(m_empty["result"]["isError"], false);
    let m_empty_off = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_members","arguments":{"target":"Empty Room","offline":true}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(m_empty_off["result"]["isError"], false);
    let me_payload: Value =
        serde_json::from_str(m_empty_off["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(me_payload["total"], 0);
    assert_eq!(me_payload["members"], json!([]));

    let m_unfetched = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        &json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_members","arguments":{"target":"Chat 2","offline":true}}}).to_string(),
    )
    .await
    .unwrap();
    assert_eq!(m_unfetched["result"]["isError"], true);
    assert!(m_unfetched["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("have not been fetched yet"));

    server.abort();
}
