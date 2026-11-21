//! Local HTTP JSON API (`teams serve` / `teams api`) powered by `axum`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    models::{ConversationKind, ConversationTarget, MentionInfo},
    parser, resolver,
    storage::Storage,
    sync::{self, SyncCommand},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8788,
            token: None,
        }
    }
}

/// Shared state passed to every axum request handler.
pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub token: Option<String>,
}

/// JSON error returned by every failing handler.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        let status = if cli::is_forbidden_error(&err) {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self::new(status, format!("{err:#}"))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub user_id: Option<String>,
    pub version: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConversationsQuery {
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CreateChatBody {
    pub members: Vec<String>,
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TeamsQuery {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct MessagesQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub fetch_older: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MentionInput {
    Spec(String),
    Object {
        #[serde(default)]
        id: Option<i32>,
        user_id: String,
        display_name: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct SendMessageBody {
    pub markdown: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub mentions: Vec<MentionInput>,
    #[serde(default)]
    pub important: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ThreadQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReplyBody {
    pub markdown: String,
    #[serde(default)]
    pub mentions: Vec<MentionInput>,
    #[serde(default)]
    pub important: bool,
}

#[derive(Debug, Deserialize)]
pub struct SendFileBody {
    pub path: String,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub thread_root_id: Option<String>,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadBody {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub out_dir: Option<String>,
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReactionBody {
    pub reaction: String,
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct EditMessageBody {
    pub markdown: String,
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteMessageParams {
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PresenceQuery {
    #[serde(default)]
    pub user_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetPresenceBody {
    pub availability: String,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default)]
    pub expiration_mins: Option<u32>,
}


/// Build the `axum::Router` for the local HTTP JSON API.
pub fn build_router(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    token: Option<String>,
) -> axum::Router {
    let has_token = token.as_deref().is_some_and(|t| !t.trim().is_empty());
    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        token,
    });

    let cors = if has_token {
        CorsLayer::permissive()
    } else {
        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(|origin, _| {
                origin.to_str().ok().is_some_and(is_loopback_origin)
            }))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
    };

    Router::new()
        .route("/health", get(health))
        .route("/conversations", get(list_conversations))
        .route("/chats", post(create_chat))
        .route("/teams", get(list_teams))
        .route(
            "/conversations/{key}/messages",
            get(get_messages).post(send_message),
        )
        .route("/conversations/{key}/threads/{root_id}", get(get_thread))
        .route(
            "/conversations/{key}/messages/{msg_id}/replies",
            post(reply_message),
        )
        .route("/conversations/{key}/files", post(send_file))
        .route(
            "/conversations/{key}/messages/{msg_id}/attachments/download",
            post(download_attachment),
        )
        .route(
            "/conversations/{key}/messages/{msg_id}/reactions",
            post(react_message),
        )
        .route(
            "/conversations/{key}/messages/{msg_id}",
            patch(edit_message).delete(delete_message),
        )
        .route("/conversations/{key}/read", post(mark_read))
        .route("/conversations/{key}/members", get(list_members))
        .route("/search", get(search))
        .route("/presence", get(get_presence).post(set_presence))
        .route("/openapi.json", get(openapi_spec))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .layer(cors)
        .with_state(state)
}

/// Start the HTTP JSON API server and background sync engine until stopped.
pub async fn run(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("Invalid bind address: {}:{}", opts.bind, opts.port))?;
    let allow_insecure = std::env::var("TEAMS_ALLOW_INSECURE_SERVE").ok();
    check_bind_security(addr, opts.token.as_deref(), allow_insecure.as_deref())?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let sync_ctl =
        sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), event_tx);
    sync_ctl.request(SyncCommand::RefreshConversations);

    let app = build_router(config, graph, storage, opts.token);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind listener: {addr}"))?;

    println!("teams HTTP API listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;

    Ok(())
}

fn check_bind_security(
    addr: SocketAddr,
    token: Option<&str>,
    allow_insecure_env: Option<&str>,
) -> Result<()> {
    let has_token = token.is_some_and(|t| !t.trim().is_empty());
    let allow_insecure = allow_insecure_env.map(str::trim) == Some("1");
    if !addr.ip().is_loopback() && !has_token && !allow_insecure {
        anyhow::bail!(
            "Refusing to bind HTTP API on non-loopback address {addr} without `--token <SECRET>`. \
             Pass `--token <SECRET>` to require Bearer authentication before exposing Teams and host file endpoints, \
             or set `TEAMS_ALLOW_INSECURE_SERVE=1` to override."
        );
    }
    Ok(())
}

fn is_loopback_origin(origin: &str) -> bool {
    let Ok(parsed) = url::Url::parse(origin) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

async fn with_storage<T, F>(storage: &Storage, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
{
    let storage = storage.clone();
    tokio::task::spawn_blocking(move || f(&storage))
        .await
        .map_err(|err| anyhow::anyhow!("storage task panicked: {err}"))?
}

async fn map_graph_error(graph: &GraphClient, err: anyhow::Error) -> ApiError {
    if cli::is_forbidden_error(&err) {
        let wrapped = cli::wrap_graph_permission_error(graph, err).await;
        return ApiError::new(StatusCode::FORBIDDEN, format!("{wrapped:#}"));
    }
    ApiError::from(err)
}

async fn map_target_error(graph: &GraphClient, err: anyhow::Error) -> ApiError {
    if cli::is_forbidden_error(&err) {
        let wrapped = cli::wrap_graph_permission_error(graph, err).await;
        return ApiError::new(StatusCode::FORBIDDEN, format!("{wrapped:#}"));
    }
    let msg = format!("{err:#}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("target conversation/channel name is empty") {
        ApiError::bad_request(msg)
    } else if lower.contains("conversation/channel not found")
        || lower.contains("ambiguous target")
        || lower.starts_with("multiple ")
    {
        ApiError::not_found(msg)
    } else {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

async fn map_download_error(graph: &GraphClient, err: anyhow::Error) -> ApiError {
    if cli::is_forbidden_error(&err) {
        let wrapped = cli::wrap_graph_permission_error(graph, err).await;
        return ApiError::new(StatusCode::FORBIDDEN, format!("{wrapped:#}"));
    }
    let msg = format!("{err:#}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("message not found") {
        ApiError::not_found(msg)
    } else if lower.contains("has no attachments") || lower.contains("is out of range") {
        ApiError::bad_request(msg)
    } else {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

async fn resolve_target_for_api(
    state: &AppState,
    key: &str,
    offline: bool,
) -> Result<crate::models::ConversationSummary, ApiError> {
    if offline {
        let key_owned = key.to_string();
        return with_storage(&state.storage, move |s| {
            let cached = s.list_conversations()?;
            resolver::resolve_from_slice(&cached, &key_owned)
        })
        .await
        .map_err(|err| {
            let msg = format!("{err:#}");
            if msg
                .to_ascii_lowercase()
                .contains("target conversation/channel name is empty")
            {
                ApiError::bad_request(msg)
            } else if msg
                .to_ascii_lowercase()
                .contains("conversation/channel not found")
                || msg.to_ascii_lowercase().contains("ambiguous target")
                || msg.to_ascii_lowercase().starts_with("multiple ")
            {
                ApiError::not_found(msg)
            } else {
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        });
    }

    match cli::resolve_target(&state.storage, &state.graph, key, false).await {
        Ok(conv) => Ok(conv),
        Err(err) => Err(map_target_error(&state.graph, err).await),
    }
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(expected) = &state.token {
        if request.uri().path() != "/health" {
            let presented = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));

            if presented != Some(expected.as_str()) {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token",
                ));
            }
        }
    }

    Ok(next.run(request).await)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        user_id: state.graph.my_user_id().map(ToString::to_string),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ConversationsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.map(cli::clamp_limit);

    if query.refresh {
        if let Some(chat_limit) = limit {
            let (chats_res, teams_res) = tokio::join!(
                state.graph.list_chats(chat_limit),
                state.graph.list_joined_teams_with_channels(),
            );
            let chats = match chats_res {
                Ok(c) => c,
                Err(err) => return Err(map_graph_error(&state.graph, err).await),
            };
            with_storage(&state.storage, move |s| {
                s.upsert_conversations(&chats)?;
                if let Ok(teams) = teams_res {
                    s.upsert_teams(&teams)?;
                }
                Ok(())
            })
            .await?;
        } else if let Err(err) = sync::bootstrap_conversations(&state.graph, &state.storage).await {
            return Err(map_graph_error(&state.graph, err).await);
        }
    }

    let kind_filter = match query.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => Some(parse_kind_filter(raw)?),
    };

    let conversations = with_storage(&state.storage, move |s| {
        let mut items = s.list_conversations()?;
        if let Some(kind) = kind_filter {
            items.retain(|c| c.kind == kind);
        }
        if let Some(max_items) = limit {
            items.truncate(max_items);
        }
        Ok(items)
    })
    .await?;

    Ok(Json(json!({ "conversations": conversations })))
}

async fn create_chat(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateChatBody>,
) -> Result<Json<Value>, ApiError> {
    let members: Vec<String> = body
        .members
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    if members.is_empty() {
        return Err(ApiError::bad_request("members must not be empty"));
    }

    let chat = match state
        .graph
        .create_chat(&members, body.topic.as_deref())
        .await
    {
        Ok(c) => c,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let chat_to_store = chat.clone();
    with_storage(&state.storage, move |s| {
        s.upsert_conversations(std::slice::from_ref(&chat_to_store))?;
        Ok(())
    })
    .await?;

    Ok(Json(json!({ "ok": true, "conversation": chat })))
}

async fn list_teams(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TeamsQuery>,
) -> Result<Json<Value>, ApiError> {
    let teams = if query.refresh {
        let fetched = match state.graph.list_joined_teams_with_channels().await {
            Ok(t) => t,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        with_storage(&state.storage, move |s| {
            s.upsert_teams(&fetched)?;
            let stored = s.list_teams()?;
            Ok(if stored.is_empty() { fetched } else { stored })
        })
        .await?
    } else {
        with_storage(&state.storage, |s| s.list_teams()).await?
    };

    Ok(Json(json!({ "teams": teams })))
}

async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let conv = resolve_target_for_api(&state, &key, query.offline).await?;

    let before_owned = query
        .before
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(ref raw_before) = before_owned {
        let conv_key = conv.conv_key.clone();
        let before_val = raw_before.clone();
        with_storage(&state.storage, move |s| {
            cli::resolve_before_cursor(s, &conv_key, &before_val)
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    }

    if query.offline {
        let conv_clone = conv.clone();
        let res = with_storage(&state.storage, move |s| {
            cli::read_conversation_messages_offline(s, &conv_clone, limit, before_owned.as_deref())
        })
        .await?;
        return Ok(Json(json!({
            "conversation": conv,
            "messages": res.messages,
            "has_more": res.has_more,
            "next_before": res.next_before,
        })));
    }

    let fetch_older = query.fetch_older.unwrap_or(before_owned.is_some());
    let res = match cli::read_conversation_messages(
        &state.storage,
        &state.graph,
        &conv,
        limit,
        before_owned.as_deref(),
        false,
        fetch_older,
    )
    .await
    {
        Ok(r) => r,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };

    Ok(Json(json!({
        "conversation": conv,
        "messages": res.messages,
        "has_more": res.has_more,
        "next_before": res.next_before,
    })))
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let mentions = parse_mention_inputs(&body.mentions)?;
    let conv = resolve_target_for_api(&state, &key, false).await?;

    let mut outgoing = parser::compile_outgoing_markdown(
        &body.markdown,
        body.subject,
        mentions,
        vec![],
        vec![],
        None,
    );
    if body.important {
        outgoing.importance = Some("high".to_string());
    }

    let mut sent = match state.graph.send_message(&conv.target, &outgoing).await {
        Ok(m) => m,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let sent_clone = sent.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.upsert_messages(std::slice::from_ref(&sent_clone))
    })
    .await;
    cli::hydrate_message_attachments(&mut sent);

    Ok(Json(json!({
        "ok": true,
        "conversation": conv,
        "message": sent,
    })))
}

async fn get_thread(
    State(state): State<Arc<AppState>>,
    Path((key, root_id)): Path<(String, String)>,
    Query(query): Query<ThreadQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let conv = resolve_target_for_api(&state, &key, query.offline).await?;

    let ConversationTarget::Channel {
        ref team_id,
        ref channel_id,
    } = conv.target
    else {
        return Err(ApiError::bad_request(format!(
            "threads are only supported on team channels: {}",
            conv.conv_key
        )));
    };

    let mut thread = if query.offline {
        let conv_key = conv.conv_key.clone();
        let root_id_clone = root_id.clone();
        with_storage(&state.storage, move |s| s.get_thread(&conv_key, &root_id_clone, limit))
            .await?
            .ok_or_else(|| ApiError::not_found(format!("thread not found in cache: {root_id}")))?
    } else {
        let (view, complete) = match state
            .graph
            .get_thread_with_coverage(team_id, channel_id, &root_id, limit)
            .await
        {
            Ok(v) => v,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let cached = view.clone();
        with_storage(&state.storage, move |s| s.save_fetched_thread(&cached, complete)).await?;
        view
    };
    cli::hydrate_thread_attachments(&mut thread);

    Ok(Json(json!({ "thread": thread })))
}

async fn reply_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Json(body): Json<ReplyBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let mentions = parse_mention_inputs(&body.mentions)?;
    let conv = resolve_target_for_api(&state, &key, false).await?;

    let quote = cli::build_quote_reply(&state.storage, &state.graph, &conv, &msg_id).await;
    let mut outgoing =
        parser::compile_outgoing_markdown(&body.markdown, None, mentions, vec![], vec![], quote);
    if body.important {
        outgoing.importance = Some("high".to_string());
    }

    let mut sent = match state
        .graph
        .reply_message(&conv.target, &msg_id, &outgoing)
        .await
    {
        Ok(m) => m,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let sent_clone = sent.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.upsert_messages(std::slice::from_ref(&sent_clone))
    })
    .await;
    cli::hydrate_message_attachments(&mut sent);

    Ok(Json(json!({
        "ok": true,
        "conversation": conv,
        "reply_to": msg_id,
        "message": sent,
    })))
}

async fn send_file(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<SendFileBody>,
) -> Result<Json<Value>, ApiError> {
    let trimmed_path = body.path.trim();
    if trimmed_path.is_empty() {
        return Err(ApiError::bad_request("path must not be empty"));
    }
    let path = std::path::Path::new(trimmed_path);
    if !path.exists() || !path.is_file() {
        return Err(ApiError::bad_request(format!(
            "file does not exist on host: {}",
            path.display()
        )));
    }
    if body.inline && cli::image_mime_for_path(path).is_none() {
        return Err(ApiError::bad_request(
            "inline=true requires an image file (png, jpg, jpeg, gif, webp, bmp)",
        ));
    }

    let conv = resolve_target_for_api(&state, &key, false).await?;

    let sent = match cli::send_file_or_inline(
        &state.graph,
        &state.storage,
        &conv,
        path,
        body.caption.as_deref(),
        body.thread_root_id.as_deref(),
        body.inline,
    )
    .await
    {
        Ok(m) => m,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };

    Ok(Json(json!({
        "ok": true,
        "conversation": conv,
        "message": sent,
    })))
}

async fn download_attachment(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let parsed: DownloadBody = if body.iter().all(u8::is_ascii_whitespace) {
        DownloadBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };

    let conv = resolve_target_for_api(&state, &key, false).await?;

    let dest_dir = parsed
        .out_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.config.resolved_download_dir());

    let downloaded = match cli::download_message_attachment(
        &state.graph,
        &state.storage,
        &conv,
        &msg_id,
        parsed.index.unwrap_or(0),
        parsed.thread_root_id.as_deref(),
        &dest_dir,
    )
    .await
    {
        Ok(d) => d,
        Err(err) => return Err(map_download_error(&state.graph, err).await),
    };

    Ok(Json(json!({
        "ok": true,
        "path": downloaded.path,
        "file_name": downloaded.file_name,
        "mime_type": downloaded.mime_type,
        "size_bytes": downloaded.bytes_written,
        "bytes_written": downloaded.bytes_written,
        "is_image": downloaded.is_image,
    })))
}

async fn react_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Json(body): Json<ReactionBody>,
) -> Result<Json<Value>, ApiError> {
    let normalized = cli::normalize_reaction(&body.reaction)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let conv = resolve_target_for_api(&state, &key, false).await?;

    if let Err(err) = state
        .graph
        .set_reaction(
            &conv.target,
            &msg_id,
            body.thread_root_id.as_deref(),
            &normalized,
            body.enable,
        )
        .await
    {
        return Err(map_graph_error(&state.graph, err).await);
    }

    let my_name = state
        .graph
        .auth()
        .current_tokens()
        .await
        .and_then(|t| t.user_display_name);
    let ck = conv.conv_key.clone();
    let mid = msg_id.clone();
    let rx = normalized.clone();
    let enable = body.enable;
    let _ = with_storage(&state.storage, move |s| {
        s.apply_local_reaction(&ck, &mid, &rx, enable, my_name.as_deref())
            .map(|_| ())
    })
    .await;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
        "msg_id": msg_id,
        "reaction": normalized,
        "enable": body.enable,
    })))
}

async fn edit_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Json(body): Json<EditMessageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let conv = resolve_target_for_api(&state, &key, false).await?;
    let compiled =
        parser::compile_outgoing_markdown(&body.markdown, None, vec![], vec![], vec![], None);

    if let Err(err) = state
        .graph
        .edit_message(
            &conv.target,
            &msg_id,
            body.thread_root_id.as_deref(),
            &compiled.body_html,
        )
        .await
    {
        return Err(map_graph_error(&state.graph, err).await);
    }

    let ck = conv.conv_key.clone();
    let mid = msg_id.clone();
    let html = compiled.body_html.clone();
    let now = chrono::Utc::now().to_rfc3339();
    let _ = with_storage(&state.storage, move |s| {
        s.apply_local_edit(&ck, &mid, &html, &now).map(|_| ())
    })
    .await;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
        "msg_id": msg_id,
    })))
}

