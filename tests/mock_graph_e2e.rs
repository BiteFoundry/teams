//! End-to-end integration tests against an in-process mock Microsoft Graph v1.0 + Entra OAuth server.

use std::{
    collections::HashMap,
    io::Cursor,
    sync::{Arc, Once, RwLock},
};

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use image::{ImageBuffer, Rgba, RgbaImage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;

use teams_cli::{
    auth::{
        diagnose_access_token_scopes, explain_graph_permission_error, AuthManager, StoredTokens,
    },
    config::{AppConfig, ImageProtocolPreference},
    graph::GraphClient,
    img_preview::{self, TerminalGraphicsCaps, TerminalImageProtocol},
    mcp,
    models::{AttachmentInfo, ConversationTarget, MentionInfo, MessageItem, TeamSummary},
    parser, resolver, serve,
    storage::Storage,
    updater,
};

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();

#[cfg(windows)]
#[test]
fn windows_cli_starts_and_reports_its_version() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .arg("--version")
        .output()
        .expect("start Windows CLI");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("teams {}", env!("CARGO_PKG_VERSION"))
    );
}

fn ensure_isolated_env() {
    INIT_TEST_ENV.call_once(|| {
        let dir = tempfile::tempdir().expect("create shared test config tempdir");
        // Safety: initialized once before any AuthManager writes fallback credentials.
        std::env::set_var("TEAMS_DISABLE_KEYRING", "1");
        std::env::set_var("TEAMS_CONFIG_DIR", dir.path());
        std::env::remove_var("TEAMS_ACCESS_TOKEN");
        let _ = TEST_CONFIG_DIR.set(dir);
    });
}

#[derive(Debug, Clone)]
struct StoredChatMsg {
    id: String,
    reply_to_id: Option<String>,
    subject: Option<String>,
    from_id: String,
    from_name: String,
    created_at: String,
    body_html: String,
    mentions: Vec<Value>,
    attachments: Vec<Value>,
}

impl StoredChatMsg {
    fn to_graph_json(&self, replies: Option<Vec<Value>>) -> Value {
        let mut obj = json!({
            "id": self.id,
            "replyToId": self.reply_to_id,
            "messageType": "message",
            "createdDateTime": self.created_at,
            "subject": self.subject,
            "importance": "normal",
            "from": {
                "user": {
                    "id": self.from_id,
                    "displayName": self.from_name
                }
            },
            "body": {
                "contentType": "html",
                "content": self.body_html
            },
            "mentions": self.mentions,
            "attachments": self.attachments,
            "reactions": []
        });
        if let Some(r) = replies {
            obj["replies"] = Value::Array(r);
        }
        obj
    }
}

#[derive(Default)]
struct MockState {
    expected_token: RwLock<String>,
    refresh_calls: RwLock<usize>,
    me_auth_headers: RwLock<Vec<String>>,
    next_msg_seq: RwLock<u64>,
    chat_messages: RwLock<HashMap<String, Vec<StoredChatMsg>>>,
    group_topic: RwLock<Option<String>>,
    channel_messages: RwLock<HashMap<(String, String), Vec<StoredChatMsg>>>,
    channel_replies: RwLock<HashMap<(String, String, String), Vec<StoredChatMsg>>>,
    uploaded_files: RwLock<HashMap<String, Vec<u8>>>,
    share_contents: RwLock<HashMap<String, Vec<u8>>>,
    posted_chat_payloads: RwLock<Vec<Value>>,
    posted_reply_payloads: RwLock<Vec<Value>>,
    /// Raw query strings of every small-file `PUT …:/content` upload, in arrival order.
    upload_queries: RwLock<Vec<String>>,
    /// Per-test switch: emulate Graph `conflictBehavior=rename` (a second PUT of a name
    /// creates a new driveItem). Off by default so existing tests see identical responses.
    emulate_upload_rename: RwLock<bool>,
    upload_name_counts: RwLock<HashMap<String, usize>>,
}

