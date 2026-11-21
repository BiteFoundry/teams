//! Fuzzy target resolver for CLI, HTTP API, and MCP (`"Alice"`, `"Group Chat"`, `"Engineering/General"`, `chat:...`, `chan:...:...`).

use anyhow::{bail, Result};

use crate::{
    graph::GraphClient,
    models::{ConversationKind, ConversationSummary, ConversationTarget},
    storage::Storage,
};

/// Resolves a user-supplied conversation query against the known conversation list.
///
/// Supported query forms:
/// 1. Exact `conv_key` (`chat:...` or `chan:<team_id>:<channel_id>`)
/// 2. Raw `chat_id` (`19:...@thread.v2` or `19:...@unq.gbl.spaces`) or `channel_id`
/// 3. Case-insensitive exact match on chat `title` or channel `"team_name/title"` (handling `/` in team names),
///    then exact case-insensitive channel `title`
/// 4. Slash notation `"<team substring>/<channel substring>"` (and chat title substring when query has `/`)
/// 5. Case-insensitive unique substring match on `title` (or `"team_name/title"`)
pub fn resolve_from_slice(
    conversations: &[ConversationSummary],
    query: &str,
) -> Result<ConversationSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Target conversation/channel name is empty.");
    }

    // Priority 1: Exact conv_key
    if let Some(exact) = conversations.iter().find(|c| c.conv_key == q) {
        return Ok(exact.clone());
    }

    // Priority 2: Exact raw chat_id or channel_id
    let raw_id_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| match &c.target {
            ConversationTarget::Chat { chat_id } => chat_id == q,
            ConversationTarget::Channel { channel_id, .. } => channel_id == q,
        })
        .collect();
    if raw_id_matches.len() == 1 {
        return Ok(raw_id_matches[0].clone());
    } else if raw_id_matches.len() > 1 {
        bail!(
            "Multiple conversations/channels match target ID '{q}':\n{}",
            format_candidates(&raw_id_matches)
        );
    }

    let q_lower = q.to_lowercase();

    // Priority 3: Case-insensitive exact match across full chat title and full "TeamName/ChannelName"
    let exact_full_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| {
            if c.kind == ConversationKind::Channel {
                c.team_name
                    .as_deref()
                    .is_some_and(|team| matches_exact_team_channel(&q_lower, team, &c.title))
            } else {
                c.title.trim().to_lowercase() == q_lower
            }
        })
        .collect();
    if exact_full_matches.len() == 1 {
        return Ok(exact_full_matches[0].clone());
    } else if exact_full_matches.len() > 1 {
        bail!(
            "Multiple targets match '{q}' exactly. Use conv_key or 'TeamName/ChannelName':\n{}",
            format_candidates(&exact_full_matches)
        );
    }

    // Priority 3b: Case-insensitive exact channel title match (e.g. "General" without team prefix)
    let exact_channel_title_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| c.kind == ConversationKind::Channel && c.title.trim().to_lowercase() == q_lower)
        .collect();
    if exact_channel_title_matches.len() == 1 {
        return Ok(exact_channel_title_matches[0].clone());
    } else if exact_channel_title_matches.len() > 1 {
        bail!(
            "Multiple targets match title '{q}' exactly. Use conv_key or 'TeamName/ChannelName':\n{}",
            format_candidates(&exact_channel_title_matches)
        );
    }

    // Priority 4: Slash notation partial match "<team_part>/<channel_part>" (supports '/' inside team names)
    if q.contains('/') {
        let slash_matches: Vec<&ConversationSummary> = conversations
            .iter()
            .filter(|c| {
                if c.kind == ConversationKind::Channel {
                    let tname = c.team_name.as_deref().unwrap_or("").to_lowercase();
                    let cname = c.title.to_lowercase();
                    q.match_indices('/').any(|(idx, _)| {
                        let team_part = q[..idx].trim().to_lowercase();
                        let chan_part = q[idx + 1..].trim().to_lowercase();
                        (!team_part.is_empty() || !chan_part.is_empty())
                            && (team_part.is_empty() || tname.contains(&team_part))
                            && (chan_part.is_empty() || cname.contains(&chan_part))
                    })
                } else {
                    c.title.to_lowercase().contains(&q_lower)
                }
            })
            .collect();

        if slash_matches.len() == 1 {
            return Ok(slash_matches[0].clone());
        } else if slash_matches.len() > 1 {
            bail!(
                "Multiple targets match '{q}'. Please specify a more specific target:\n{}",
                format_candidates(&slash_matches)
            );
        }
    }

    // Priority 5: Unique case-insensitive substring match on title or "team_name/title"
    let sub_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| {
            let title_lower = c.title.to_lowercase();
            if title_lower.contains(&q_lower) {
                return true;
            }
            if let Some(team_name) = &c.team_name {
                let combined = format!("{team_name}/{}", c.title).to_lowercase();
                if combined.contains(&q_lower) {
                    return true;
                }
            }
            false
        })
        .collect();

    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous target '{q}' matches multiple conversations ({n}):\n{}",
            format_candidates(&sub_matches)
        ),
        _ => bail!("Conversation/channel not found: {q}"),
    }
}

