//! Shared in-process mock Microsoft Graph v1.0 + Entra OAuth server and test harness helpers.

use std::{
    collections::HashMap,
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
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use teams_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
};

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();

pub(crate) fn ensure_isolated_env() {
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
pub(crate) struct StoredChatMsg {
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
pub(crate) struct MockState {
    pub(crate) expected_token: RwLock<String>,
    pub(crate) refresh_calls: RwLock<usize>,
    pub(crate) me_auth_headers: RwLock<Vec<String>>,
    pub(crate) next_msg_seq: RwLock<u64>,
    pub(crate) chat_messages: RwLock<HashMap<String, Vec<StoredChatMsg>>>,
    pub(crate) group_topic: RwLock<Option<String>>,
    pub(crate) channel_messages: RwLock<HashMap<(String, String), Vec<StoredChatMsg>>>,
    pub(crate) channel_replies: RwLock<HashMap<(String, String, String), Vec<StoredChatMsg>>>,
    pub(crate) uploaded_files: RwLock<HashMap<String, Vec<u8>>>,
    pub(crate) share_contents: RwLock<HashMap<String, Vec<u8>>>,
    pub(crate) posted_chat_payloads: RwLock<Vec<Value>>,
    pub(crate) posted_reply_payloads: RwLock<Vec<Value>>,
    /// Raw query strings of every small-file `PUT …:/content` upload, in arrival order.
    pub(crate) upload_queries: RwLock<Vec<String>>,
    /// Per-test switch: emulate Graph `conflictBehavior=rename` (a second PUT of a name
    /// creates a new driveItem). Off by default so existing tests see identical responses.
    pub(crate) emulate_upload_rename: RwLock<bool>,
    pub(crate) upload_name_counts: RwLock<HashMap<String, usize>>,
}

pub(crate) fn check_bearer_header(headers: &HeaderMap, expected: &str) -> Option<Response> {
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

pub(crate) fn mock_teams_payload() -> Vec<Value> {
    vec![json!({
        "id": "team-dev",
        "displayName": "Engineering",
        "description": "Engineering Team"
    })]
}

pub(crate) fn mock_channels_payload() -> Vec<Value> {
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

pub(crate) async fn spawn_mock_graph_server(initial_token: &str) -> (String, Arc<MockState>) {
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

pub(crate) fn build_test_config(base_url: &str, download_dir: &std::path::Path) -> AppConfig {
    AppConfig {
        client_id: "test-client-id".to_string(),
        tenant_id: "test-tenant".to_string(),
        graph_base_url: format!("{base_url}/v1.0"),
        auth_base_url: base_url.to_string(),
        download_dir: download_dir.to_string_lossy().to_string(),
        ..AppConfig::default()
    }
}

pub(crate) fn build_direct_graph_client(
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

pub(crate) fn mock_graph_message(
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

/// Serves `app` on an ephemeral loopback port and returns `http://127.0.0.1:PORT`.
pub(crate) async fn spawn_test_router(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test router");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

/// Environment for driving the built `teams` binary against a mock Graph listener.
pub(crate) struct CliEnv {
    pub(crate) graph_base: String,
    pub(crate) auth_base: String,
    pub(crate) jwt: String,
    pub(crate) cfg_dir: std::path::PathBuf,
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) _tmp: TempDir,
}

impl CliEnv {
    pub(crate) fn new(base_url: &str) -> Self {
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

    pub(crate) fn path(&self, name: &str) -> std::path::PathBuf {
        self._tmp.path().join(name)
    }

    pub(crate) async fn run(&self, args: &[&str]) -> std::process::Output {
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