fn check_bearer_header(headers: &HeaderMap, expected: &str) -> Option<Response> {
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if actual == Some(expected) {
        None
    } else {
        Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "code": "InvalidAuthenticationToken",
                        "message": "Access token has expired or is not valid."
                    }
                })),
            )
                .into_response(),
        )
    }
}

impl MockState {
    fn new(initial_token: &str) -> Self {
        let state = Self::default();
        *state.expected_token.write().unwrap() = initial_token.to_string();
        *state.next_msg_seq.write().unwrap() = 100;
        state
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut seq = self.next_msg_seq.write().unwrap();
        *seq += 1;
        format!("{prefix}-{}", *seq)
    }

    fn check_bearer(&self, headers: &HeaderMap) -> Option<Response> {
        let expected = self.expected_token.read().unwrap().clone();
        check_bearer_header(headers, &expected)
    }

    /// Records the upload query and, when rename emulation is on and the request asked for
    /// it, returns `(item ordinal, stored name)` the way Graph renames a conflicting upload.
    fn record_small_upload(&self, scope: &str, file_name: &str, query: Option<String>) -> (usize, String) {
        let query = query.unwrap_or_default();
        let wants_rename = query.contains("@microsoft.graph.conflictBehavior=rename");
        self.upload_queries.write().unwrap().push(query);
        if !*self.emulate_upload_rename.read().unwrap() || !wants_rename {
            return (1, file_name.to_string());
        }
        let mut counts = self.upload_name_counts.write().unwrap();
        let seen = counts.entry(format!("{scope}/{file_name}")).or_insert(0);
        *seen += 1;
        if *seen == 1 {
            return (1, file_name.to_string());
        }
        let renamed = match file_name.rsplit_once('.') {
            Some((stem, ext)) => format!("{stem} {}.{ext}", *seen - 1),
            None => format!("{file_name} {}", *seen - 1),
        };
        (*seen, renamed)
    }
}

fn mock_teams_payload() -> Vec<Value> {
    vec![json!({
        "id": "team-dev",
        "displayName": "Engineering",
        "description": "Engineering Team"
    })]
}

fn mock_channels_payload() -> Vec<Value> {
    vec![
        json!({
            "id": "chan-general",
            "displayName": "General",
            "description": "General channel",
            "membershipType": "standard"
        }),
        json!({
            "id": "chan-deploy",
            "displayName": "Deployments",
            "description": "Release announcements",
            "membershipType": "standard"
        }),
    ]
}

fn mock_chats_payload() -> Vec<Value> {
    vec![
        json!({
            "id": "19:oneonone-peer@unq.gbl.spaces",
            "chatType": "oneOnOne",
            "lastUpdatedDateTime": "2026-11-15T05:00:00Z",
            "members": [
                { "userId": "me-user-1", "displayName": "Me (Tester)" },
                { "userId": "peer-kim", "displayName": "Alice" }
            ],
            "viewpoint": {
                "lastMessageReadDateTime": "2026-11-15T04:00:00Z"
            },
            "lastMessagePreview": {
                "id": "seed-1",
                "createdDateTime": "2026-11-15T05:00:00Z",
                "from": { "user": { "id": "peer-kim", "displayName": "Alice" } },
                "body": { "contentType": "html", "content": "<p>Hello</p>" }
            }
        }),
        json!({
            "id": "19:group-sprint@thread.v2",
            "chatType": "group",
            "topic": "Backend Sprint",
            "lastUpdatedDateTime": "2026-11-15T05:10:00Z",
            "members": [
                { "userId": "me-user-1", "displayName": "Me (Tester)" },
                { "userId": "peer-kim", "displayName": "Alice" },
                { "userId": "peer-lee", "displayName": "Bob" }
            ]
        }),
    ]
}