fn matches_exact_team_channel(q_lower: &str, team_name: &str, channel_title: &str) -> bool {
    let team_lower = team_name.trim().to_lowercase();
    let chan_lower = channel_title.trim().to_lowercase();
    if team_lower.is_empty() || chan_lower.is_empty() {
        return false;
    }
    if let Some(rest) = q_lower.strip_prefix(&team_lower) {
        if let Some(after_slash) = rest.trim_start().strip_prefix('/') {
            return after_slash.trim() == chan_lower;
        }
    }
    false
}

/// Resolves against local SQLite cache first, refreshing from Graph once if not found.
pub async fn resolve_conversation(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
) -> Result<ConversationSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Target conversation/channel name is empty.");
    }

    let cached = storage.list_conversations()?;
    if let Ok(found) = resolve_from_slice(&cached, q) {
        return Ok(found);
    }

    if q.starts_with("chat:") {
        let fetched = graph.list_chats_with_cursor(150).await?;
        storage.upsert_conversations_with_null_topics(
            &fetched.chats,
            &fetched.explicit_null_topic_keys,
        )?;
        let refreshed = storage.list_conversations()?;
        if let Ok(found) = resolve_from_slice(&refreshed, q) {
            return Ok(found);
        }
        if let Some(target) = ConversationTarget::from_conv_key(q) {
            return Ok(summary_from_target(target));
        }
        return resolve_from_slice(&refreshed, q);
    }

    if q.starts_with("chan:") {
        let fetched = graph.list_joined_teams_with_channels().await?;
        storage.upsert_joined_teams(&fetched)?;
        let refreshed = storage.list_conversations()?;
        if let Ok(found) = resolve_from_slice(&refreshed, q) {
            return Ok(found);
        }
        if let Some(target) = ConversationTarget::from_conv_key(q) {
            return Ok(summary_from_target(target));
        }
        return resolve_from_slice(&refreshed, q);
    }

    let (chats_res, teams_res) = tokio::join!(
        graph.list_chats_with_cursor(150),
        graph.list_joined_teams_with_channels(),
    );

    let mut chats_err: Option<anyhow::Error> = None;
    let mut teams_err: Option<anyhow::Error> = None;
    match chats_res {
        Ok(c) => {
            storage.upsert_conversations_with_null_topics(&c.chats, &c.explicit_null_topic_keys)?;
        }
        Err(err) => {
            chats_err = Some(err);
        }
    }
    match teams_res {
        Ok(t) => {
            storage.upsert_joined_teams(&t)?;
        }
        Err(err) => {
            teams_err = Some(err);
        }
    }

    let refreshed = storage.list_conversations()?;
    match resolve_from_slice(&refreshed, q) {
        Ok(found) => Ok(found),
        Err(resolve_err) => match (chats_err, teams_err) {
            (Some(ce), Some(te)) => Err(anyhow::anyhow!(
                "{resolve_err} (chats lookup failed: {ce:#}; teams/channels lookup failed: {te:#})"
            )),
            (Some(ce), None) => Err(anyhow::anyhow!(
                "{resolve_err} (chats lookup failed: {ce:#})"
            )),
            (None, Some(te)) => Err(anyhow::anyhow!(
                "{resolve_err} (teams/channels lookup failed: {te:#})"
            )),
            (None, None) => Err(resolve_err),
        },
    }
}

