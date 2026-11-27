//! Chat, team, channel, member, user directory, search, and presence CLI commands.

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{ConversationSummary, ConversationTarget, MemberInfo, MessageItem, SyncCursor, TeamChannelError, TeamSummary},
    parser, resolver,
    safe_text::sanitize_terminal_inline,
    storage::Storage,
};

use super::{
    format::{
        clamp_limit, fetch_stamp, paginate, print_channels_output, print_chats_output,
        print_teams_output, users_search_json, wrap_graph_permission_error,
    },
    runtime::{
        chats_cache_satisfies_limit, cli_cache_ttl_secs, is_cursor_fresh, open_runtime,
        open_storage_only, record_chats_list_cursor, record_cli_sync_cursor, resolve_daemon_warm,
    },
};

pub(super) async fn cmd_chats(
    cfg: &AppConfig,
    limit: usize,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached
                || daemon_warm
                || is_cursor_fresh(&storage, &["cli:chats", SyncCursor::CHATS_LIST_SCOPE], ttl)
            {
                let mut chats: Vec<ConversationSummary> = storage
                    .list_conversations()?
                    .into_iter()
                    .filter(|c| !c.kind.is_channel())
                    .collect();
                if chats_cache_satisfies_limit(&storage, chats.len(), limit) {
                    chats.truncate(limit);
                    return print_chats_output(&chats, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;

    let cached_chats: Vec<ConversationSummary> = storage
        .list_conversations()?
        .into_iter()
        .filter(|c| !c.kind.is_channel())
        .collect();

    let cache_usable = !refresh
        && (cached
            || daemon_warm
            || (ttl > 0
                && is_cursor_fresh(&storage, &["cli:chats", SyncCursor::CHATS_LIST_SCOPE], ttl)));
    let mut chats =
        if !cache_usable || !chats_cache_satisfies_limit(&storage, cached_chats.len(), limit) {
            let fetched = match graph.list_chats_with_cursor(limit).await {
                Ok(items) => items,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            storage.upsert_conversations_with_null_topics(
                &fetched.chats,
                &fetched.explicit_null_topic_keys,
            )?;
            record_chats_list_cursor(&storage, fetched.next_link, fetched.eof);
            record_cli_sync_cursor(&storage, "cli:chats");
            let stored: Vec<ConversationSummary> = storage
                .list_conversations()?
                .into_iter()
                .filter(|c| !c.kind.is_channel())
                .collect();
            if stored.is_empty() {
                fetched.chats
            } else {
                stored
            }
        } else {
            cached_chats
        };

    chats.truncate(limit);
    print_chats_output(&chats, json_out)
}

pub(super) async fn cmd_new_chat(
    cfg: &AppConfig,
    to: Vec<String>,
    topic: Option<String>,
    message: Option<String>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let members: Vec<String> = to
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        bail!("At least one recipient (--to) must be specified.");
    }

    let chat = graph.create_chat(&members, topic.as_deref()).await?;
    storage.upsert_conversations(std::slice::from_ref(&chat))?;

    let mut sent_msg: Option<MessageItem> = None;
    if let Some(md) = message {
        if !md.trim().is_empty() {
            let outgoing =
                parser::compile_outgoing_markdown(&md, None, vec![], vec![], vec![], None);
            let sent = graph.send_message(&chat.target, &outgoing).await?;
            let _ = storage.upsert_messages(std::slice::from_ref(&sent));
            sent_msg = Some(sent);
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "chat": chat,
                "message": sent_msg,
            }))?
        );
    } else {
        println!(
            "Chat created: {} ({})",
            sanitize_terminal_inline(&chat.title),
            chat.conv_key
        );
        if let Some(msg) = sent_msg {
            println!("Initial message sent: msg_id={}", msg.msg_id);
        }
    }
    Ok(())
}