async fn oauth_token_handler(
    State(state): State<Arc<MockState>>,
    body: String,
) -> impl IntoResponse {
    assert!(
        body.contains("grant_type=refresh_token"),
        "expected refresh_token grant, got: {body}"
    );
    assert!(
        body.contains("refresh_token=stale-refresh-tok"),
        "expected stored refresh_token in body: {body}"
    );

    *state.refresh_calls.write().unwrap() += 1;
    let new_tok = "fresh-access-tok-2026";
    *state.expected_token.write().unwrap() = new_tok.to_string();

    Json(json!({
        "token_type": "Bearer",
        "expires_in": 3600,
        "access_token": new_tok,
        "refresh_token": "rotated-refresh-tok"
    }))
}

async fn get_me_handler(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    if let Some(auth_val) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        state
            .me_auth_headers
            .write()
            .unwrap()
            .push(auth_val.to_string());
    }
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }

    Json(json!({
        "id": "me-user-1",
        "displayName": "Me (Tester)",
        "mail": "tester@contoso.com",
        "userPrincipalName": "tester@contoso.com"
    }))
    .into_response()
}

async fn list_chats_handler(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let mut chats = mock_chats_payload();
    if let Some(topic) = state.group_topic.read().unwrap().as_ref() {
        chats[1]["topic"] = json!(topic);
    }
    Json(json!({ "value": chats })).into_response()
}

async fn get_chat_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let Some(mut chat) = mock_chats_payload().into_iter().find(|chat| chat["id"] == chat_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if chat["chatType"] == "group" {
        if let Some(topic) = state.group_topic.read().unwrap().as_ref() {
            chat["topic"] = json!(topic);
        }
    }
    Json(chat).into_response()
}

async fn patch_chat_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let Some(mut chat) = mock_chats_payload()
        .into_iter()
        .find(|chat| chat["id"] == chat_id && chat["chatType"] == "group")
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(topic) = payload["topic"].as_str() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if payload != json!({"topic": topic}) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    *state.group_topic.write().unwrap() = Some(topic.to_string());
    chat["topic"] = json!(topic);
    Json(chat).into_response()
}

async fn list_chat_messages_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let map = state.chat_messages.read().unwrap();
    let msgs: Vec<Value> = map
        .get(&chat_id)
        .map(|list| list.iter().map(|m| m.to_graph_json(None)).collect())
        .unwrap_or_default();
    Json(json!({ "value": msgs })).into_response()
}

async fn post_chat_message_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.posted_chat_payloads.write().unwrap().push(payload.clone());

    let id = state.next_id("chat-msg");
    let stored = StoredChatMsg {
        id: id.clone(),
        reply_to_id: None,
        subject: payload
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        from_id: "me-user-1".to_string(),
        from_name: "Me (Tester)".to_string(),
        created_at: "2026-11-15T06:00:00Z".to_string(),
        body_html: payload
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mentions: payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        attachments: payload
            .get("attachments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let json_resp = stored.to_graph_json(None);
    state
        .chat_messages
        .write()
        .unwrap()
        .entry(chat_id)
        .or_default()
        .push(stored);

    (StatusCode::CREATED, Json(json_resp)).into_response()
}

async fn joined_teams_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({ "value": mock_teams_payload() })).into_response()
}

async fn batch_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let reqs = payload
        .get("requests")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut responses = Vec::with_capacity(reqs.len());
    for req in reqs {
        let req_id = req.get("id").and_then(|v| v.as_str()).unwrap_or("0");
        let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("")
            // Channel listings are requested from `/allChannels`; answer them like `/channels`.
            .replace("/allChannels", "/channels");
        if url.contains("/teams/team-dev/channels") {
            responses.push(json!({
                "id": req_id,
                "status": 200,
                "body": { "value": mock_channels_payload() }
            }));
        } else {
            responses.push(json!({
                "id": req_id,
                "status": 404,
                "body": { "error": { "code": "NotFound", "message": "not found" } }
            }));
        }
    }
    Json(json!({ "responses": responses })).into_response()
}

