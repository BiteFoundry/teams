//! Offline history synchronization (`sync`) and conversation export (`export`) commands.

use std::{collections::HashMap, path::Path};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use futures_util::stream::{self, StreamExt};
use serde_json::json;

use crate::{
    config::{self, AppConfig},
    graph::GraphClient,
    models::{ConversationSummary, ConversationTarget, MessageItem, SyncCursor},
    parser,
    safe_text::sanitize_terminal_inline,
    storage::Storage,
};

use super::{
    conversations::resolve_target,
    format::{clamp_limit, wrap_graph_permission_error},
    history::hydrate_messages_attachments,
    runtime::{open_runtime, record_chats_list_cursor},
};

pub(super) async fn sync_conversation_depth(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    depth: usize,
) -> Result<usize> {
    let first_page_top = depth.min(50);
    let (latest_msgs, first_next_link) = match graph
        .list_messages_page(&conv.target, first_page_top, None)
        .await
    {
        Ok(res) => res,
        Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
    };
    let mut synced_count = if latest_msgs.is_empty() {
        0
    } else {
        storage.upsert_messages(&latest_msgs).unwrap_or(latest_msgs.len())
    };
    let mut fetched_roots = latest_msgs
        .iter()
        .filter(|m| m.parent_msg_id.is_none())
        .count();

    let scope_key = SyncCursor::history_scope(&conv.conv_key);
    let mut current_next_link = first_next_link;
    let mut oldest_seen_ts = latest_msgs
        .iter()
        .filter(|m| m.parent_msg_id.is_none())
        .map(|m| m.created_at.clone())
        .min();

    while fetched_roots < depth {
        let Some(link) = current_next_link.clone() else {
            break;
        };
        let page_top = (depth - fetched_roots).min(50);
        let (page_msgs, new_next_link) = match graph
            .list_messages_page(&conv.target, page_top, Some(&link))
            .await
        {
            Ok(res) => res,
            Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
        };
        if !page_msgs.is_empty() {
            synced_count += storage.upsert_messages(&page_msgs).unwrap_or(page_msgs.len());
            fetched_roots += page_msgs
                .iter()
                .filter(|m| m.parent_msg_id.is_none())
                .count();
            if let Some(page_oldest) = page_msgs
                .iter()
                .filter(|m| m.parent_msg_id.is_none())
                .map(|m| m.created_at.clone())
                .min()
            {
                oldest_seen_ts = match oldest_seen_ts {
                    Some(prev) if prev < page_oldest => Some(prev),
                    _ => Some(page_oldest),
                };
            }
        }
        let no_progress = page_msgs.is_empty() && new_next_link.as_deref() == Some(link.as_str());
        current_next_link = new_next_link;
        if current_next_link.is_none() || no_progress {
            break;
        }
    }

    let oldest_ts = if current_next_link.is_none() {
        Some(SyncCursor::HISTORY_EOF.to_string())
    } else {
        oldest_seen_ts
    };
    let _ = storage.save_sync_cursor(&SyncCursor {
        scope_key,
        delta_link: current_next_link,
        oldest_msg_ts: oldest_ts,
        synced_at: Utc::now().to_rfc3339(),
    });

    let local_count = storage
        .list_root_messages(&conv.conv_key, depth, None)
        .map(|v| v.len())
        .unwrap_or(0);

    Ok(synced_count.max(local_count))
}

