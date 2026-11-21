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
}

fn check_bearer_header(headers: &HeaderMap, expected: &str) -> Result<(), Response> {
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if actual == Some(expected) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "code": "InvalidAuthenticationToken",
                    "message": "Access token has expired or is not valid."
                }
            })),
        )
            .into_response())
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

    fn check_bearer(&self, headers: &HeaderMap) -> Result<(), Response> {
        let expected = self.expected_token.read().unwrap().clone();
        check_bearer_header(headers, &expected)
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({ "value": mock_teams_payload() })).into_response()
}

async fn batch_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
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
        let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("");
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    body: Bytes,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let file_name = rest
        .trim_end_matches(":/content")
        .rsplit('/')
        .next()
        .unwrap_or("uploaded.bin")
        .to_string();
    let item_id = "drive-item-chat-1".to_string();
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    body: Bytes,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let file_name = rest
        .trim_end_matches(":/content")
        .rsplit('/')
        .next()
        .unwrap_or("channel_file.bin")
        .to_string();
    let item_id = format!("sp-item-{drive_id}-1");
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
    if let Err(resp) = state.check_bearer(&headers) {
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
    let teams: Vec<TeamSummary> = graph
        .list_joined_teams_with_channels()
        .await
        .expect("list_joined_teams_with_channels");
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].display_name, "Engineering");
    assert_eq!(teams[0].channels.len(), 2);
    storage.upsert_teams(&teams).expect("upsert teams");

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
        16,
        8,
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
                if let Err(resp) = check_bearer_header(&headers, "inline-img-tok") {
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
                if let Err(resp) = check_bearer_header(&headers, "inline-img-tok") {
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
                        let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("");
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