async fn list_channel_messages_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let msgs_guard = state.channel_messages.read().unwrap();
    let replies_guard = state.channel_replies.read().unwrap();
    let list = msgs_guard
        .get(&(team_id.clone(), channel_id.clone()))
        .cloned()
        .unwrap_or_default();
    let values: Vec<Value> = list
        .into_iter()
        .map(|m| {
            let reply_stubs: Vec<Value> = replies_guard
                .get(&(team_id.clone(), channel_id.clone(), m.id.clone()))
                .map(|rs| rs.iter().map(|r| json!({ "id": r.id })).collect())
                .unwrap_or_default();
            m.to_graph_json(Some(reply_stubs))
        })
        .collect();
    Json(json!({ "value": values })).into_response()
}

async fn post_channel_message_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id)): Path<(String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let id = state.next_id("chan-root");
    let stored = StoredChatMsg {
        id: id.clone(),
        reply_to_id: None,
        subject: payload
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        from_id: "me-user-1".to_string(),
        from_name: "Me (Tester)".to_string(),
        created_at: "2026-11-15T06:10:00Z".to_string(),
        body_html: payload
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mentions: payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        attachments: payload
            .get("attachments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let resp_json = stored.to_graph_json(Some(vec![]));
    state
        .channel_messages
        .write()
        .unwrap()
        .entry((team_id, channel_id))
        .or_default()
        .push(stored);

    (StatusCode::CREATED, Json(resp_json)).into_response()
}

async fn get_channel_root_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let msgs_guard = state.channel_messages.read().unwrap();
    let found = msgs_guard
        .get(&(team_id.clone(), channel_id.clone()))
        .and_then(|list| list.iter().find(|m| m.id == root_id))
        .cloned();
    match found {
        Some(m) => Json(m.to_graph_json(None)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NotFound", "message": "missing root" } }))).into_response(),
    }
}

async fn patch_channel_root_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let mut messages = state.channel_messages.write().unwrap();
    let Some(message) = messages
        .get_mut(&(team_id, channel_id))
        .and_then(|list| list.iter_mut().find(|msg| msg.id == root_id))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(subject) = payload["subject"].as_str() {
        if payload != json!({"subject": subject}) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        message.subject = Some(subject.to_string());
    } else if let Some(content) = payload.pointer("/body/content").and_then(Value::as_str) {
        message.body_html = content.to_string();
    } else {
        return StatusCode::BAD_REQUEST.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn list_channel_replies_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let guard = state.channel_replies.read().unwrap();
    let replies: Vec<Value> = guard
        .get(&(team_id, channel_id, root_id))
        .map(|list| list.iter().map(|r| r.to_graph_json(None)).collect())
        .unwrap_or_default();
    Json(json!({ "value": replies })).into_response()
}

async fn post_channel_reply_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.posted_reply_payloads.write().unwrap().push(payload.clone());

    let id = state.next_id("chan-reply");
    let stored = StoredChatMsg {
        id: id.clone(),
        reply_to_id: Some(root_id.clone()),
        subject: None,
        from_id: "me-user-1".to_string(),
        from_name: "Me (Tester)".to_string(),
        created_at: "2026-11-15T06:15:00Z".to_string(),
        body_html: payload
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mentions: payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        attachments: payload
            .get("attachments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let resp_json = stored.to_graph_json(None);
    state
        .channel_replies
        .write()
        .unwrap()
        .entry((team_id, channel_id, root_id))
        .or_default()
        .push(stored);

    (StatusCode::CREATED, Json(resp_json)).into_response()
}

async fn chat_drive_upload_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let file_name = rest
        .trim_end_matches(":/content")
        .rsplit('/')
        .next()
        .unwrap_or("uploaded.bin")
        .to_string();
    let (ordinal, file_name) = state.record_small_upload("chat", &file_name, query);
    let item_id = format!("drive-item-chat-{ordinal}");
    let share_url = format!("https://contoso.sharepoint.com/personal/tester/Documents/Microsoft%20Teams%20Chat%20Files/{file_name}");

    state
        .uploaded_files
        .write()
        .unwrap()
        .insert(item_id.clone(), body.to_vec());
    state
        .share_contents
        .write()
        .unwrap()
        .insert(share_url.clone(), body.to_vec());

    Json(json!({
        "id": item_id,
        "name": file_name,
        "size": body.len(),
        "eTag": "\"{153FA47D-18C9-4179-BE08-9879815A9F90},1\"",
        "webUrl": share_url,
        "webDavUrl": share_url
    }))
    .into_response()
}

async fn create_link_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(_item_id): Path<String>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let shared_url = "https://contoso.sharepoint.com/:t:/s/chat/Eabc123";
    // Copy the uploaded chat file bytes under the created sharing link too
    if let Some(bytes) = state.uploaded_files.read().unwrap().get("drive-item-chat-1").cloned() {
        state
            .share_contents
            .write()
            .unwrap()
            .insert(shared_url.to_string(), bytes);
    }
    Json(json!({
        "link": { "webUrl": shared_url },
        "webUrl": shared_url
    }))
    .into_response()
}

async fn channel_files_folder_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((_team_id, _channel_id)): Path<(String, String)>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({
        "id": "folder-deploy-1",
        "name": "Deployments",
        "parentReference": {
            "driveId": "drive-sp-dev"
        }
    }))
    .into_response()
}

