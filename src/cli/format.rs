//! Human-readable terminal formatting, compact JSON shaping, pagination, and error envelopes.

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    auth::explain_graph_permission_error,
    config::Persona,
    graph::GraphClient,
    models::{
        ConversationKind, ConversationSummary, MessageItem, TeamChannelError, TeamSummary,
        ThreadCoverage, ThreadView, UserProfile,
    },
    parser, resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    storage::Storage,
};

use super::history::ReadMessagesResult;

pub(super) fn print_chats_output(chats: &[ConversationSummary], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "chats": chats }))?
        );
    } else if chats.is_empty() {
        println!("No chats to display.");
    } else {
        for c in chats {
            let unread = if c.unread_count > 0 {
                format!(" [unread:{}]", c.unread_count)
            } else {
                String::new()
            };
            let preview = c.last_msg_preview.as_deref().unwrap_or("");
            println!(
                "{}  [{}] {}{}  {}",
                c.conv_key,
                c.kind.as_str(),
                sanitize_terminal_inline(&c.title),
                unread,
                truncate_single_line(preview, 60)
            );
        }
    }
    Ok(())
}

pub(super) fn print_teams_output(
    page: &Page<TeamSummary>,
    channel_errors: Option<&[TeamChannelError]>,
    json_out: bool,
) -> Result<()> {
    if json_out {
        let mut obj = json!({
            "teams": page.items,
            "returned": page.items.len(),
            "has_more": page.has_more,
            "next_cursor": page.next_cursor,
            "total": page.total,
        });
        if let Some(errors) = channel_errors {
            obj["channel_errors"] = json!(errors);
        }
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else if page.items.is_empty() {
        println!("No joined teams found.");
    } else {
        for team in &page.items {
            println!("{} ({})", sanitize_terminal_inline(&team.display_name), team.id);
            for ch in &team.channels {
                println!(
                    "  - {} [{}] ({})",
                    sanitize_terminal_inline(&ch.display_name),
                    sanitize_terminal_inline(&ch.membership_type),
                    ch.conv_key()
                );
            }
        }
        if let Some(ref next) = page.next_cursor {
            println!(
                "(Showing {} of {} teams. Next page: --cursor {next})",
                page.items.len(),
                page.total
            );
        }
    }
    Ok(())
}

pub(super) fn print_channels_output(
    team: &TeamSummary,
    page: &Page<crate::models::ChannelSummary>,
    json_out: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "team": { "id": team.id, "display_name": team.display_name },
                "channels": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
                "total": page.total,
            }))?
        );
    } else {
        println!("# {} ({})", sanitize_terminal_inline(&team.display_name), team.id);
        for ch in &page.items {
            println!(
                "  {}  [{}]  {}",
                ch.conv_key(),
                sanitize_terminal_inline(&ch.membership_type),
                sanitize_terminal_inline(&ch.display_name)
            );
        }
        if let Some(ref next) = page.next_cursor {
            println!(
                "(Showing {} of {} channels. Next page: --cursor {next})",
                page.items.len(),
                page.total
            );
        }
    }
    Ok(())
}

pub(super) struct ReadOutput {
    pub(super) json: bool,
    pub(super) compact: bool,
    /// Arguments to repeat in the --full hint, e.g. " --limit 30 --before c-msg-3 --offline".
    pub(super) full_args: String,
}

pub(super) fn format_full_args(limit: usize, before: Option<&str>, offline: bool) -> String {
    let mut out = format!(" --limit {}", clamp_limit(limit));
    if let Some(b) = before.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!(" --before {b}"));
    }
    if offline {
        out.push_str(" --offline");
    }
    out
}

pub(super) fn print_read_output(
    conv: &ConversationSummary,
    res: &ReadMessagesResult,
    output: &ReadOutput,
) -> Result<()> {
    if output.json {
        let hint = output.compact.then(|| {
            format!(
                "{} read {} --json --full{}",
                Persona::detect().bin_name(),
                conv.conv_key,
                output.full_args
            )
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&read_messages_json(conv, res, hint.as_deref()))?
        );
    } else {
        println!("# {} ({})", sanitize_terminal_inline(&conv.title), conv.conv_key);
        if res.messages.is_empty() {
            println!("(No messages)");
        } else {
            for msg in &res.messages {
                print_message_line(msg, "");
            }
            if res.has_more {
                if let Some(ref next_before) = res.next_before {
                    println!(
                        "--- Load older messages: teams read \"{}\" --before {} ---",
                        conv.conv_key, next_before
                    );
                }
            }
        }
    }
    Ok(())
}

