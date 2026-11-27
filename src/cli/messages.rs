//! Message reading, sending, replying, files, downloads, reactions, editing, and deletion commands.

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::json;

use crate::{
    config::AppConfig,
    models::SyncCursor,
    parser, resolver,
    safe_text::sanitize_terminal_inline,
};

use super::{
    conversations::{resolve_post_target, resolve_target},
    format::{
        clamp_limit, print_read_output, print_thread_output, sent_ack_json,
        wrap_graph_permission_error, PostOutput, ReadOutput,
    },
    history::{
        build_quote_reply, download_message_attachment, edit_sent_message, ensure_mentions_used,
        normalize_reaction, parse_mentions_cli, read_conversation_messages,
        read_conversation_messages_offline, read_thread, read_thread_offline, send_file_or_inline,
        thread_cache_miss, MessageEdit,
    },
    runtime::{
        cli_cache_ttl_secs, is_cursor_fresh, open_runtime, open_storage_only, prompt,
        record_cli_sync_cursor,
    },
    DELETE_JSON_REQUIRES_YES,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_read(
    cfg: &AppConfig,
    target: &str,
    limit: usize,
    before: Option<&str>,
    thread: Option<&str>,
    offline: bool,
    cached: bool,
    output: ReadOutput,
) -> Result<()> {
    if let Some(root_msg_id) = thread.map(str::trim).filter(|s| !s.is_empty()) {
        return cmd_thread_with_cache(cfg, target, root_msg_id, limit, offline, cached, output)
            .await;
    }

    let limit = clamp_limit(limit);
    if offline {
        let storage = open_storage_only(cfg)?;
        let cached_convs = storage.list_conversations()?;
        let conv = resolver::resolve_from_slice(&cached_convs, target)?;
        let res = read_conversation_messages_offline(&storage, &conv, limit, before)?;
        return print_read_output(&conv, &res, &output);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    if cached || (before.is_none() && ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(cached_convs) = storage.list_conversations() {
                if let Ok(conv) = resolver::resolve_from_slice(&cached_convs, target) {
                    if before.is_some() {
                        let res = read_conversation_messages_offline(&storage, &conv, limit, before)?;
                        let is_eof = storage
                            .get_sync_cursor(&SyncCursor::history_scope(&conv.conv_key))
                            .ok()
                            .flatten()
                            .is_some_and(|c| c.is_history_eof());
                        if res.messages.len() >= limit || is_eof {
                            return print_read_output(&conv, &res, &output);
                        }
                    } else {
                        let cursor_key = format!("cli:read:{}", conv.conv_key);
                        if cached
                            || is_cursor_fresh(
                                &storage,
                                &[cursor_key.as_str(), conv.conv_key.as_str()],
                                ttl,
                            )
                        {
                            let res = read_conversation_messages_offline(&storage, &conv, limit, None)?;
                            let is_eof = storage
                                .get_sync_cursor(&SyncCursor::history_scope(&conv.conv_key))
                                .ok()
                                .flatten()
                                .is_some_and(|c| c.is_history_eof());
                            if res.messages.len() >= limit || is_eof {
                                return print_read_output(&conv, &res, &output);
                            }
                        }
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let res = read_conversation_messages(&storage, &graph, &conv, limit, before, false, true).await?;
    if before.is_none() {
        record_cli_sync_cursor(&storage, &format!("cli:read:{}", conv.conv_key));
    }

    print_read_output(&conv, &res, &output)
}

pub(super) async fn cmd_thread_with_cache(
    cfg: &AppConfig,
    channel_target: &str,
    root_msg_id: &str,
    limit: usize,
    offline: bool,
    cached: bool,
    output: ReadOutput,
) -> Result<()> {
    let limit = clamp_limit(limit);
    if offline {
        let storage = open_storage_only(cfg)?;
        let conv = resolver::resolve_from_slice(&storage.list_conversations()?, channel_target)?;
        let (thread, coverage) = read_thread_offline(&storage, &conv, root_msg_id, limit)?
            .ok_or_else(|| thread_cache_miss(root_msg_id, limit))?;
        return print_thread_output(&conv, &thread, coverage, &output);
    }
    if cached {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(cached_convs) = storage.list_conversations() {
                if let Ok(conv) = resolver::resolve_from_slice(&cached_convs, channel_target) {
                    if let Some((thread, coverage)) =
                        read_thread_offline(&storage, &conv, root_msg_id, limit)?
                    {
                        return print_thread_output(&conv, &thread, coverage, &output);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, channel_target, false).await?;
    let (thread, coverage) = match read_thread(&storage, &graph, &conv, root_msg_id, limit).await {
        Ok(found) => found,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    print_thread_output(&conv, &thread, coverage, &output)
}

pub(super) async fn cmd_send(
    cfg: &AppConfig,
    target: &str,
    markdown: &str,
    subject: Option<String>,
    mentions_raw: &[String],
    important: bool,
    output: PostOutput,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("Message body is empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_post_target(&storage, &graph, target).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;
    let requested_mentions = mentions.clone();

    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, subject, mentions, vec![], vec![], None);
    ensure_mentions_used(&requested_mentions, &outgoing.mentions)?;
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.send_message(&conv.target, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if output.json {
        let payload = if output.full {
            json!({
                "ok": true,
                "conversation": conv,
                "message": sent,
            })
        } else {
            sent_ack_json(&conv, &sent)
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Sent: {} ({}) msg_id={}",
            sanitize_terminal_inline(&conv.title),
            conv.conv_key,
            sent.msg_id
        );
    }
    Ok(())
}

pub(super) async fn cmd_reply(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    markdown: &str,
    mentions_raw: &[String],
    important: bool,
    output: PostOutput,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("Reply body is empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;
    let requested_mentions = mentions.clone();

    let quote = build_quote_reply(&storage, &graph, &conv, msg_id).await;
    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, None, mentions, vec![], vec![], quote);
    ensure_mentions_used(&requested_mentions, &outgoing.mentions)?;
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.reply_message(&conv.target, msg_id, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if output.json {
        let payload = if output.full {
            json!({
                "ok": true,
                "conversation": conv,
                "reply_to": msg_id,
                "message": sent,
            })
        } else {
            let mut ack = sent_ack_json(&conv, &sent);
            ack["reply_to"] = json!(msg_id);
            ack
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Replied: {} ({}) msg_id={} (to {})",
            sanitize_terminal_inline(&conv.title),
            conv.conv_key,
            sent.msg_id,
            msg_id
        );
    }
    Ok(())
}

pub(super) async fn cmd_send_file(
    cfg: &AppConfig,
    target: &str,
    path: &Path,
    caption: Option<&str>,
    thread: Option<&str>,
    inline: bool,
    output: PostOutput,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_post_target(&storage, &graph, target).await?;
    let sent = send_file_or_inline(&graph, &storage, &conv, path, caption, thread, inline).await?;

    if output.json {
        let payload = if output.full {
            json!({
                "ok": true,
                "conversation": conv,
                "path": path,
                "inline": inline,
                "message": sent,
            })
        } else {
            let mut ack = sent_ack_json(&conv, &sent);
            ack["path"] = json!(path);
            ack["inline"] = json!(inline);
            ack
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "File sent: {} ({}) msg_id={} ({})",
            sanitize_terminal_inline(&conv.title),
            conv.conv_key,
            sent.msg_id,
            path.display()
        );
    }
    Ok(())
}

pub(super) async fn cmd_download(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    index: usize,
    thread: Option<&str>,
    out: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let out_dir = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded =
        download_message_attachment(&graph, &storage, &conv, msg_id, index, thread, &out_dir)
            .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": downloaded.path,
                "file_name": downloaded.file_name,
                "mime_type": downloaded.mime_type,
                "size_bytes": downloaded.bytes_written,
                "bytes_written": downloaded.bytes_written,
                "is_image": downloaded.is_image,
            }))?
        );
    } else {
        println!(
            "Downloaded: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.bytes_written
        );
    }
    Ok(())
}

pub(super) async fn cmd_react(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    reaction: &str,
    enable: bool,
    thread: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let normalized = normalize_reaction(reaction)?;
    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    graph
        .set_reaction(&conv.target, msg_id, thread, &normalized, enable)
        .await?;
    let my_name = auth
        .current_tokens()
        .await
        .and_then(|t| t.user_display_name);
    let _ = storage.apply_local_reaction(
        &conv.conv_key,
        msg_id,
        &normalized,
        enable,
        my_name.as_deref(),
    );

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
                "reaction": normalized,
                "enable": enable,
            }))?
        );
    } else {
        println!(
            "Reaction {}: {} ({}) msg_id={}",
            if enable { "set" } else { "removed" },
            normalized,
            conv.conv_key,
            msg_id
        );
    }
    Ok(())
}

pub(super) async fn cmd_edit(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    edit: MessageEdit<'_>,
    thread: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    if let Err(err) = edit_sent_message(&storage, &graph, &conv, msg_id, thread, edit).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))?
        );
    } else {
        println!("Message edited: {} msg_id={}", conv.conv_key, msg_id);
    }
    Ok(())
}

pub(super) async fn cmd_delete(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    thread: Option<&str>,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;

    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete message {msg_id} in '{}' ({})? Type 'yes' to continue: ",
            sanitize_terminal_inline(&conv.title),
            conv.conv_key
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_message(&conv.target, msg_id, thread).await?;
    let _ = storage.apply_local_soft_delete(&conv.conv_key, msg_id, &Utc::now().to_rfc3339());

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))?
        );
    } else {
        println!("Message deleted: {} msg_id={}", conv.conv_key, msg_id);
    }
    Ok(())
}

pub(super) fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("No confirmation input received. Message was not deleted.")?;
    if !answer.trim().eq_ignore_ascii_case("yes") {
        bail!("Cancelled. Message was not deleted.");
    }
    Ok(())
}