pub(super) fn paged_list_limit(
    limit: Option<usize>,
    cursor: Option<&str>,
    json_out: bool,
    total: usize,
) -> usize {
    if json_out || limit.is_some() || cursor.is_some() {
        clamp_limit(limit.unwrap_or(50))
    } else {
        total.max(1)
    }
}

pub(super) async fn cmd_teams(
    cfg: &AppConfig,
    limit: Option<usize>,
    cursor: Option<&str>,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if cursor.is_some() {
        let storage = open_storage_only(cfg)?;
        let teams = storage.list_teams()?;
        let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
        let page_limit = paged_list_limit(limit, cursor, json_out, teams.len());
        let page = paginate(teams, page_limit, cursor, stamp)?;
        return print_teams_output(&page, None, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:teams"], ttl) {
                let teams = storage.list_teams()?;
                if !teams.is_empty() {
                    let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
                    let page_limit = paged_list_limit(limit, None, json_out, teams.len());
                    let page = paginate(teams, page_limit, None, stamp)?;
                    return print_teams_output(&page, None, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let force_fetch = refresh
        || (!cached
            && !daemon_warm
            && !(ttl > 0 && is_cursor_fresh(&storage, &["cli:teams"], ttl)));
    let (teams, channel_errors) = load_teams_with_refresh(&graph, &storage, force_fetch).await?;
    if let Some(ref errors) = channel_errors {
        for err in errors {
            eprintln!(
                "Warning: could not refresh channels for team \"{}\" ({}): {}. Cached channels are kept.",
                err.display_name, err.team_id, err.error
            );
        }
    }
    let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
    let page_limit = paged_list_limit(limit, None, json_out, teams.len());
    let page = paginate(teams, page_limit, None, stamp)?;
    print_teams_output(&page, channel_errors.as_deref(), json_out)
}

pub(super) async fn cmd_channels(
    cfg: &AppConfig,
    team_query: &str,
    limit: Option<usize>,
    cursor: Option<&str>,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if cursor.is_some() {
        let storage = open_storage_only(cfg)?;
        let teams = storage.list_teams()?;
        let team = resolve_team_from_slice(&teams, team_query)?;
        let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
        let page_limit = paged_list_limit(limit, cursor, json_out, team.channels.len());
        let page = paginate(team.channels.clone(), page_limit, cursor, stamp)?;
        return print_channels_output(&team, &page, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:teams"], ttl) {
                let teams = storage.list_teams()?;
                if !teams.is_empty() {
                    if let Ok(team) = resolve_team_from_slice(&teams, team_query) {
                        let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
                        let page_limit =
                            paged_list_limit(limit, None, json_out, team.channels.len());
                        let page = paginate(team.channels.clone(), page_limit, None, stamp)?;
                        return print_channels_output(&team, &page, json_out);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let force_fetch = refresh
        || (!cached
            && !daemon_warm
            && !(ttl > 0 && is_cursor_fresh(&storage, &["cli:teams"], ttl)));
    let (teams, channel_errors) = load_teams_with_refresh(&graph, &storage, force_fetch).await?;
    let mut team = resolve_team_from_slice(&teams, team_query)?;

    if let Some(err) = channel_errors
        .as_deref()
        .and_then(|errs| errs.iter().find(|e| e.team_id == team.id))
    {
        bail!(
            "Could not list channels for team \"{}\" ({}): {}",
            err.display_name,
            err.team_id,
            err.error
        );
    }

    let channels = if refresh && team.channels.is_empty() {
        let fetched = graph.list_team_channels(&team.id, &team.display_name).await?;
        if !fetched.is_empty() {
            team.channels = fetched.clone();
            let _ = storage.upsert_teams(std::slice::from_ref(&team));
        }
        fetched
    } else {
        team.channels.clone()
    };

    let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
    let page_limit = paged_list_limit(limit, None, json_out, channels.len());
    let page = paginate(channels, page_limit, None, stamp)?;
    print_channels_output(&team, &page, json_out)
}

pub(super) async fn load_teams_with_refresh(
    graph: &GraphClient,
    storage: &Storage,
    refresh: bool,
) -> Result<(Vec<TeamSummary>, Option<Vec<TeamChannelError>>)> {
    let cached = storage.list_teams()?;
    if refresh || cached.is_empty() {
        let fetched = graph.list_joined_teams_with_channels().await?;
        let merged = storage.upsert_joined_teams(&fetched)?;
        record_cli_sync_cursor(storage, "cli:teams");
        let stored = storage.list_teams()?;
        let teams = if stored.is_empty() { merged } else { stored };
        Ok((teams, Some(fetched.channel_errors)))
    } else {
        Ok((cached, None))
    }
}

pub(super) fn resolve_team_from_slice(teams: &[TeamSummary], query: &str) -> Result<TeamSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Team name or ID is required.");
    }
    if let Some(exact_id) = teams.iter().find(|t| t.id == q) {
        return Ok(exact_id.clone());
    }
    let q_lower = q.to_lowercase();
    let exact_name: Vec<&TeamSummary> = teams
        .iter()
        .filter(|t| t.display_name.to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    }
    let matches: Vec<&TeamSummary> = teams
        .iter()
        .filter(|t| t.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match matches.len() {
        0 => bail!("Team not found: {q}"),
        1 => Ok(matches[0].clone()),
        _ => {
            let names: Vec<String> = matches
                .iter()
                .map(|t| format!("{} ({})", t.display_name, t.id))
                .collect();
            bail!("Multiple teams match ({q}): {}", names.join(", "))
        }
    }
}

pub(super) async fn cmd_update_chat(
    cfg: &AppConfig,
    target: &str,
    topic: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let result = update_group_chat_topic(&storage, &graph, target, topic).await;
    let chat = match result {
        Ok(chat) => chat,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({
            "ok": true,
            "conversation": chat,
        }))?);
    } else {
        println!(
            "Group chat topic updated: {} ({})",
            sanitize_terminal_inline(&chat.title),
            chat.conv_key
        );
    }
    Ok(())
}

/// One complete Graph fetch: replaces the cached member list, then records the completion marker.
pub(crate) async fn refresh_members(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
) -> Result<()> {
    let fetched = graph.list_members(&conv.target).await?;
    storage
        .replace_members(&conv.conv_key, &fetched)
        .context("Graph returned the members, but updating the local cache failed")?;
    record_cli_sync_cursor(storage, &format!("cli:members:{}", conv.conv_key));
    Ok(())
}

/// Reads one page from the cached member list.
pub(crate) fn members_page(
    storage: &Storage,
    conv: &ConversationSummary,
    limit: usize,
    cursor: Option<&str>,
) -> Result<serde_json::Value> {
    let marker = fetch_stamp(storage, &format!("cli:members:{}", conv.conv_key));
    let members = storage.list_members(&conv.conv_key)?;
    if marker.is_none() && members.is_empty() {
        bail!(
            "Members of {} have not been fetched yet. Read the members online first.",
            conv.conv_key
        );
    }
    let stamp = marker.as_ref().map_or(0, |(stamp, _)| *stamp);
    let page = paginate(members, limit, cursor, stamp)?;
    let known = marker.is_some();
    let returned = page.items.len();
    Ok(json!({
        "conv_key": conv.conv_key,
        "title": conv.title,
        "members": page.items,
        "returned": returned,
        "has_more": if page.has_more || known { json!(page.has_more) } else { serde_json::Value::Null },
        "next_cursor": page.next_cursor,
        "total": if known { json!(page.total) } else { serde_json::Value::Null },
        "fetched_at": marker.map(|(_, at)| at),
    }))
}

pub(super) async fn cmd_members(
    cfg: &AppConfig,
    target: &str,
    limit: Option<usize>,
    cursor: Option<&str>,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let from_cache = offline || cursor.is_some();
    let (_auth, graph, storage) = open_runtime(cfg, !from_cache).await?;
    let conv = resolve_target(&storage, &graph, target, from_cache).await?;

    if !from_cache {
        refresh_members(&storage, &graph, &conv).await?;
    }

    let page_limit = if json_out {
        clamp_limit(limit.unwrap_or(50))
    } else if limit.is_none() && cursor.is_none() {
        1_000
    } else {
        clamp_limit(limit.unwrap_or(50))
    };

    let payload = members_page(&storage, &conv, page_limit, cursor)?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        let members: Vec<MemberInfo> =
            serde_json::from_value(payload["members"].clone()).unwrap_or_default();
        if members.is_empty() {
            println!("No member information available.");
        } else {
            let returned = payload["returned"].as_u64().unwrap_or(members.len() as u64);
            let header_count = payload["total"].as_u64().unwrap_or(returned);
            println!(
                "# {} ({}) Members ({})",
                sanitize_terminal_inline(&conv.title),
                conv.conv_key,
                header_count
            );
            for m in &members {
                let role = if m.roles.is_empty() {
                    String::new()
                } else {
                    let roles: Vec<String> = m
                        .roles
                        .iter()
                        .map(|r| sanitize_terminal_inline(r).into_owned())
                        .collect();
                    format!(" [{}]", roles.join(","))
                };
                let pres = m
                    .presence
                    .as_ref()
                    .map(|p| format!(" ({})", sanitize_terminal_inline(&p.availability)))
                    .unwrap_or_default();
                println!(
                    "  {}  <{}>{}{}",
                    sanitize_terminal_inline(&m.display_name),
                    sanitize_terminal_inline(m.email.as_deref().unwrap_or(&m.user_id)),
                    role,
                    pres
                );
            }
            if let Some(next) = payload["next_cursor"].as_str() {
                match payload["total"].as_u64() {
                    Some(total) => println!(
                        "(Showing {} of {} members. Next page: --cursor {next})",
                        returned, total
                    ),
                    None => println!(
                        "(Showing {} members. Next page: --cursor {next})",
                        returned
                    ),
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_users(
    cfg: &AppConfig,
    query: &str,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Search query is empty.");
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let users = match graph.search_users(q, clamp_limit(limit)).await {
        Ok(users) => users,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&users_search_json(&users))?);
    } else if users.is_empty() {
        println!("No matching users found.");
    } else {
        for u in &users {
            if let Some(addr) = u.mail.as_deref().or(u.user_principal_name.as_deref()) {
                println!(
                    "{}  <{}>  {}",
                    sanitize_terminal_inline(&u.display_name),
                    sanitize_terminal_inline(addr),
                    u.id
                );
            } else {
                println!("{}  {}", sanitize_terminal_inline(&u.display_name), u.id);
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_search(
    cfg: &AppConfig,
    query: &str,
    target: Option<&str>,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("Search query is empty.");
    }
    let storage = open_storage_only(cfg)?;
    let limit = clamp_limit(limit);

    let conv_key_filter = match target {
        Some(t) => {
            let cached = storage.list_conversations()?;
            Some(resolver::resolve_from_slice(&cached, t)?.conv_key)
        }
        None => None,
    };

    let hits = storage.search_messages(query, conv_key_filter.as_deref(), limit)?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "hits": hits,
            }))?
        );
    } else if hits.is_empty() {
        println!("No messages matching '{query}'.");
    } else {
        for hit in &hits {
            println!(
                "[{}] {} | {}: {}",
                hit.created_at,
                sanitize_terminal_inline(&hit.conv_title),
                sanitize_terminal_inline(&hit.author_name),
                sanitize_terminal_inline(&hit.snippet)
            );
        }
    }
    Ok(())
}

pub(crate) async fn fetch_presence_after_set(
    graph: &GraphClient,
    availability: &str,
    activity: &str,
) -> (
    Option<crate::models::PresenceStatus>,
    Option<crate::models::PresenceStatus>,
) {
    let latest = graph.get_my_presence().await.ok();
    let my_uid = latest
        .as_ref()
        .map(|p| p.user_id.clone())
        .filter(|id| !id.is_empty())
        .or_else(|| graph.my_user_id().map(str::to_string));
    let my_uid = match my_uid {
        Some(id) if !id.is_empty() => Some(id),
        _ => graph
            .auth()
            .current_tokens()
            .await
            .and_then(|t| t.user_id)
            .filter(|id| !id.is_empty()),
    };
    let to_cache = if availability.eq_ignore_ascii_case("Reset") {
        latest.clone().filter(|p| !p.user_id.is_empty())
    } else {
        my_uid.map(|uid| {
            latest.clone().unwrap_or(crate::models::PresenceStatus {
                user_id: uid,
                availability: availability.to_string(),
                activity: activity.to_string(),
            })
        })
    };
    (latest, to_cache)
}

pub(super) async fn cmd_presence(
    cfg: &AppConfig,
    set: Option<&str>,
    activity: Option<&str>,
    expiration_mins: Option<u32>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;

    if let Some(raw_avail) = set {
        let availability = normalize_presence_availability(raw_avail)?;
        let act = activity
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .unwrap_or(&availability);
        graph
            .set_my_presence(&availability, act, expiration_mins)
            .await?;

        let (latest, to_cache) = fetch_presence_after_set(&graph, &availability, act).await;
        if let Some(status) = to_cache {
            let _ = storage.upsert_presences(std::slice::from_ref(&status));
        }
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "availability": availability,
                    "activity": act,
                    "expiration_mins": expiration_mins,
                    "presence": latest,
                }))?
            );
        } else {
            println!("Presence updated: {availability} ({act})");
        }
    } else {
        let status = graph.get_my_presence().await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            println!(
            "Current presence: {} ({})",
            sanitize_terminal_inline(&status.availability),
            sanitize_terminal_inline(&status.activity)
        );
        }
    }
    Ok(())
}

