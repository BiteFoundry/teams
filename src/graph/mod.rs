//! Microsoft Graph v1.0 client for Teams Chats, Teams/Channels/Threads, Media, and Presence.

pub mod chats;
pub mod client;
pub mod media;
pub mod presence;
pub mod teams;

use std::{
    cmp::Ordering,
    collections::HashSet,
    sync::{atomic::AtomicBool, Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;

use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{ConversationTarget, MemberInfo, MessageItem},
};

/// Canonical prefix emitted by `GraphClient` HTTP errors; kept as a shared constant for safe parsing.
pub const GRAPH_API_ERROR_PREFIX: &str = "Graph API error (";

/// HTTP status of a `GraphClient` error message (`Graph API error (METHOD URL): 404 …`).
///
/// Only the three digits right after `): ` count, so a status-looking number inside the
/// URL, a message id or the error body is never mistaken for the response status.
pub(crate) fn extract_graph_http_status(msg: &str) -> Option<u16> {
    let start = msg.find(GRAPH_API_ERROR_PREFIX)?;
    let rest = &msg[start..];
    let (_, after) = rest.split_once("): ")?;
    let bytes = after.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_digit()
        || !bytes[1].is_ascii_digit()
        || !bytes[2].is_ascii_digit()
    {
        return None;
    }
    if bytes.len() > 3 && bytes[3].is_ascii_digit() {
        return None;
    }
    after[..3].parse::<u16>().ok()
}

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    my_user_id: Option<String>,
    cached_my_user_id: Arc<OnceLock<String>>,
    chats_orderby_unsupported: Arc<AtomicBool>,
    users_filter_unsupported: Arc<AtomicBool>,
    resolve_user_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = crate::http::client_builder()?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            auth,
            config,
            http,
            my_user_id: None,
            cached_my_user_id: Arc::new(OnceLock::new()),
            chats_orderby_unsupported: Arc::new(AtomicBool::new(false)),
            users_filter_unsupported: Arc::new(AtomicBool::new(false)),
            resolve_user_mutex: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn with_my_user_id(mut self, user_id: impl Into<String>) -> Self {
        let uid = user_id.into();
        let lock = OnceLock::new();
        let _ = lock.set(uid.clone());
        self.cached_my_user_id = Arc::new(lock);
        self.my_user_id = Some(uid);
        self
    }

    pub fn my_user_id(&self) -> Option<&str> {
        self.my_user_id
            .as_deref()
            .or_else(|| self.cached_my_user_id.get().map(String::as_str))
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    // --- Unified Messaging Operations (Chats + Channel Posts) ---
    pub async fn list_members(&self, target: &ConversationTarget) -> Result<Vec<MemberInfo>> {
        match target {
            ConversationTarget::Chat { chat_id } => self.list_chat_members(chat_id).await,
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => self.list_channel_members(team_id, channel_id).await,
        }
    }

    pub async fn list_messages(
        &self,
        target: &ConversationTarget,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        match target {
            ConversationTarget::Chat { chat_id } => self.list_chat_messages(chat_id, limit).await,
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => self.list_channel_messages(team_id, channel_id, limit).await,
        }
    }

    /// Fetches up to `limit` newest root messages (plus any expanded channel replies) across
    /// one or more Graph pages, preserving the final `@odata.nextLink` for history cursors.
    pub async fn list_messages_with_cursor(
        &self,
        target: &ConversationTarget,
        limit: usize,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let mut items = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut root_count = 0usize;
        let mut next_link: Option<String> = None;

        loop {
            let page_top = if next_link.is_none() && limit <= 50 {
                limit
            } else {
                50
            };
            let (page_items, next) = self
                .list_messages_page(target, page_top, next_link.as_deref())
                .await?;
            for item in page_items {
                if seen_ids.insert(item.msg_id.clone()) {
                    if item.parent_msg_id.is_none() {
                        root_count += 1;
                    }
                    items.push(item);
                }
            }
            next_link = next;
            if root_count >= limit || next_link.is_none() {
                break;
            }
        }

        client::sort_messages_chronologically(&mut items);
        Ok((items, next_link))
    }

    /// Fetches a single page of root messages (oldest-first within the returned batch) and returns `(messages, next_link)`.
    pub async fn list_messages_page(
        &self,
        target: &ConversationTarget,
        top: usize,
        next_link: Option<&str>,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        match target {
            ConversationTarget::Chat { chat_id } => {
                self.list_chat_messages_page(chat_id, top, next_link).await
            }
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => {
                self.list_channel_messages_page(team_id, channel_id, top, next_link)
                    .await
            }
        }
    }

    /// Fetches older root messages using `@odata.nextLink` cursor pagination (with automatic fallback if `$skiptoken` expired),
    /// returning `(older_messages_chronological, updated_next_link)`.
    pub async fn fetch_older_messages(
        &self,
        target: &ConversationTarget,
        saved_next_link: Option<&str>,
        older_than_ts: Option<&str>,
        min_count: usize,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        const MAX_PAGES_PER_FETCH: usize = 15;
        self.fetch_older_messages_bounded(
            target,
            saved_next_link,
            older_than_ts,
            min_count,
            MAX_PAGES_PER_FETCH,
        )
        .await
    }

    /// [`Self::fetch_older_messages`] with an explicit page budget: at most `max_pages` Graph
    /// requests are made, and the returned link is the page the walk stopped before.
    pub async fn fetch_older_messages_bounded(
        &self,
        target: &ConversationTarget,
        saved_next_link: Option<&str>,
        older_than_ts: Option<&str>,
        min_count: usize,
        max_pages: usize,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        let desired = min_count.max(1);
        let older_than_ts = older_than_ts.map(str::trim).filter(|s| !s.is_empty());
        let initial_cursor = saved_next_link.map(str::trim).filter(|s| !s.is_empty());

        let mut collected = Vec::new();
        let mut seen_keys = HashSet::new();
        let mut older_root_count = 0usize;
        let mut current_link: Option<String> = None;

        for page_idx in 0..max_pages.max(1) {
            let (page_msgs, next_link) = if page_idx == 0 {
                if let Some(link) = initial_cursor {
                    match self.list_messages_page(target, 50, Some(link)).await {
                        Ok(res) => res,
                        Err(err) if is_invalid_skiptoken_error(&err) => {
                            self.list_messages_page(target, 50, None).await?
                        }
                        Err(err) => return Err(err),
                    }
                } else {
                    self.list_messages_page(target, 50, None).await?
                }
            } else {
                self.list_messages_page(target, 50, current_link.as_deref())
                    .await?
            };

            for msg in page_msgs {
                if !seen_keys.insert(msg.msg_key.clone()) {
                    continue;
                }
                if msg.parent_msg_id.is_none() {
                    let counts_as_older = match older_than_ts {
                        Some(ts) => compare_timestamps(&msg.created_at, ts) == Ordering::Less,
                        None => true,
                    };
                    if counts_as_older {
                        older_root_count += 1;
                    }
                }
                collected.push(msg);
            }

            current_link = next_link;
            if older_root_count >= desired || current_link.is_none() {
                break;
            }
        }

        if let Some(ts) = older_than_ts {
            let retained_root_ids: HashSet<String> = collected
                .iter()
                .filter(|m| {
                    m.parent_msg_id.is_none()
                        && compare_timestamps(&m.created_at, ts) != Ordering::Greater
                })
                .map(|m| m.msg_id.clone())
                .collect();
            collected.retain(|m| match m.parent_msg_id.as_deref() {
                None => retained_root_ids.contains(&m.msg_id),
                Some(pid) => retained_root_ids.contains(pid),
            });
        }

        client::sort_messages_chronologically(&mut collected);
        Ok((collected, current_link))
    }
}

fn compare_timestamps(lhs: &str, rhs: &str) -> Ordering {
    match (
        chrono::DateTime::parse_from_rfc3339(lhs),
        chrono::DateTime::parse_from_rfc3339(rhs),
    ) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => lhs.cmp(rhs),
    }
}