pub(super) async fn cmd_sync(
    cfg: &AppConfig,
    target: Option<&str>,
    chats: usize,
    depth: usize,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let chats_limit = clamp_limit(chats);
    let depth = clamp_limit(depth);

    let mut errors = Vec::new();
    let targets: Vec<ConversationSummary> = match target {
        Some(t) => vec![resolve_target(&storage, &graph, t, false).await?],
        None => {
            let (chats_res, teams_res) = tokio::join!(
                graph.list_chats_with_cursor(chats_limit),
                graph.list_joined_teams_with_channels(),
            );
            let fetched_chats = match chats_res {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            storage.upsert_conversations_with_null_topics(
                &fetched_chats.chats,
                &fetched_chats.explicit_null_topic_keys,
            )?;
            record_chats_list_cursor(&storage, fetched_chats.next_link, fetched_chats.eof);
            match teams_res {
                Ok(fetched) => {
                    let _ = storage.upsert_joined_teams(&fetched)?;
                }
                Err(err) => {
                    let wrapped = wrap_graph_permission_error(&graph, err).await;
                    errors.push(format!("teams/channels: {wrapped:#}"));
                }
            }
            let all_convs = storage.list_conversations()?;
            let mut chat_count = 0usize;
            let mut selected = Vec::new();
            for conv in all_convs {
                if conv.kind.is_channel() {
                    selected.push(conv);
                } else if chat_count < chats_limit {
                    chat_count += 1;
                    selected.push(conv);
                }
            }
            selected
        }
    };

    if !json_out && target.is_some() {
        for conv in &targets {
            println!(
                "Syncing: {} ({}, target depth {})...",
                sanitize_terminal_inline(&conv.title),
                conv.conv_key,
                depth
            );
        }
    }

    let mut total_messages = 0usize;
    let mut synced_convs = 0usize;

    let results: Vec<(String, Result<usize>)> = stream::iter(targets.iter())
        .map(|conv| {
            let storage = &storage;
            let graph = &graph;
            async move {
                let res = sync_conversation_depth(storage, graph, conv, depth).await;
                (conv.conv_key.clone(), res)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;

    for (conv_key, res) in results {
        match res {
            Ok(count) => {
                total_messages += count;
                synced_convs += 1;
            }
            Err(err) => {
                errors.push(format!("{conv_key}: {err:#}"));
            }
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": errors.is_empty(),
                "chats_limit": chats_limit,
                "depth": depth,
                "conversations_total": targets.len(),
                "conversations_synced": synced_convs,
                "messages_synced": total_messages,
                "errors": errors,
            }))?
        );
    } else {
        println!(
            "Sync complete: {synced_convs}/{} conversations · {total_messages} messages (chats={}, depth={})",
            targets.len(),
            chats_limit,
            depth
        );
        for err in &errors {
            eprintln!("Warning: {err}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_export(
    cfg: &AppConfig,
    target: &str,
    format: &str,
    include_threads: bool,
    limit: usize,
    offline: bool,
    out: Option<&Path>,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let conv = resolve_target(&storage, &graph, target, offline).await?;

    let fetched = if !offline {
        let msgs = graph.list_messages(&conv.target, limit).await?;
        storage.upsert_messages(&msgs)?;
        Some(msgs)
    } else {
        None
    };

    let mut messages = storage.list_root_messages(&conv.conv_key, limit, None)?;
    if messages.is_empty() {
        if let Some(f) = fetched {
            messages = f;
        }
    }

    let mut threads_by_root: HashMap<String, Vec<MessageItem>> = HashMap::new();
    // Roots whose replies could not be fetched: (root_msg_id, error). Their cached replies
    // are exported instead, and the command reports the export as incomplete.
    let mut incomplete_threads: Vec<(String, String)> = Vec::new();
    let mut thread_roots_total = 0usize;
    if include_threads {
        if let ConversationTarget::Channel {
            ref team_id,
            ref channel_id,
        } = conv.target
        {
            let root_ids: Vec<&str> = messages
                .iter()
                .filter(|m| m.reply_count > 0)
                .map(|m| m.msg_id.as_str())
                .collect();
            thread_roots_total = root_ids.len();

            if offline {
                for msg_id in root_ids {
                    let replies = storage.list_thread_replies(&conv.conv_key, msg_id, limit)?;
                    if !replies.is_empty() {
                        threads_by_root.insert(msg_id.to_string(), replies);
                    }
                }
            } else {
                let fetched: Vec<(String, Result<Vec<MessageItem>>)> = stream::iter(root_ids)
                    .map(|msg_id| {
                        let graph = &graph;
                        let storage = &storage;
                        async move {
                            let r = graph
                                .list_thread_replies(team_id, channel_id, msg_id, limit)
                                .await;
                            if let Ok(replies) = &r {
                                if !replies.is_empty() {
                                    let _ = storage.upsert_messages(replies);
                                }
                            }
                            (msg_id.to_string(), r)
                        }
                    })
                    .buffer_unordered(6)
                    .collect()
                    .await;

                for (msg_id, result) in fetched {
                    let replies = match result {
                        Ok(replies) => replies,
                        Err(err) => {
                            incomplete_threads.push((msg_id.clone(), format!("{err:#}")));
                            storage
                                .list_thread_replies(&conv.conv_key, &msg_id, limit)
                                .unwrap_or_default()
                        }
                    };
                    if !replies.is_empty() {
                        threads_by_root.insert(msg_id, replies);
                    }
                }
                // Deterministic report order regardless of fetch completion order.
                incomplete_threads.sort();
            }
        }
    }

    hydrate_messages_attachments(&mut messages);
    for replies in threads_by_root.values_mut() {
        hydrate_messages_attachments(replies);
    }

    let body = match format {
        "json" => {
            if include_threads {
                let entries: Vec<serde_json::Value> = messages
                    .iter()
                    .map(|m| {
                        let replies = threads_by_root
                            .get(&m.msg_id)
                            .cloned()
                            .unwrap_or_default();
                        json!({
                            "message": m,
                            "replies": replies,
                        })
                    })
                    .collect();
                let mut doc = json!({
                    "conversation": conv,
                    "items": entries,
                });
                // Present only for a partial export, so a clean one is byte-identical.
                if !incomplete_threads.is_empty() {
                    doc["incomplete_threads"] = incomplete_threads
                        .iter()
                        .map(|(root_msg_id, error)| {
                            json!({ "root_msg_id": root_msg_id, "error": error })
                        })
                        .collect();
                }
                serde_json::to_string_pretty(&doc)?
            } else {
                serde_json::to_string_pretty(&json!({
                    "conversation": conv,
                    "messages": messages,
                }))?
            }
        }
        "txt" => {
            let mut out_str = format!("# {} ({})\n\n", conv.title, conv.conv_key);
            for m in &messages {
                let plain = if m.deleted {
                    "(deleted message)".to_string()
                } else if !m.body_plain.trim().is_empty() {
                    m.body_plain.trim().to_string()
                } else {
                    parser::html_and_cards_to_plain(&m.body_html, &m.attachments)
                };
                out_str.push_str(&format!("[{}] {}: {}\n", m.created_at, m.author_name, plain));
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rplain = if r.deleted {
                            "(deleted message)".to_string()
                        } else if !r.body_plain.trim().is_empty() {
                            r.body_plain.trim().to_string()
                        } else {
                            parser::html_and_cards_to_plain(&r.body_html, &r.attachments)
                        };
                        out_str.push_str(&format!(
                            "  ↳ [{}] {}: {}\n",
                            r.created_at, r.author_name, rplain
                        ));
                    }
                }
            }
            out_str
        }
        _ => {
            let mut out_str = format!("# {}\n\n", conv.title);
            for m in &messages {
                if m.deleted {
                    out_str.push_str(&format!(
                        "- **{}** `{}`\n\n  (deleted message)\n\n",
                        m.author_name, m.created_at
                    ));
                } else {
                    if let Some(ref subj) = m.subject {
                        if !subj.trim().is_empty() {
                            out_str.push_str(&format!("## {}\n\n", subj.trim()));
                        }
                    }
                    let md = parser::html_and_cards_to_markdown(&m.body_html, &m.attachments);
                    let text = if md.trim().is_empty() {
                        m.body_plain.trim().to_string()
                    } else {
                        md.trim().to_string()
                    };
                    out_str.push_str(&format!(
                        "- **{}** `{}`\n\n  {}\n\n",
                        m.author_name,
                        m.created_at,
                        text.replace('\n', "\n  ")
                    ));
                }
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rtext = if r.deleted {
                            "(deleted message)".to_string()
                        } else {
                            let rmd = parser::html_and_cards_to_markdown(&r.body_html, &r.attachments);
                            if rmd.trim().is_empty() {
                                r.body_plain.trim().to_string()
                            } else {
                                rmd.trim().to_string()
                            }
                        };
                        out_str.push_str(&format!(
                            "  - ↳ **{}** `{}`: {}\n",
                            r.author_name, r.created_at, rtext
                        ));
                    }
                    out_str.push('\n');
                }
            }
            out_str
        }
    };

    match out {
        Some(path) => {
            config::ensure_parent(path)?;
            std::fs::write(path, body)
                .with_context(|| format!("Failed to write export file: {}", path.display()))?;
            println!("Exported: {}", path.display());
        }
        None => print!("{body}"),
    }

    // The (partial) export is already written; now make the gap impossible to miss.
    if !incomplete_threads.is_empty() {
        for (root_msg_id, error) in &incomplete_threads {
            eprintln!("warning: thread {root_msg_id}: replies could not be fetched: {error}");
        }
        bail!(
            "export incomplete: {} of {} threads could not be fetched",
            incomplete_threads.len(),
            thread_roots_total
        );
    }
    Ok(())
}