async fn sp_drive_upload_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((drive_id, rest)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let file_name = rest
        .trim_end_matches(":/content")
        .rsplit('/')
        .next()
        .unwrap_or("channel_file.bin")
        .to_string();
    let (ordinal, file_name) = state.record_small_upload(&drive_id, &file_name, query);
    let item_id = format!("sp-item-{drive_id}-{ordinal}");
    let web_url = format!("https://contoso.sharepoint.com/sites/dev/Shared%20Documents/{file_name}");

    state
        .uploaded_files
        .write()
        .unwrap()
        .insert(item_id.clone(), body.to_vec());
    state
        .share_contents
        .write()
        .unwrap()
        .insert(web_url.clone(), body.to_vec());

    Json(json!({
        "id": item_id,
        "name": file_name,
        "size": body.len(),
        "eTag": "\"{223FA47D-18C9-4179-BE08-9879815A9F91},1\"",
        "webUrl": web_url,
        "webDavUrl": web_url
    }))
    .into_response()
}

async fn download_share_content_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(share_token): Path<String>,
) -> Response {
    if let Some(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let Some(encoded) = share_token.strip_prefix("u!") else {
        return (StatusCode::BAD_REQUEST, "missing u! prefix").into_response();
    };
    let Ok(decoded_bytes) = URL_SAFE_NO_PAD.decode(encoded) else {
        return (StatusCode::BAD_REQUEST, "bad share token").into_response();
    };
    let Ok(decoded_url) = String::from_utf8(decoded_bytes) else {
        return (StatusCode::BAD_REQUEST, "non-utf8 share url").into_response();
    };

    let guard = state.share_contents.read().unwrap();
    match guard.get(&decoded_url) {
        Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, format!("unknown share: {decoded_url}")).into_response(),
    }
}

async fn spawn_mock_graph_server(initial_token: &str) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState::new(initial_token));
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(oauth_token_handler))
        .route("/v1.0/me", get(get_me_handler))
        .route("/v1.0/me/chats", get(list_chats_handler))
        .route("/v1.0/chats/{chat_id}", get(get_chat_handler).patch(patch_chat_handler))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(list_chat_messages_handler).post(post_chat_message_handler),
        )
        .route("/v1.0/me/joinedTeams", get(joined_teams_handler))
        .route("/v1.0/$batch", post(batch_handler))
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            get(list_channel_messages_handler).post(post_channel_message_handler),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}",
            get(get_channel_root_handler).patch(patch_channel_root_handler),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}/replies",
            get(list_channel_replies_handler).post(post_channel_reply_handler),
        )
        .route("/v1.0/me/drive/root:/{*rest}", put(chat_drive_upload_handler))
        .route("/v1.0/me/drive/items/{item_id}/createLink", post(create_link_handler))
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/filesFolder",
            get(channel_files_folder_handler),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{*rest}",
            put(sp_drive_upload_handler),
        )
        .route(
            "/v1.0/shares/{share_token}/driveItem/content",
            get(download_share_content_handler),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Graph server");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock Graph");
    });

    (format!("http://{addr}"), state)
}