async fn delete_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Query(query): Query<DeleteMessageParams>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body_params: DeleteMessageParams = if body.iter().all(u8::is_ascii_whitespace) {
        DeleteMessageParams::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };

    let thread_root_id = body_params
        .thread_root_id
        .or(query.thread_root_id)
        .filter(|s| !s.trim().is_empty());

    let conv = resolve_target_for_api(&state, &key, false).await?;

    if let Err(err) = state
        .graph
        .delete_message(&conv.target, &msg_id, thread_root_id.as_deref())
        .await
    {
        return Err(map_graph_error(&state.graph, err).await);
    }

    let ck = conv.conv_key.clone();
    let mid = msg_id.clone();
    let now = chrono::Utc::now().to_rfc3339();
    let _ = with_storage(&state.storage, move |s| {
        s.apply_local_soft_delete(&ck, &mid, &now).map(|_| ())
    })
    .await;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
        "msg_id": msg_id,
    })))
}

async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conv = resolve_target_for_api(&state, &key, false).await?;

    if let ConversationTarget::Chat { ref chat_id } = conv.target {
        if let Err(err) = state.graph.mark_chat_read(chat_id).await {
            return Err(map_graph_error(&state.graph, err).await);
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let conv_key = conv.conv_key.clone();
    with_storage(&state.storage, move |s| {
        s.mark_conversation_read(&conv_key, &now)
    })
    .await?;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
    })))
}