pub(crate) async fn update_group_chat_topic(
    storage: &Storage,
    graph: &GraphClient,
    target: &str,
    topic: &str,
) -> Result<ConversationSummary> {
    let topic = topic.trim();
    if topic.is_empty() || topic.contains([':', '\0']) || topic.chars().count() > 250 {
        bail!("Group chat topic must contain 1 to 250 characters and cannot contain colons or NUL characters.");
    }
    let conv = resolve_target(storage, graph, target, false).await?;
    let ConversationTarget::Chat { chat_id } = conv.target else {
        bail!("Chat topic changes require an existing group chat.");
    };
    let updated = graph.update_group_chat_topic(&chat_id, topic).await?;
    storage.save_updated_chat_topic(&updated).context(
        "Graph returned the updated group chat topic, but updating the local cache failed",
    )
}

pub(crate) async fn resolve_target(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    offline: bool,
) -> Result<ConversationSummary> {
    if offline {
        let cached = storage.list_conversations()?;
        resolver::resolve_from_slice(&cached, query)
    } else {
        resolver::resolve_conversation(storage, graph, query).await
    }
}

pub(crate) async fn resolve_post_target(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
) -> Result<ConversationSummary> {
    resolver::resolve_post_target(storage, graph, query).await
}

pub(crate) fn normalize_presence_availability(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "available" => Ok("Available".to_string()),
        "busy" => Ok("Busy".to_string()),
        "donotdisturb" | "do_not_disturb" | "dnd" => Ok("DoNotDisturb".to_string()),
        "berightback" | "be_right_back" | "brb" => Ok("BeRightBack".to_string()),
        "away" => Ok("Away".to_string()),
        "reset" => Ok("Reset".to_string()),
        other => bail!(
            "Unsupported presence availability: '{other}' (Available|Busy|DoNotDisturb|BeRightBack|Away|Reset)"
        ),
    }
}