pub(super) fn print_thread_output(
    conv: &ConversationSummary,
    thread: &ThreadView,
    coverage: ThreadCoverage,
    output: &ReadOutput,
) -> Result<()> {
    if output.json {
        let hint = output.compact.then(|| {
            format!(
                "{} thread {} {} --json --full{}",
                Persona::detect().bin_name(),
                conv.conv_key,
                thread.root.msg_id,
                output.full_args
            )
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&thread_json(conv, thread, coverage, hint.as_deref()))?
        );
    } else {
        println!(
            "# {} ({}) — Thread {}",
            sanitize_terminal_inline(&conv.title),
            conv.conv_key,
            thread.root.msg_id
        );
        let mut display_root = thread.root.clone();
        display_root.reply_count = coverage.reply_total.unwrap_or(0);
        print_message_line(&display_root, "");
        if thread.replies.is_empty() {
            println!("  (No replies)");
        } else {
            for reply in &thread.replies {
                print_message_line(reply, "  ↳ ");
            }
        }
        if coverage.has_more {
            match coverage.reply_total {
                Some(total) => println!(
                    "  (Showing the newest {} of {} replies. Raise --limit to read more.)",
                    thread.replies.len(),
                    total
                ),
                None => println!(
                    "  (Showing the newest {} replies; more exist. Raise --limit, up to 1000, to read more.)",
                    thread.replies.len()
                ),
            }
        }
    }
    Ok(())
}

pub(super) fn format_message_line(msg: &MessageItem, indent: &str) -> String {
    let author = sanitize_terminal_inline(&msg.author_name);
    if msg.deleted {
        let tag_str = if msg.reply_count > 0 {
            format!(" [💬 {}]", msg.reply_count)
        } else {
            String::new()
        };
        return format!(
            "{indent}[{}] {} ({}){tag_str}: (deleted message)",
            msg.created_at, author, msg.msg_id
        );
    }

    let attachments = msg.effective_attachments();
    let mut tags = Vec::new();
    if let Some(ref subj) = msg.subject {
        if !subj.trim().is_empty() {
            tags.push(format!("[Subject: {}]", sanitize_terminal_inline(subj.trim())));
        }
    }
    if msg.importance.eq_ignore_ascii_case("high") || msg.importance.eq_ignore_ascii_case("urgent") {
        tags.push(format!("[Importance:{}]", sanitize_terminal_inline(&msg.importance)));
    }
    if msg.reply_count > 0 {
        tags.push(format!("[💬 {}]", msg.reply_count));
    }
    if !attachments.is_empty() {
        let names: Vec<String> = attachments
            .iter()
            .filter(|a| !a.is_message_reference())
            .map(|a| {
                let raw = a.name.as_deref().unwrap_or(a.content_type.as_str());
                sanitize_terminal_inline(raw).into_owned()
            })
            .collect();
        if !names.is_empty() {
            tags.push(format!("[📎 {}]", names.join(", ")));
        }
    }
    if !msg.reactions.is_empty() {
        let reacts: Vec<String> = msg
            .reactions
            .iter()
            .map(|r| {
                format!(
                    "{} {}",
                    sanitize_terminal_inline(reaction_symbol(&r.reaction_type)),
                    r.count
                )
            })
            .collect();
        tags.push(format!("[{}]", reacts.join(" ")));
    }

    let raw_body = if msg.deleted {
        "(deleted message)".to_string()
    } else if !msg.body_plain.trim().is_empty() {
        msg.body_plain.trim().to_string()
    } else {
        parser::html_and_cards_to_plain(&msg.body_html, &attachments)
            .trim()
            .to_string()
    };
    let body = sanitize_terminal_text(&raw_body);

    let quote = match (&msg.reply_to_author, &msg.reply_to_preview) {
        (Some(quote_author), Some(prev)) => format!(
            " ↩ {}: {}",
            sanitize_terminal_inline(quote_author),
            truncate_single_line(prev, 40)
        ),
        _ => String::new(),
    };

    let tag_str = if tags.is_empty() {
        String::new()
    } else {
        format!(" {}", tags.join(" "))
    };

    format!(
        "{indent}[{}] {} ({}){quote}{tag_str}: {}",
        msg.created_at, author, msg.msg_id, body
    )
}