fn build_test_config(base_url: &str, download_dir: &std::path::Path) -> AppConfig {
    AppConfig {
        client_id: "test-client-id".to_string(),
        tenant_id: "test-tenant".to_string(),
        graph_base_url: format!("{base_url}/v1.0"),
        auth_base_url: base_url.to_string(),
        download_dir: download_dir.to_string_lossy().to_string(),
        ..AppConfig::default()
    }
}

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

// ===========================================================================
// Pure Teams E2E: Deep @odata.nextLink History, SQLite Tie-Breaking & Scope Diagnostics
// ===========================================================================

fn build_direct_graph_client(
    base_url: &str,
    download_dir: &std::path::Path,
    token: &str,
) -> (AppConfig, GraphClient) {
    let cfg = build_test_config(base_url, download_dir);
    let tokens = StoredTokens {
        access_token: token.to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("me-user-1".to_string()),
        user_display_name: Some("Me (Tester)".to_string()),
        user_email: Some("tester@contoso.com".to_string()),
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth)
        .expect("build direct GraphClient")
        .with_my_user_id("me-user-1");
    (cfg, graph)
}

fn mock_graph_message(
    id: &str,
    created_at: &str,
    body_text: &str,
    reply_to_id: Option<&str>,
    replies: Option<Vec<Value>>,
) -> Value {
    let mut obj = json!({
        "id": id,
        "replyToId": reply_to_id,
        "messageType": "message",
        "createdDateTime": created_at,
        "subject": Value::Null,
        "importance": "normal",
        "from": {
            "user": {
                "id": "peer-kim",
                "displayName": "Alice"
            }
        },
        "body": {
            "contentType": "html",
            "content": format!("<p>{body_text}</p>")
        },
        "mentions": [],
        "attachments": [],
        "reactions": []
    });
    if let Some(r) = replies {
        obj["replies"] = Value::Array(r);
    }
    obj
}

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
    let digest = hex::encode(Sha256::digest(&asset_bytes));
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
// ---------------------------------------------------------------------------------------

/// Serves `app` on an ephemeral loopback port and returns `http://127.0.0.1:PORT`.
async fn spawn_test_router(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test router");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

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

/// Environment for driving the built `teams` binary against a mock Graph listener.
struct CliEnv {
    graph_base: String,
    auth_base: String,
    jwt: String,
    cfg_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    _tmp: TempDir,
}

impl CliEnv {
    fn new(base_url: &str) -> Self {
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
        let tmp = tempfile::tempdir().unwrap();
        Self {
            graph_base: format!("{base_url}/v1.0"),
            auth_base: base_url.to_string(),
            jwt: format!("{header_b64}.{payload_b64}.sig"),
            cfg_dir: tmp.path().join("config"),
            data_dir: tmp.path().join("data"),
            _tmp: tmp,
        }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self._tmp.path().join(name)
    }

    async fn run(&self, args: &[&str]) -> std::process::Output {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_teams"))
            .args(args)
            .env("TEAMS_GRAPH_BASE_URL", &self.graph_base)
            .env("TEAMS_AUTH_BASE_URL", &self.auth_base)
            .env("TEAMS_ACCESS_TOKEN", &self.jwt)
            .env("TEAMS_DISABLE_KEYRING", "1")
            .env("TEAMS_CONFIG_DIR", &self.cfg_dir)
            .env("TEAMS_DATA_DIR", &self.data_dir)
            .env("TEAMS_AUTO_DAEMON", "0")
            .env_remove("TEAMS_TOKEN_FILE")
            .env_remove("TEAMS_TOKEN_REFRESH_CMD")
            .env_remove("TEAMS_EXTRA_CA_PEM")
            .output()
            .await
            .expect("run teams CLI")
    }
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