fn summary_from_target(target: ConversationTarget) -> ConversationSummary {
    let conv_key = target.conv_key();
    let (kind, team_id, title) = match &target {
        ConversationTarget::Chat { chat_id } => (ConversationKind::Group, None, chat_id.clone()),
        ConversationTarget::Channel {
            team_id,
            channel_id,
        } => (
            ConversationKind::Channel,
            Some(team_id.clone()),
            channel_id.clone(),
        ),
    };
    ConversationSummary {
        conv_key,
        target,
        kind,
        team_id,
        team_name: None,
        title,
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
    }
}

fn format_candidates(candidates: &[&ConversationSummary]) -> String {
    const MAX_LIST: usize = 8;
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_LIST)
        .map(|c| match c.team_name.as_deref() {
            Some(team) if c.kind == ConversationKind::Channel && !team.is_empty() => {
                format!("  - {team}/{} ({})", c.title, c.conv_key)
            }
            _ => format!("  - {} ({})", c.title, c.conv_key),
        })
        .collect();
    if candidates.len() > MAX_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_LIST));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chat(chat_id: &str, title: &str) -> ConversationSummary {
        let target = ConversationTarget::Chat {
            chat_id: chat_id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
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
            last_updated_at: None,
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }
    }

    fn make_channel(
        team_id: &str,
        team_name: &str,
        channel_id: &str,
        title: &str,
    ) -> ConversationSummary {
        let target = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::Channel,
            team_id: Some(team_id.to_string()),
            team_name: Some(team_name.to_string()),
            title: title.to_string(),
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
        }
    }

    #[test]
    fn test_resolver_priorities_and_slash_notation() {
        let convs = vec![
            make_chat("19:alice@unq.gbl.spaces", "Alice Smith"),
            make_chat("19:bob@unq.gbl.spaces", "Bob Jones"),
            make_channel("t1", "Engineering", "19:gen1@thread.tacv2", "General"),
            make_channel("t1", "Engineering", "19:dep1@thread.tacv2", "Deploy"),
            make_channel("t2", "Infra", "19:dep2@thread.tacv2", "Deploy-Alerts"),
            make_channel("t2", "Infra", "19:gen2@thread.tacv2", "General"),
        ];

        // Priority 1: exact conv_key
        let r1 = resolve_from_slice(&convs, "chan:t1:19:dep1@thread.tacv2").unwrap();
        assert_eq!(r1.title, "Deploy");

        // Priority 2: raw chat_id / channel_id
        let r2 = resolve_from_slice(&convs, "19:alice@unq.gbl.spaces").unwrap();
        assert_eq!(r2.title, "Alice Smith");
        let r2b = resolve_from_slice(&convs, "19:dep2@thread.tacv2").unwrap();
        assert_eq!(r2b.title, "Deploy-Alerts");

        // Priority 3: slash notation exact & substring
        let r3 = resolve_from_slice(&convs, "Engineering/Deploy").unwrap();
        assert_eq!(r3.conv_key, "chan:t1:19:dep1@thread.tacv2");
        let r3b = resolve_from_slice(&convs, "inf/alerts").unwrap();
        assert_eq!(r3b.conv_key, "chan:t2:19:dep2@thread.tacv2");

        // Priority 4: exact title match
        let r4 = resolve_from_slice(&convs, "Alice Smith").unwrap();
        assert_eq!(r4.conv_key, "chat:19:alice@unq.gbl.spaces");

        // Priority 5: unique substring match
        let r5 = resolve_from_slice(&convs, "Jones").unwrap();
        assert_eq!(r5.title, "Bob Jones");
    }

    #[test]
    fn test_resolver_ambiguity_error_lists_candidates() {
        let convs = vec![
            make_channel("t1", "Engineering", "c1", "General"),
            make_channel("t2", "Design", "c2", "General"),
            make_channel("t3", "Engineering2", "c3", "Deploy-General"),
        ];

        // Ambiguous exact title
        let err = resolve_from_slice(&convs, "General").unwrap_err().to_string();
        assert!(err.contains("Engineering/General (chan:t1:c1)"));
        assert!(err.contains("Design/General (chan:t2:c2)"));

        // Ambiguous slash notation
        let err2 = resolve_from_slice(&convs, "Eng/General")
            .unwrap_err()
            .to_string();
        assert!(err2.contains("Engineering/General (chan:t1:c1)"));
        assert!(err2.contains("Engineering2/Deploy-General (chan:t3:c3)"));
    }

    #[test]
    fn test_resolver_empty_and_not_found() {
        let convs = vec![make_chat("c1", "Alice")];
        assert!(resolve_from_slice(&convs, "   ").is_err());
        let err = resolve_from_slice(&convs, "nonexistent")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn test_resolve_conversation_domain_fast_path_and_error_preservation() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use axum::{http::StatusCode, routing::{get, post}, Json, Router};
        use serde_json::json;
        use tokio::net::TcpListener;

        use crate::{
            auth::{AuthManager, StoredTokens},
            config::AppConfig,
        };

        let chats_hits = Arc::new(AtomicUsize::new(0));
        let teams_hits = Arc::new(AtomicUsize::new(0));
        let fail_mode = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let ch_hits = chats_hits.clone();
        let ch_fail = fail_mode.clone();
        let tm_hits = teams_hits.clone();
        let tm_fail = fail_mode.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(move || {
                    let hits = ch_hits.clone();
                    let fail = ch_fail.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if fail.load(Ordering::SeqCst) {
                            return (
                                StatusCode::FORBIDDEN,
                                Json(json!({"error": {"code": "Authorization_RequestDenied", "message": "Forbidden"}})),
                            )
                                .into_response();
                        }
                        Json(json!({
                            "value": [{
                                "id": "19:c1@thread.v2",
                                "chatType": "group",
                                "topic": "Ops Room",
                                "members": []
                            }]
                        }))
                        .into_response()
                    }
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(move || {
                    let hits = tm_hits.clone();
                    let fail = tm_fail.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if fail.load(Ordering::SeqCst) {
                            return (
                                StatusCode::FORBIDDEN,
                                Json(json!({"error": {"code": "Authorization_RequestDenied", "message": "Forbidden"}})),
                            )
                                .into_response();
                        }
                        Json(json!({
                            "value": [{
                                "id": "t1",
                                "displayName": "Engineering"
                            }]
                        }))
                        .into_response()
                    }
                }),
            )
            .route(
                "/v1.0/$batch",
                post(|| async {
                    Json(json!({
                        "responses": [{
                            "id": "0",
                            "status": 200,
                            "body": {
                                "value": [{
                                    "id": "19:ch1@thread.tacv2",
                                    "displayName": "General",
                                    "membershipType": "standard"
                                }]
                            }
                        }]
                    }))
                }),
            );

        use axum::response::IntoResponse;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
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
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1");

        // 1. "chat:..." fast-path hits ONLY /me/chats, not /me/joinedTeams
        let storage1 = Storage::open_in_memory().unwrap();
        let found_chat = resolve_conversation(&storage1, &graph, "chat:19:c1@thread.v2")
            .await
            .unwrap();
        assert_eq!(found_chat.title, "Ops Room");
        assert_eq!(chats_hits.load(Ordering::SeqCst), 1);
        assert_eq!(teams_hits.load(Ordering::SeqCst), 0);

        // Unlisted valid chat:... falls back to ConversationTarget::from_conv_key without hitting teams
        let unlisted_chat = resolve_conversation(&storage1, &graph, "chat:19:older@thread.v2")
            .await
            .unwrap();
        assert_eq!(unlisted_chat.conv_key, "chat:19:older@thread.v2");
        assert_eq!(teams_hits.load(Ordering::SeqCst), 0);

        // 2. "chan:..." fast-path hits ONLY /me/joinedTeams, not /me/chats
        let storage2 = Storage::open_in_memory().unwrap();
        let prev_chats = chats_hits.load(Ordering::SeqCst);
        let found_chan = resolve_conversation(&storage2, &graph, "chan:t1:19:ch1@thread.tacv2")
            .await
            .unwrap();
        assert_eq!(found_chan.title, "General");
        assert_eq!(chats_hits.load(Ordering::SeqCst), prev_chats);
        assert_eq!(teams_hits.load(Ordering::SeqCst), 1);

        // 3. 403 error on cold cache is preserved rather than masked as "not found"
        fail_mode.store(true, Ordering::SeqCst);
        let storage3 = Storage::open_in_memory().unwrap();
        let err = resolve_conversation(&storage3, &graph, "MissingRoom")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("403") || err.contains("Forbidden") || err.contains("Authorization_RequestDenied"),
            "expected Graph 403 error, got: {err}"
        );

        server.abort();
    }
}