async fn list_members(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conv = resolve_target_for_api(&state, &key, false).await?;

    let members = match state.graph.list_members(&conv.target).await {
        Ok(m) => m,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let conv_key = conv.conv_key.clone();
    let members_clone = members.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.upsert_members(&conv_key, &members_clone)
    })
    .await;

    Ok(Json(json!({
        "conv_key": conv.conv_key,
        "title": conv.title,
        "members": members,
    })))
}

async fn search(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let q = query.q.trim().to_string();
    if q.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let target_opt = query
        .target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let q_clone = q.clone();
    let hits = with_storage(&state.storage, move |s| {
        let conv_key_filter = match target_opt {
            Some(ref target) => {
                let cached = s.list_conversations()?;
                Some(resolver::resolve_from_slice(&cached, target)?.conv_key)
            }
            None => None,
        };
        s.search_messages(&q_clone, conv_key_filter.as_deref(), limit)
    })
    .await
    .map_err(|err| {
        let msg = format!("{err:#}");
        let lower = msg.to_ascii_lowercase();
        if lower.contains("conversation/channel not found")
            || lower.contains("ambiguous target")
            || lower.starts_with("multiple ")
        {
            ApiError::not_found(msg)
        } else {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
        }
    })?;

    Ok(Json(json!({
        "query": q,
        "hits": hits,
    })))
}