fn is_invalid_skiptoken_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    let detail = msg
        .strip_prefix(GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    let lower = detail.to_ascii_lowercase();
    if detail.contains("401")
        || detail.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
    {
        return false;
    }
    detail.contains("400")
        || detail.contains("404")
        || detail.contains("410")
        || lower.contains("bad request")
        || lower.contains("badrequest")
        || lower.contains("invalidskiptoken")
        || lower.contains("skiptoken")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fetch_older_messages_skiptoken_fallback_eof_and_403() {
        use axum::{extract::RawQuery, http::StatusCode, routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let p2_url = format!("{base}/chats/c-hist/messages?$skiptoken=page2");
        let p3_url = format!("{base}/chats/c-hist/messages?$skiptoken=page3");
        let expired_url = format!("{base}/chats/c-hist/messages?$skiptoken=expired");
        let forbidden_url = format!("{base}/chats/c-hist/messages?$skiptoken=forbidden");
        let (p2_c, p3_c) = (p2_url.clone(), p3_url.clone());

        let app = Router::new().route(
            "/v1.0/chats/c-hist/messages",
            get(move |RawQuery(q): RawQuery| {
                let (p2_url, p3_url) = (p2_c.clone(), p3_c.clone());
                async move {
                    let query = q.unwrap_or_default();
                    if query.contains("skiptoken=forbidden") {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({
                                "error": { "code": "Forbidden", "message": "Access denied" }
                            })),
                        );
                    }
                    if query.contains("skiptoken=expired") {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": { "code": "InvalidSkipToken", "message": "skiptoken expired" }
                            })),
                        );
                    }
                    if query.contains("skiptoken=page2") {
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "m3",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:03:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "m3" }
                                    },
                                    {
                                        "id": "m2",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:02:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "m2" }
                                    }
                                ],
                                "@odata.nextLink": p3_url
                            })),
                        );
                    }
                    if query.contains("skiptoken=page3") {
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "m1",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:01:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "m1" }
                                    }
                                ]
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m5",
                                    "messageType": "message",
                                    "createdDateTime": "2026-11-18T10:05:00Z",
                                    "from": { "user": { "id": "u1", "displayName": "A" } },
                                    "body": { "contentType": "text", "content": "m5" }
                                },
                                {
                                    "id": "m4",
                                    "messageType": "message",
                                    "createdDateTime": "2026-11-18T10:04:00Z",
                                    "from": { "user": { "id": "u1", "displayName": "A" } },
                                    "body": { "contentType": "text", "content": "m4" }
                                }
                            ],
                            "@odata.nextLink": p2_url
                        })),
                    )
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: base,
            ..Default::default()
        };
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1");
        let target = ConversationTarget::Chat {
            chat_id: "c-hist".to_string(),
        };

        // 1. Expired $skiptoken falls back to page 1, walks page 2, retains <= 10:04, counts < 10:04
        let (older, next_cursor) = client
            .fetch_older_messages(
                &target,
                Some(&expired_url),
                Some("2026-11-18T10:04:00Z"),
                2,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = older.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["m2", "m3", "m4"]);
        assert_eq!(next_cursor.as_deref(), Some(p3_url.as_str()));

        // 2. Final page returns EOF (next_cursor == None)
        let (oldest, eof_cursor) = client
            .fetch_older_messages(
                &target,
                next_cursor.as_deref(),
                Some("2026-11-18T10:02:00Z"),
                5,
            )
            .await
            .unwrap();
        let oldest_ids: Vec<&str> = oldest.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(oldest_ids, vec!["m1"]);
        assert_eq!(eof_cursor, None);

        // 3. 403 Forbidden with $skiptoken in URL must NOT be swallowed
        let err = client
            .fetch_older_messages(&target, Some(&forbidden_url), None, 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // TEAMS-27: the page budget is a hard cap, and the returned link is where the walk stopped.
    #[tokio::test]
    async fn fetch_older_messages_bounded_stops_after_max_pages() {
        use axum::{extract::RawQuery, routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let hits = Arc::new(AtomicUsize::new(0));
        let route_hits = Arc::clone(&hits);
        let route_base = base.clone();

        // Ten pages of one message each, newest first: page N holds the message of 10:(60-N).
        let app = Router::new().route(
            "/v1.0/chats/c-deep/messages",
            get(move |RawQuery(q): RawQuery| {
                let hits = Arc::clone(&route_hits);
                let base = route_base.clone();
                async move {
                    hits.fetch_add(1, AtomicOrdering::SeqCst);
                    let query = q.unwrap_or_default();
                    let page = query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("$skiptoken=page"))
                        .and_then(|n| n.parse::<usize>().ok())
                        .unwrap_or(1);
                    let mut body = serde_json::json!({
                        "value": [{
                            "id": format!("m{page}"),
                            "messageType": "message",
                            "createdDateTime": format!("2026-11-18T10:{:02}:00Z", 60 - page),
                            "from": { "user": { "id": "u1", "displayName": "A" } },
                            "body": { "contentType": "text", "content": "x" }
                        }]
                    });
                    if page < 10 {
                        body["@odata.nextLink"] = serde_json::Value::String(format!(
                            "{base}/chats/c-deep/messages?$skiptoken=page{}",
                            page + 1
                        ));
                    }
                    Json(body)
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: base.clone(),
            ..AppConfig::default()
        };
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");
        let target = ConversationTarget::Chat {
            chat_id: "c-deep".to_string(),
        };

        // Pages 1 and 2 hold nothing older than the window, so the budget runs out first.
        let (msgs, link) = graph
            .fetch_older_messages_bounded(&target, None, Some("2026-11-18T10:57:00Z"), 5, 2)
            .await
            .unwrap();
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 2, "at most max_pages requests");
        assert!(msgs.is_empty(), "nothing older than the window was reached: {msgs:?}");
        assert_eq!(
            link.as_deref(),
            Some(format!("{base}/chats/c-deep/messages?$skiptoken=page3").as_str())
        );

        // The unbounded entry point keeps its larger budget: it walks until five messages
        // strictly older than the window are found (pages 4..=8) and keeps the boundary one.
        hits.store(0, AtomicOrdering::SeqCst);
        let (msgs, link) = graph
            .fetch_older_messages(&target, None, Some("2026-11-18T10:57:00Z"), 5)
            .await
            .unwrap();
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 8);
        let ids: Vec<&str> = msgs.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, ["m8", "m7", "m6", "m5", "m4", "m3"]);
        assert_eq!(
            link.as_deref(),
            Some(format!("{base}/chats/c-deep/messages?$skiptoken=page9").as_str())
        );
    }
}