pub(super) fn print_message_line(msg: &MessageItem, indent: &str) {
    println!("{}", format_message_line(msg, indent));
}

pub(super) fn reaction_symbol(reaction_type: &str) -> &str {
    match reaction_type {
        "like" => "👍",
        "heart" => "❤️",
        "laugh" => "😄",
        "surprised" => "😮",
        "sad" => "😢",
        "angry" => "😡",
        other => other,
    }
}

pub(super) struct PostOutput {
    pub(super) json: bool,
    pub(super) full: bool,
}


pub(crate) fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 1_000)
}

pub(crate) struct Page<T> {
    pub items: Vec<T>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub total: usize,
}

/// Offset paging over a fully loaded list. A cursor is "<offset>-<stamp>"; the stamp ties it
/// to one completed fetch of one resource, so a cursor from another list or an older fetch is rejected.
pub(crate) fn paginate<T>(
    items: Vec<T>,
    limit: usize,
    cursor: Option<&str>,
    stamp: i64,
) -> Result<Page<T>> {
    let total = items.len();
    let offset = match cursor {
        None => 0,
        Some(raw) => {
            let (offset, cursor_stamp) = raw
                .trim()
                .split_once('-')
                .and_then(|(o, s)| Some((o.parse::<usize>().ok()?, s.parse::<i64>().ok()?)))
                .filter(|(offset, _)| *offset <= total)
                .with_context(|| {
                    format!(
                        "Invalid cursor: {raw}. Pass the next_cursor value from the previous page."
                    )
                })?;
            if cursor_stamp != stamp {
                bail!(
                    "Stale cursor: {raw}. The list was refreshed or belongs to another target; start again without a cursor."
                );
            }
            offset
        }
    };
    let end = offset.saturating_add(limit).min(total);
    let has_more = end < total;
    Ok(Page {
        items: items.into_iter().skip(offset).take(limit).collect(),
        has_more,
        next_cursor: has_more.then(|| format!("{end}-{stamp}")),
        total,
    })
}

/// Milliseconds of the completed-fetch marker for a scope; None when no complete fetch was recorded.
pub(crate) fn fetch_stamp(storage: &Storage, scope_key: &str) -> Option<(i64, String)> {
    let cursor = storage.get_sync_cursor(scope_key).ok().flatten()?;
    let at = chrono::DateTime::parse_from_rfc3339(&cursor.synced_at).ok()?;
    Some((at.timestamp_millis(), cursor.synced_at))
}

pub(super) const COMPACT_OMITTED_FIELDS: &[&str] = &[
    "msg_key",
    "conv_key",
    "reply_to_author",
    "reply_to_preview",
    "is_me",
    "body_html",
    "web_url",
    "attachments.content_url",
    "attachments.content_json",
    "attachments.thumbnail_url",
    "conversation.target",
    "conversation.kind",
    "conversation.team_id",
    "conversation.team_name",
    "conversation.topic",
    "conversation.unread_count",
    "conversation.last_read_at",
    "conversation.last_msg_id",
    "conversation.last_msg_author",
    "conversation.last_msg_preview",
    "conversation.last_updated_at",
    "conversation.pinned",
    "conversation.peer_user_id",
    "conversation.web_url",
];

pub(super) fn compact_attachment(att: &crate::models::AttachmentInfo) -> serde_json::Value {
    let mut stub = json!({ "id": att.id, "name": att.name });
    let content_type = att.content_type.trim();
    if !content_type.is_empty() {
        stub["content_type"] = json!(content_type);
    }
    stub
}

pub(super) fn compact_conversation(conv: &ConversationSummary) -> serde_json::Value {
    json!({ "conv_key": conv.conv_key, "title": conv.title })
}