async fn get_presence(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PresenceQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(raw_ids) = query.user_ids.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let ids: Vec<String> = raw_ids
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        if ids.is_empty() {
            return Err(ApiError::bad_request("user_ids must contain at least one ID"));
        }
        let presences = match state.graph.get_presences_by_user_ids(&ids).await {
            Ok(p) => p,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        Ok(Json(json!({ "presences": presences })))
    } else {
        let presence = match state.graph.get_my_presence().await {
            Ok(p) => p,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        Ok(Json(json!({ "presence": presence })))
    }
}

async fn set_presence(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetPresenceBody>,
) -> Result<Json<Value>, ApiError> {
    let availability = cli::normalize_presence_availability(&body.availability)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let activity = body
        .activity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&availability);

    if let Err(err) = state
        .graph
        .set_my_presence(&availability, activity, body.expiration_mins)
        .await
    {
        return Err(map_graph_error(&state.graph, err).await);
    }

    let my_uid = match state.graph.my_user_id().map(str::to_string) {
        Some(id) if !id.is_empty() => Some(id),
        _ => state
            .graph
            .auth()
            .current_tokens()
            .await
            .and_then(|t| t.user_id),
    };
    if let Some(uid) = my_uid.filter(|id| !id.is_empty()) {
        let status = crate::models::PresenceStatus {
            user_id: uid,
            availability: availability.clone(),
            activity: activity.to_string(),
        };
        let _ = with_storage(&state.storage, move |s| {
            s.upsert_presences(std::slice::from_ref(&status))
        })
        .await;
    }

    Ok(Json(json!({
        "ok": true,
        "availability": availability,
        "activity": activity,
        "expiration_mins": body.expiration_mins,
    })))
}