pub(super) fn compact_message(msg: &MessageItem) -> serde_json::Value {
    let mut value = json!({
        "msg_id": msg.msg_id,
        "author_id": msg.author_id,
        "author_name": msg.author_name,
        "created_at": msg.created_at,
        "body_plain": msg.body_plain,
    });
    for (key, value_if_present) in [
        ("last_modified_at", &msg.last_modified_at),
        ("parent_msg_id", &msg.parent_msg_id),
        ("reply_to_id", &msg.reply_to_id),
    ] {
        if let Some(text) = value_if_present {
            value[key] = json!(text);
        }
    }
    if let Some(subj) = msg.subject.as_deref().filter(|s| !s.trim().is_empty()) {
        value["subject"] = json!(subj);
    }
    if msg.deleted {
        value["deleted"] = json!(true);
    }
    if msg.message_type != "message" {
        value["message_type"] = json!(msg.message_type);
    }
    if !msg.importance.eq_ignore_ascii_case("normal") {
        value["importance"] = json!(msg.importance);
    }
    if msg.reply_count > 0 {
        value["reply_count"] = json!(msg.reply_count);
    }
    let attachments: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .filter(|att| !att.is_message_reference())
        .map(compact_attachment)
        .collect();
    if !attachments.is_empty() {
        value["attachments"] = serde_json::Value::Array(attachments);
    }
    let links: Vec<serde_json::Value> =
        crate::ui::overlays::links::extract_links_from_text(&msg.body_html, "")
            .into_iter()
            .filter(|link| !msg.body_plain.contains(&link.url))
            .map(|link| json!({ "text": link.label, "url": link.url }))
            .collect();
    if !links.is_empty() {
        value["links"] = serde_json::Value::Array(links);
    }
    if !msg.mentions.is_empty() || !msg.reactions.is_empty() {
        value["omitted"] = json!({
            "mentions": msg.mentions.len(),
            "reactions": msg.reactions.len(),
        });
    }
    value
}

pub(crate) fn sent_ack_json(
    conv: &ConversationSummary,
    msg: &MessageItem,
) -> serde_json::Value {
    let mut conv_val = json!({
        "conv_key": conv.conv_key,
        "title": conv.title,
    });
    if conv.kind == ConversationKind::Channel {
        if let Some(team) = conv.team_name.as_deref().filter(|t| !t.is_empty()) {
            conv_val["team_name"] = json!(team);
        }
    }
    let mut msg_val = json!({
        "msg_id": msg.msg_id,
        "created_at": msg.created_at,
    });
    if !msg.mentions.is_empty() {
        msg_val["mentions"] = json!(msg.mentions.len());
    }
    let attachments: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .filter(|att| !att.is_message_reference())
        .map(compact_attachment)
        .collect();
    if !attachments.is_empty() {
        msg_val["attachments"] = serde_json::Value::Array(attachments);
    }
    json!({
        "ok": true,
        "conversation": conv_val,
        "message": msg_val,
    })
}

pub(crate) fn users_search_json(users: &[UserProfile]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            json!({
                "user_id": u.id,
                "display_name": u.display_name,
                "email": u.mail,
                "user_principal_name": u.user_principal_name,
            })
        })
        .collect();
    let returned = items.len();
    json!({
        "users": items,
        "returned": returned,
    })
}

pub(crate) fn finish_thread_value(
    mut thread: serde_json::Value,
    returned: usize,
    coverage: ThreadCoverage,
) -> serde_json::Value {
    thread["root"]["reply_count"] = json!(coverage.reply_total);
    thread["returned"] = json!(returned);
    thread["has_more"] = json!(coverage.has_more);
    thread
}

pub(crate) fn read_messages_json(
    conv: &ConversationSummary,
    res: &ReadMessagesResult,
    hint: Option<&str>,
) -> serde_json::Value {
    if let Some(hint) = hint {
        let omitted_mentions: usize = res.messages.iter().map(|m| m.mentions.len()).sum();
        let omitted_reactions: usize = res.messages.iter().map(|m| m.reactions.len()).sum();
        json!({
            "conversation": compact_conversation(conv),
            "messages": serde_json::Value::Array(res.messages.iter().map(compact_message).collect()),
            "has_more": res.has_more,
            "next_before": res.next_before,
            "omitted": {
                "mentions": omitted_mentions,
                "reactions": omitted_reactions,
                "fields": COMPACT_OMITTED_FIELDS,
            },
            "hint": hint,
        })
    } else {
        json!({
            "conversation": conv,
            "messages": res.messages,
            "has_more": res.has_more,
            "next_before": res.next_before,
        })
    }
}