/// Minimal OpenAPI 3.0 description of every route exposed by `build_router`.
pub fn openapi_document() -> Value {
    let ops: &[(&str, &str, &str)] = &[
        ("/health", "get", "Health check (public)"),
        ("/conversations", "get", "List chats & channels (supports ?limit=100&refresh=true)"),
        ("/chats", "post", "Create a chat"),
        ("/teams", "get", "List joined teams"),
        ("/conversations/{key}/messages", "get post", "Read (supports ?limit=&before=&fetch_older=&offline=) / send messages"),
        ("/conversations/{key}/threads/{root_id}", "get", "Read a channel thread"),
        ("/conversations/{key}/messages/{msg_id}/replies", "post", "Reply to a message"),
        ("/conversations/{key}/files", "post", "Send a file"),
        ("/conversations/{key}/messages/{msg_id}/attachments/download", "post", "Download attachment"),
        ("/conversations/{key}/messages/{msg_id}/reactions", "post", "React to a message"),
        ("/conversations/{key}/messages/{msg_id}", "patch delete", "Edit / delete a message"),
        ("/conversations/{key}/read", "post", "Mark conversation read"),
        ("/conversations/{key}/members", "get", "List members"),
        ("/search", "get", "Search Teams messages"),
        ("/presence", "get post", "Get / set presence"),
        ("/openapi.json", "get", "This OpenAPI document"),
    ];

    let mut paths = serde_json::Map::new();
    for (path, methods, summary) in ops {
        let mut item = serde_json::Map::new();
        for m in methods.split_whitespace() {
            item.insert(
                m.to_string(),
                json!({
                    "summary": summary,
                    "responses": { "200": { "description": "JSON response" } },
                }),
            );
        }
        paths.insert((*path).to_string(), Value::Object(item));
    }

    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "teams-cli local HTTP API (Microsoft Teams)",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": { "type": "http", "scheme": "bearer" }
            }
        },
        "security": [ { "bearerAuth": [] } ],
        "paths": Value::Object(paths),
    })
}

async fn openapi_spec() -> Json<Value> {
    Json(openapi_document())
}

fn parse_kind_filter(raw: &str) -> Result<ConversationKind, ApiError> {
    match raw.to_ascii_lowercase().as_str() {
        "oneonone" | "one_on_one" | "1:1" => Ok(ConversationKind::OneOnOne),
        "group" => Ok(ConversationKind::Group),
        "meeting" => Ok(ConversationKind::Meeting),
        "channel" => Ok(ConversationKind::Channel),
        _ => Err(ApiError::bad_request(format!(
            "invalid kind '{raw}' (expected oneOnOne, group, meeting, or channel)"
        ))),
    }
}

fn parse_mention_inputs(inputs: &[MentionInput]) -> Result<Vec<MentionInfo>, ApiError> {
    let mut out = Vec::with_capacity(inputs.len());
    for (idx, item) in inputs.iter().enumerate() {
        match item {
            MentionInput::Spec(spec) => {
                let parsed = cli::parse_mention_spec(spec, idx as i32)
                    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
                out.push(parsed);
            }
            MentionInput::Object {
                id,
                user_id,
                display_name,
            } => {
                let uid = user_id.trim();
                let name = display_name.trim();
                if uid.is_empty() || name.is_empty() {
                    return Err(ApiError::bad_request(
                        "mention user_id and display_name must not be empty",
                    ));
                }
                out.push(MentionInfo {
                    id: id.unwrap_or(idx as i32),
                    user_id: uid.to_string(),
                    display_name: name.to_string(),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;

    async fn spawn_test_server(token: Option<String>) -> String {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-123");
        let storage = Storage::open_in_memory().unwrap();
        let router = build_router(cfg, graph, storage, token);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn health_is_public_and_protected_routes_require_bearer_token() {
        let base = spawn_test_server(Some("secret-token".to_string())).await;
        let client = reqwest::Client::new();

        // /health must succeed without Authorization header
        let res = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: HealthResponse = res.json().await.unwrap();
        assert!(body.ok);
        assert_eq!(body.user_id.as_deref(), Some("me-123"));
        assert_eq!(body.version, env!("CARGO_PKG_VERSION"));

        // Protected route without token -> 401 JSON error
        let res = client
            .get(format!("{base}/conversations"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let err: Value = res.json().await.unwrap();
        assert!(err["error"].as_str().is_some());

        // Protected route with wrong token -> 401
        let res = client
            .get(format!("{base}/conversations"))
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Protected route with valid token -> 200
        let res = client
            .get(format!("{base}/conversations"))
            .bearer_auth("secret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let data: Value = res.json().await.unwrap();
        assert!(data["conversations"].is_array());
    }

    #[tokio::test]
    async fn validation_rejects_empty_bodies_and_bad_queries() {
        let base = spawn_test_server(None).await;
        let client = reqwest::Client::new();

        let res = client
            .post(format!("{base}/chats"))
            .json(&json!({ "members": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .get(format!("{base}/search?q=%20%20"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .get(format!("{base}/conversations?kind=bogus"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/presence"))
            .json(&json!({ "availability": "InvalidState" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/conversations/chat%3A1/messages"))
            .json(&json!({ "markdown": "  " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parses_mention_inputs_both_formats() {
        let parsed = parse_mention_inputs(&[
            MentionInput::Spec("u1:Alice".to_string()),
            MentionInput::Object {
                id: Some(5),
                user_id: "u2".to_string(),
                display_name: "Bob".to_string(),
            },
        ])
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, 0);
        assert_eq!(parsed[0].user_id, "u1");
        assert_eq!(parsed[1].id, 5);
        assert_eq!(parsed[1].display_name, "Bob");
    }

    #[tokio::test]
    async fn offline_routes_resolve_url_encoded_conv_key_and_search() {
        use crate::models::MessageItem;

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let conv = crate::models::ConversationSummary {
            conv_key: "chat:19:room1@thread.v2".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "19:room1@thread.v2".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Alice".to_string(),
            topic: None,
            unread_count: 1,
            last_read_at: None,
            last_msg_id: Some("m1".to_string()),
            last_msg_author: Some("Alice".to_string()),
            last_msg_preview: Some("Deploy complete".to_string()),
            last_updated_at: Some("2026-11-15T06:00:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u-alice".to_string()),
            web_url: None,
        };
        storage.upsert_conversations(&[conv]).unwrap();
        storage
            .upsert_messages(&[MessageItem {
                msg_key: "chat:19:room1@thread.v2|m1".to_string(),
                conv_key: "chat:19:room1@thread.v2".to_string(),
                msg_id: "m1".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u-alice".to_string()),
                author_name: "Alice".to_string(),
                is_me: false,
                created_at: "2026-11-15T06:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>Deployment completed</p>".to_string(),
                body_plain: "Deployment completed".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            }])
            .unwrap();

        let router = build_router(cfg, graph, storage, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // URL-encoded conv_key resolves cleanly in offline mode
        let res = client
            .get(format!(
                "{base}/conversations/chat%3A19%3Aroom1%40thread.v2/messages?offline=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["conversation"]["title"], "Alice");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["body_plain"], "Deployment completed");

        // Search finds cached message
        let res = client
            .get(format!("{base}/search?q=Deployment&target=Alice"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let search_body: Value = res.json().await.unwrap();
        assert_eq!(search_body["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search_body["hits"][0]["msg_id"], "m1");
    }

    #[tokio::test]
    async fn conversations_limit_messages_before_pagination_and_pure_teams_openapi() {
        use crate::models::MessageItem;

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let mk_conv = |id: &str, title: &str, updated: &str| crate::models::ConversationSummary {
            conv_key: format!("chat:{id}"),
            target: ConversationTarget::Chat {
                chat_id: id.to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: title.to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: Some(updated.to_string()),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        };
        storage
            .upsert_conversations(&[
                mk_conv("c1", "First Room", "2026-11-15T06:00:00Z"),
                mk_conv("c2", "Second Room", "2026-11-15T05:00:00Z"),
            ])
            .unwrap();

        let mk_msg = |id: &str, ts: &str| MessageItem {
            msg_key: format!("chat:c1|{id}"),
            conv_key: "chat:c1".to_string(),
            msg_id: id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u1".to_string()),
            author_name: "Bob".to_string(),
            is_me: false,
            created_at: ts.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: format!("<p>{id}</p>"),
            body_plain: id.to_string(),
            attachments: vec![],
            mentions: vec![],
            reactions: vec![],
            reply_count: 0,
            web_url: None,
        };
        storage
            .upsert_messages(&[
                mk_msg("m1", "2026-10-30T10:00:00Z"),
                mk_msg("m2", "2026-10-31T10:00:00Z"),
                mk_msg("m3", "2026-11-01T10:00:00Z"),
            ])
            .unwrap();

        let router = build_router(cfg, graph, storage, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // `GET /conversations?limit=1` truncates to 1
        let res = client
            .get(format!("{base}/conversations?limit=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let conv_body: Value = res.json().await.unwrap();
        assert_eq!(conv_body["conversations"].as_array().unwrap().len(), 1);

        // `GET /conversations/{key}/messages?before=m3&limit=1&offline=true`
        let res = client
            .get(format!(
                "{base}/conversations/chat%3Ac1/messages?before=m3&limit=1&offline=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let msg_body: Value = res.json().await.unwrap();
        assert_eq!(msg_body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(msg_body["messages"][0]["msg_id"], "m2");
        assert_eq!(msg_body["has_more"], true);
        assert_eq!(msg_body["next_before"], "m2");

        // Invalid `before` -> 400 BAD_REQUEST
        let res = client
            .get(format!(
                "{base}/conversations/chat%3Ac1/messages?before=not-a-date&offline=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // `/openapi.json` advertises only pure Teams endpoints
        let res = client.get(format!("{base}/openapi.json")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let spec: Value = res.json().await.unwrap();
        let paths = spec["paths"].as_object().unwrap();
        assert!(paths.contains_key("/conversations"));
        assert!(paths.contains_key("/conversations/{key}/messages"));
        for p in paths.keys() {
            assert!(
                !p.starts_with("/mail")
                    && !p.starts_with("/calendar")
                    && !p.starts_with("/contacts")
                    && !p.starts_with("/todo")
                    && !p.contains("onenote"),
                "unexpected non-Teams route in openapi: {p}"
            );
        }
    }

    #[test]
    fn non_loopback_bind_requires_token_or_env_override() {
        let loopback: SocketAddr = "127.0.0.1:8788".parse().unwrap();
        let any_v4: SocketAddr = "0.0.0.0:8788".parse().unwrap();
        let lan_v4: SocketAddr = "192.168.1.10:8788".parse().unwrap();

        assert!(check_bind_security(loopback, None, None).is_ok());
        assert!(check_bind_security(any_v4, None, None).is_err());
        assert!(check_bind_security(lan_v4, None, Some("0")).is_err());
        assert!(check_bind_security(any_v4, Some("   "), None).is_err());
        assert!(check_bind_security(any_v4, Some("secret"), None).is_ok());
        assert!(check_bind_security(any_v4, None, Some("1")).is_ok());
    }

    #[tokio::test]
    async fn cors_restricts_unauthenticated_remote_origins_and_allows_loopback() {
        let base_no_tok = spawn_test_server(None).await;
        let client = reqwest::Client::new();

        // Remote origin must not receive Access-Control-Allow-Origin when --token is None
        let res = client
            .get(format!("{base_no_tok}/conversations"))
            .header(header::ORIGIN, "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "unexpected ACAO header for remote origin without token"
        );

        // Localhost origin IS allowed when --token is None
        let res = client
            .get(format!("{base_no_tok}/conversations"))
            .header(header::ORIGIN, "http://localhost:3000")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:3000")
        );

        // Loopback IPv6 origin IS allowed
        assert!(is_loopback_origin("http://[::1]:5173"));
        assert!(!is_loopback_origin("https://localhost.evil.com"));
    }

    #[tokio::test]
    async fn download_and_target_resolution_preserve_403_and_500_status_codes() {
        use crate::auth::StoredTokens;

        let mock_app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "error": {
                                "code": "Authorization_RequestDenied",
                                "message": "Insufficient privileges"
                            }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "error": {
                                "code": "Authorization_RequestDenied",
                                "message": "Insufficient privileges"
                            }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/chats/{chat_id}/messages",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": { "code": "InternalServerError", "message": "boom" } })),
                    )
                }),
            );
        let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_server = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app).await.unwrap();
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{mock_addr}/v1.0"),
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
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-1");
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_conversations(&[crate::models::ConversationSummary {
                conv_key: "chat:c-err".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c-err".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "ErrRoom".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            }])
            .unwrap();

        let router = build_router(cfg, graph, storage, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api_server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // Target resolution encountering 403 from Graph returns 403 Forbidden (not 404)
        let res = client
            .get(format!("{base}/conversations/UnknownChat/messages"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // download_attachment encountering Graph 500 returns 500 (not 400)
        let res = client
            .post(format!(
                "{base}/conversations/chat%3Ac-err/messages/m999/attachments/download"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

        api_server.abort();
        mock_server.abort();
    }

    #[tokio::test]
    async fn cached_inline_hosted_image_exposed_in_messages_threads_and_download() {
        use crate::auth::StoredTokens;
        use crate::models::{ConversationSummary, MessageItem};

        let png_bytes = b"\x89PNG\r\n\x1a\nhttp-inline-shot".to_vec();
        let served_bytes = png_bytes.clone();
        let mock_app = Router::new().route(
            "/v1.0/chats/c1/messages/m2/hostedContents/1/$value",
            get(move || {
                let bytes = served_bytes.clone();
                async move { (StatusCode::OK, bytes) }
            }),
        );
        let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-1");
        let storage = Storage::open_in_memory().unwrap();

        storage
            .upsert_conversations(&[
                ConversationSummary {
                    conv_key: "chat:c1".to_string(),
                    target: ConversationTarget::Chat {
                        chat_id: "c1".to_string(),
                    },
                    kind: ConversationKind::OneOnOne,
                    team_id: None,
                    team_name: None,
                    title: "Inline Chat".to_string(),
                    topic: None,
                    unread_count: 0,
                    last_read_at: None,
                    last_msg_id: None,
                    last_msg_author: None,
                    last_msg_preview: None,
                    last_updated_at: None,
                    pinned: false,
                    peer_user_id: None,
                    web_url: None,
                },
                ConversationSummary {
                    conv_key: "chan:t1:ch1".to_string(),
                    target: ConversationTarget::Channel {
                        team_id: "t1".to_string(),
                        channel_id: "ch1".to_string(),
                    },
                    kind: ConversationKind::Channel,
                    team_id: Some("t1".to_string()),
                    team_name: Some("Eng".to_string()),
                    title: "General".to_string(),
                    topic: None,
                    unread_count: 0,
                    last_read_at: None,
                    last_msg_id: None,
                    last_msg_author: None,
                    last_msg_preview: None,
                    last_updated_at: None,
                    pinned: false,
                    peer_user_id: None,
                    web_url: None,
                },
            ])
            .unwrap();

        let graph_hosted_url =
            "https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value";
        let local_hosted_url =
            format!("http://{mock_addr}/v1.0/chats/c1/messages/m2/hostedContents/1/$value");

        let mk_cached = |conv_key: &str, msg_id: &str, parent: Option<&str>, ts: &str, url: &str| {
            MessageItem {
                msg_key: format!("{conv_key}|{msg_id}"),
                conv_key: conv_key.to_string(),
                msg_id: msg_id.to_string(),
                parent_msg_id: parent.map(str::to_string),
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u1".to_string()),
                author_name: "Alice".to_string(),
                is_me: false,
                created_at: ts.to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: format!(r#"<p>Inline</p><img src="{url}">"#),
                body_plain: "Inline".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            }
        };

        storage
            .upsert_messages(&[
                mk_cached("chat:c1", "m1", None, "2026-11-18T10:00:00Z", graph_hosted_url),
                mk_cached("chat:c1", "m2", None, "2026-11-18T10:01:00Z", &local_hosted_url),
                mk_cached(
                    "chan:t1:ch1",
                    "root-1",
                    None,
                    "2026-11-18T10:02:00Z",
                    graph_hosted_url,
                ),
            ])
            .unwrap();

        let router = build_router(cfg, graph, storage, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api_server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // GET /conversations/{key}/messages?offline=true exposes hosted:1
        let res = client
            .get(format!("{base}/conversations/chat%3Ac1/messages?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["messages"][0]["attachments"][0]["id"], "hosted:1");
        assert_eq!(
            body["messages"][0]["attachments"][0]["content_url"],
            graph_hosted_url
        );

        // GET /conversations/{key}/threads/{root_id}?offline=true exposes hosted:1
        let res = client
            .get(format!(
                "{base}/conversations/chan%3At1%3Ach1/threads/root-1?offline=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let thread_body: Value = res.json().await.unwrap();
        assert_eq!(
            thread_body["thread"]["root"]["attachments"][0]["id"],
            "hosted:1"
        );

        // POST /conversations/{key}/messages/{msg_id}/attachments/download downloads hosted:1
        let res = client
            .post(format!(
                "{base}/conversations/chat%3Ac1/messages/m2/attachments/download"
            ))
            .json(&json!({ "index": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let dl_body: Value = res.json().await.unwrap();
        assert_eq!(dl_body["ok"], true);
        assert_eq!(dl_body["bytes_written"], png_bytes.len());
        let saved_path = dl_body["path"].as_str().unwrap();
        assert_eq!(std::fs::read(saved_path).unwrap(), png_bytes);

        api_server.abort();
        mock_server.abort();
    }
}