pub(crate) fn thread_json(
    conv: &ConversationSummary,
    thread: &ThreadView,
    coverage: ThreadCoverage,
    hint: Option<&str>,
) -> serde_json::Value {
    if let Some(hint) = hint {
        let omitted_mentions: usize = std::iter::once(&thread.root)
            .chain(&thread.replies)
            .map(|m| m.mentions.len())
            .sum();
        let omitted_reactions: usize = std::iter::once(&thread.root)
            .chain(&thread.replies)
            .map(|m| m.reactions.len())
            .sum();
        let replies: Vec<_> = thread.replies.iter().map(compact_message).collect();
        let thread_val = finish_thread_value(
            json!({ "root": compact_message(&thread.root), "replies": replies }),
            thread.replies.len(),
            coverage,
        );
        json!({
            "conversation": compact_conversation(conv),
            "thread": thread_val,
            "omitted": {
                "mentions": omitted_mentions,
                "reactions": omitted_reactions,
                "fields": COMPACT_OMITTED_FIELDS,
            },
            "hint": hint,
        })
    } else {
        let thread_val = finish_thread_value(json!(thread), thread.replies.len(), coverage);
        json!({ "conversation": conv, "thread": thread_val })
    }
}

#[derive(Debug)]
pub(super) struct DetailedError {
    pub(super) message: String,
    pub(super) details: serde_json::Value,
}

impl std::fmt::Display for DetailedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DetailedError {}

fn is_forbidden_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    // The parsed response status, not a "403" substring: URLs and message ids contain digits.
    extract_graph_http_status(msg) == Some(403)
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
        || lower.contains("missing scope")
        || lower.contains("aclcheckfailed")
        || lower.contains("insufficient graph permissions")
        || lower.contains("insufficient permissions")
}

pub(crate) fn is_forbidden_error(err: &anyhow::Error) -> bool {
    is_forbidden_message(&format!("{err:#}"))
}

pub(crate) use crate::graph::extract_graph_http_status;

pub(crate) fn error_code_for_message(msg: &str) -> &'static str {
    if msg.contains(crate::http::TLS_UNTRUSTED_MARKER) {
        "tls_untrusted_certificate"
    } else if msg.contains("Conversation/channel not found") {
        "target_not_found"
    } else if msg.contains("Sign-in required") {
        "not_signed_in"
    } else if msg.contains("Token expired") {
        "token_expired"
    } else if is_forbidden_message(msg) {
        "permission_denied"
    } else {
        match extract_graph_http_status(msg) {
            Some(401) => "unauthorized",
            Some(404) => "not_found",
            _ => "error",
        }
    }
}

pub(crate) fn error_json_from_message(msg: &str) -> serde_json::Value {
    let code = error_code_for_message(msg);
    let http_status = if code == "permission_denied" {
        Some(403)
    } else {
        extract_graph_http_status(msg)
    };
    let mut err_obj = json!({
        "code": code,
        "message": msg,
    });
    if let Some(status) = http_status {
        err_obj["http_status"] = json!(status);
    }
    json!({
        "ok": false,
        "error": err_obj,
    })
}

pub(crate) fn error_json(err: &anyhow::Error) -> serde_json::Value {
    let message = format!("{err:#}");
    let mut err_obj = if let Some(amb) = err.downcast_ref::<resolver::AmbiguousTarget>() {
        json!({
            "code": "target_ambiguous",
            "message": message,
            "candidates": amb.candidates,
        })
    } else {
        let code = error_code_for_message(&message);
        let http_status = if code == "permission_denied" {
            Some(403)
        } else {
            extract_graph_http_status(&message)
        };
        let mut obj = json!({
            "code": code,
            "message": message,
        });
        if let Some(status) = http_status {
            obj["http_status"] = json!(status);
        }
        if let Some(unv) = err.downcast_ref::<resolver::UnverifiedTarget>() {
            obj["candidates"] = json!(unv.candidates);
        }
        if let Some(det) = err.downcast_ref::<DetailedError>() {
            obj["details"] = det.details.clone();
        }
        obj
    };
    if let Some(det) = err.downcast_ref::<DetailedError>() {
        if err_obj.get("details").is_none() {
            err_obj["details"] = det.details.clone();
        }
    }
    json!({
        "ok": false,
        "error": err_obj,
    })
}

pub(crate) async fn wrap_graph_permission_error(
    graph: &GraphClient,
    err: anyhow::Error,
) -> anyhow::Error {
    let raw = format!("{err:#}");
    let token = graph.auth().current_tokens().await.map(|t| t.access_token);
    if let Some(explained) = explain_graph_permission_error(&raw, token.as_deref()) {
        anyhow::anyhow!("{explained}\nRaw error: {raw}")
    } else {
        err
    }
}

pub(super) fn format_unix_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

pub(super) fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let sanitized = sanitize_terminal_inline(input);
    let trimmed = sanitized.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}
