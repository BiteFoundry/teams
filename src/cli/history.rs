//! Shared message history, thread loading, editing, mentions, and attachment domain helpers.

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};

use crate::{
    graph::GraphClient,
    models::{
        ConversationKind, ConversationSummary, ConversationTarget, DownloadedFile,
        InlineImageUpload, MentionInfo, MessageItem, QuotedReply, SyncCursor, ThreadCoverage,
        ThreadView,
    },
    parser,
    storage::Storage,
};

use super::format::{clamp_limit, truncate_single_line, wrap_graph_permission_error};

pub(crate) struct ReadMessagesResult {
    pub messages: Vec<MessageItem>,
    pub has_more: bool,
    pub next_before: Option<String>,
}

pub(crate) enum MessageEdit<'a> {
    Markdown(&'a str),
    Text { find: &'a str, replace: &'a str },
    Subject(&'a str),
}

impl<'a> MessageEdit<'a> {
    pub(crate) fn from_args(
        markdown: Option<&'a str>,
        find: Option<&'a str>,
        replace: Option<&'a str>,
        subject: Option<&'a str>,
    ) -> Result<Self> {
        if let Some(subject) = subject {
            if markdown.is_some() || find.is_some() || replace.is_some() {
                bail!("Subject edits must be requested on their own.");
            }
            let subject = subject.trim();
            if subject.is_empty() || subject.contains('\0') {
                bail!("Subject must be non-empty text without NUL characters.");
            }
            return Ok(Self::Subject(subject));
        }
        match (markdown, find, replace) {
            (Some(markdown), None, None) if !markdown.trim().is_empty() => {
                Ok(Self::Markdown(markdown))
            }
            (Some(_), None, None) => bail!("Edited message body is empty."),
            (None, Some(find), Some(replace)) if !find.is_empty() => {
                if find.contains('\0') || replace.contains('\0') {
                    bail!("Find and replacement text must be free of NUL characters.");
                }
                Ok(Self::Text { find, replace })
            }
            (None, Some(_), Some(_)) => bail!("Find text is empty."),
            _ => bail!("Choose a full Markdown body, both find and replace text, or a subject."),
        }
    }
}

pub(crate) async fn edit_sent_message(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    msg_id: &str,
    parent_msg_id: Option<&str>,
    edit: MessageEdit<'_>,
) -> Result<()> {
    let parent_msg_id = parent_msg_id.filter(|id| !id.trim().is_empty());
    let saved = match edit {
        MessageEdit::Markdown(markdown) => {
            let compiled =
                parser::compile_outgoing_markdown(markdown, None, vec![], vec![], vec![], None);
            graph
                .edit_message(&conv.target, msg_id, parent_msg_id, &compiled.body_html)
                .await?;
            let _ = storage.apply_local_edit(
                &conv.conv_key,
                msg_id,
                &compiled.body_html,
                &Utc::now().to_rfc3339(),
            );
            return Ok(());
        }
        MessageEdit::Text { find, replace } => graph
            .replace_message_text(&conv.target, msg_id, parent_msg_id, find, replace)
            .await?,
        MessageEdit::Subject(subject) => {
            if parent_msg_id.is_some() {
                bail!("Subject edits require a channel root post.");
            }
            graph.edit_channel_post_subject(&conv.target, msg_id, subject).await?
        }
    };
    storage.upsert_messages(&[saved]).context(
        "Graph returned the edited message, but updating the local cache failed",
    )?;
    Ok(())
}

pub(crate) fn read_thread_offline(
    storage: &Storage,
    conv: &ConversationSummary,
    root_msg_id: &str,
    limit: usize,
) -> Result<Option<(ThreadView, ThreadCoverage)>> {
    if !matches!(conv.target, ConversationTarget::Channel { .. }) {
        bail!("Thread reads require a team channel target: {}", conv.conv_key);
    }
    let mut found = storage.get_fetched_thread(&conv.conv_key, root_msg_id, limit)?;
    if let Some((view, _)) = found.as_mut() {
        hydrate_thread_attachments(view);
    }
    Ok(found)
}

pub(crate) fn thread_cache_miss(root_msg_id: &str, limit: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "Thread not found in local cache at reply limit {limit}: {root_msg_id}. \
         Read the thread online to cache the requested replies."
    )
}

pub(crate) async fn read_thread(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    root_msg_id: &str,
    limit: usize,
) -> Result<(ThreadView, ThreadCoverage)> {
    let ConversationTarget::Channel { team_id, channel_id } = &conv.target else {
        bail!("Thread reads require a team channel target: {}", conv.conv_key);
    };
    let (mut thread, complete) = graph
        .get_thread_with_coverage(team_id, channel_id, root_msg_id, limit)
        .await?;
    storage.save_fetched_thread(&thread, complete)?;
    hydrate_thread_attachments(&mut thread);
    let coverage = ThreadCoverage {
        has_more: !complete,
        reply_total: complete.then_some(thread.replies.len()),
    };
    Ok((thread, coverage))
}

/// Resolves `--before` (`TIMESTAMP_OR_MSG_ID`) into `(before_created_at, Option<before_msg_id>)`.
pub(crate) fn resolve_before_cursor(
    storage: &Storage,
    conv_key: &str,
    before_input: &str,
) -> Result<(String, Option<String>)> {
    let trimmed = before_input.trim();
    if trimmed.is_empty() {
        bail!("--before value is empty.");
    }

    if let Some(msg) = storage.get_message(conv_key, trimmed)? {
        return Ok((
            crate::storage::db::normalize_timestamp_sort_key(&msg.created_at),
            Some(msg.msg_id),
        ));
    }

    let ts = normalize_before_timestamp(trimmed)?;
    Ok((ts, None))
}

pub(super) fn normalize_before_timestamp(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Nanos, true));
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = date
            .and_hms_opt(0, 0, 0)
            .context("Failed to construct midnight UTC timestamp")?
            .and_utc();
        return Ok(dt.to_rfc3339_opts(SecondsFormat::Nanos, true));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%d_%H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            let local_dt = Local
                .from_local_datetime(&ndt)
                .single()
                .or_else(|| Local.from_local_datetime(&ndt).earliest())
                .context("Failed to convert local timestamp")?;
            return Ok(local_dt
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Nanos, true));
        }
    }
    bail!(
        "Invalid --before value: '{trimmed}' (expected ISO-8601 timestamp, YYYY-MM-DD, or cached message ID)"
    )
}

pub(super) fn trim_older_window(mut rows: Vec<MessageItem>, limit: usize) -> (Vec<MessageItem>, bool) {
    if rows.len() > limit {
        let excess = rows.len() - limit;
        rows.drain(..excess);
        (rows, true)
    } else {
        (rows, false)
    }
}

pub(crate) fn hydrate_message_attachments(msg: &mut MessageItem) {
    if msg.deleted {
        return;
    }
    if let std::borrow::Cow::Owned(effective) = msg.effective_attachments() {
        msg.attachments = effective;
    }
}

pub(crate) fn hydrate_messages_attachments(messages: &mut [MessageItem]) {
    for msg in messages {
        hydrate_message_attachments(msg);
    }
}

pub(crate) fn hydrate_thread_attachments(thread: &mut crate::models::ThreadView) {
    hydrate_message_attachments(&mut thread.root);
    hydrate_messages_attachments(&mut thread.replies);
}

pub(crate) fn read_conversation_messages_offline(
    storage: &Storage,
    conv: &ConversationSummary,
    limit: usize,
    before: Option<&str>,
) -> Result<ReadMessagesResult> {
    let limit = clamp_limit(limit);
    let (mut messages, has_more) = match before {
        None => {
            let rows = storage.list_root_messages(&conv.conv_key, limit + 1, None)?;
            trim_older_window(rows, limit)
        }
        Some(raw_before) => {
            let (before_ts, before_id) = resolve_before_cursor(storage, &conv.conv_key, raw_before)?;
            let rows = storage.list_older_root_messages(
                &conv.conv_key,
                limit + 1,
                &before_ts,
                before_id.as_deref(),
            )?;
            trim_older_window(rows, limit)
        }
    };
    hydrate_messages_attachments(&mut messages);
    let next_before = messages.first().map(|m| m.msg_id.clone());
    Ok(ReadMessagesResult {
        messages,
        has_more,
        next_before,
    })
}

pub(crate) async fn read_conversation_messages(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    limit: usize,
    before: Option<&str>,
    offline: bool,
    fetch_older: bool,
) -> Result<ReadMessagesResult> {
    let limit = clamp_limit(limit);
    if offline {
        return read_conversation_messages_offline(storage, conv, limit, before);
    }

    let scope_key = SyncCursor::history_scope(&conv.conv_key);
    match before {
        None => {
            let (fetched, next_link) = match graph.list_messages_with_cursor(&conv.target, limit).await
            {
                Ok(res) => res,
                Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
            };
            storage.upsert_messages(&fetched)?;
            let oldest_ts = if next_link.is_none() {
                Some(SyncCursor::HISTORY_EOF.to_string())
            } else {
                fetched
                    .iter()
                    .filter(|m| m.parent_msg_id.is_none())
                    .map(|m| crate::storage::db::normalize_timestamp_sort_key(&m.created_at))
                    .filter(|s| !s.is_empty())
                    .min()
            };
            if oldest_ts.is_some() {
                // A plain read only sees the newest page: it must not replace a cursor that
                // an earlier `--before` walk or the TUI already moved deeper (or to EOF).
                let _ = storage.save_history_cursor_if_deeper(&SyncCursor {
                    scope_key: scope_key.clone(),
                    delta_link: next_link.clone(),
                    oldest_msg_ts: oldest_ts,
                    synced_at: Utc::now().to_rfc3339(),
                });
            }

            let rows = storage.list_root_messages(&conv.conv_key, limit + 1, None)?;
            let (mut messages, mut has_more) = trim_older_window(rows, limit);
            if messages.is_empty() {
                messages = fetched;
            }
            if !has_more {
                has_more = next_link.is_some()
                    || storage
                        .get_sync_cursor(&scope_key)
                        .ok()
                        .flatten()
                        .is_some_and(|c| !c.is_history_eof() && c.delta_link.is_some());
            }
            hydrate_messages_attachments(&mut messages);
            let next_before = messages.first().map(|m| m.msg_id.clone());
            Ok(ReadMessagesResult {
                messages,
                has_more,
                next_before,
            })
        }
        Some(raw_before) => {
            let (before_ts, before_id) = resolve_before_cursor(storage, &conv.conv_key, raw_before)?;
            if fetch_older {
                let cursor = storage.get_sync_cursor(&scope_key)?;
                let cursor_is_eof = cursor.as_ref().is_some_and(SyncCursor::is_history_eof);
                let local = storage.list_older_root_messages(
                    &conv.conv_key,
                    limit,
                    &before_ts,
                    before_id.as_deref(),
                )?;
                if local.len() >= limit || cursor_is_eof {
                    // Online history revalidation: reconcile the requested older window against Graph
                    // so edits/deletes/reactions in previously cached older pages are reflected.
                    let (fetched, new_next_link) = match graph
                        .fetch_older_messages(&conv.target, None, Some(&before_ts), limit)
                        .await
                    {
                        Ok(res) => res,
                        Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
                    };
                    if !fetched.is_empty() {
                        storage.upsert_messages(&fetched)?;
                    }
                    if new_next_link.is_none() || !cursor_is_eof {
                        let oldest_ts = if new_next_link.is_none() {
                            Some(SyncCursor::HISTORY_EOF.to_string())
                        } else {
                            fetched
                                .iter()
                                .filter(|m| m.parent_msg_id.is_none())
                                .map(|m| crate::storage::db::normalize_timestamp_sort_key(&m.created_at))
                                .filter(|s| !s.is_empty())
                                .min()
                                .or_else(|| cursor.as_ref().and_then(|c| c.oldest_msg_ts.clone()))
                        };
                        let _ = storage.save_sync_cursor(&SyncCursor {
                            scope_key: scope_key.clone(),
                            delta_link: new_next_link,
                            oldest_msg_ts: oldest_ts,
                            synced_at: Utc::now().to_rfc3339(),
                        });
                    }
                } else {
                    let cursor_connected = cursor.as_ref().is_some_and(|c| {
                        !c.is_history_eof()
                            && c.delta_link.is_some()
                            && c.oldest_msg_ts.as_deref().is_some_and(|ots| {
                                let norm_ots =
                                    crate::storage::db::normalize_timestamp_sort_key(ots);
                                !norm_ots.is_empty() && norm_ots >= before_ts
                            })
                    });
                    let mut next_cursor_link = if cursor_connected {
                        cursor.as_ref().and_then(|c| c.delta_link.clone())
                    } else {
                        None
                    };
                    let mut fetched_older_ids = std::collections::HashSet::new();

                    loop {
                        let prev_link = next_cursor_link.clone();
                        let remaining = limit.saturating_sub(fetched_older_ids.len()).max(1);
                        let (fetched, new_next_link) = match graph
                            .fetch_older_messages(
                                &conv.target,
                                prev_link.as_deref(),
                                Some(&before_ts),
                                remaining,
                            )
                            .await
                        {
                            Ok(res) => res,
                            Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
                        };

                        if !fetched.is_empty() {
                            for m in &fetched {
                                if m.parent_msg_id.is_none() {
                                    let m_sort = crate::storage::db::normalize_timestamp_sort_key(
                                        &m.created_at,
                                    );
                                    let is_older = match before_id.as_deref() {
                                        Some(bid) => {
                                            (m_sort.as_str(), m.msg_id.as_str())
                                                < (before_ts.as_str(), bid)
                                        }
                                        None => m_sort.as_str() < before_ts.as_str(),
                                    };
                                    if is_older {
                                        fetched_older_ids.insert(m.msg_id.clone());
                                    }
                                }
                            }
                            storage.upsert_messages(&fetched)?;
                        }
                        let oldest_ts = if new_next_link.is_none() {
                            Some(SyncCursor::HISTORY_EOF.to_string())
                        } else {
                            fetched
                                .iter()
                                .filter(|m| m.parent_msg_id.is_none())
                                .map(|m| {
                                    crate::storage::db::normalize_timestamp_sort_key(&m.created_at)
                                })
                                .filter(|s| !s.is_empty())
                                .min()
                        };
                        if oldest_ts.is_some() {
                            let _ = storage.save_sync_cursor(&SyncCursor {
                                scope_key: scope_key.clone(),
                                delta_link: new_next_link.clone(),
                                oldest_msg_ts: oldest_ts,
                                synced_at: Utc::now().to_rfc3339(),
                            });
                        }

                        next_cursor_link = new_next_link;
                        if fetched_older_ids.len() >= limit
                            || next_cursor_link.is_none()
                            || (fetched.is_empty() && next_cursor_link == prev_link)
                        {
                            break;
                        }
                    }
                }
            }

            let rows = storage.list_older_root_messages(
                &conv.conv_key,
                limit + 1,
                &before_ts,
                before_id.as_deref(),
            )?;
            let (mut messages, mut has_more) = trim_older_window(rows, limit);
            if !has_more {
                has_more = storage
                    .get_sync_cursor(&scope_key)
                    .ok()
                    .flatten()
                    .is_some_and(|c| !c.is_history_eof() && c.delta_link.is_some());
            }
            hydrate_messages_attachments(&mut messages);
            let next_before = messages.first().map(|m| m.msg_id.clone());
            Ok(ReadMessagesResult {
                messages,
                has_more,
                next_before,
            })
        }
    }
}

pub(crate) fn parse_mentions_cli(values: &[String]) -> Result<Vec<MentionInfo>> {
    values
        .iter()
        .enumerate()
        .map(|(idx, raw)| parse_mention_spec(raw, idx as i32))
        .collect()
}

pub(crate) fn parse_mention_spec(spec: &str, id: i32) -> Result<MentionInfo> {
    let Some((uid, name)) = spec.split_once(':') else {
        bail!("Invalid mention format: '{spec}' (expected user_id:DisplayName)");
    };
    let user_id = uid.trim();
    let display_name = name.trim();
    if user_id.is_empty() || display_name.is_empty() {
        bail!("Mention user_id and DisplayName cannot be empty: '{spec}'");
    }
    Ok(MentionInfo {
        id,
        user_id: user_id.to_string(),
        display_name: display_name.to_string(),
    })
}

pub(crate) fn ensure_mentions_used(
    requested: &[MentionInfo],
    used: &[MentionInfo],
) -> Result<()> {
    let missing: Vec<&MentionInfo> = requested
        .iter()
        .filter(|req| !used.iter().any(|u| u.user_id == req.user_id))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let list = missing
        .iter()
        .map(|m| format!("@{} ({})", m.display_name, m.user_id))
        .collect::<Vec<_>>()
        .join(", ");
    let tokens = missing
        .iter()
        .map(|m| format!("@{}", m.display_name))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "Mention not found in message body: {list}. Write {tokens} in the text where the mention should appear."
    );
}

pub(crate) fn normalize_reaction(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Reaction name is empty.");
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "like" | "heart" | "laugh" | "surprised" | "sad" | "angry" => Ok(lower),
        _ if !trimmed.is_ascii() => Ok(trimmed.to_string()),
        _ => bail!(
            "Unsupported reaction: '{trimmed}' (use like, heart, laugh, surprised, sad, angry, or an emoji)"
        ),
    }
}

pub(crate) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

pub(crate) async fn build_quote_reply(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    msg_id: &str,
) -> Option<QuotedReply> {
    if conv.kind == ConversationKind::Channel {
        return None;
    }
    let msg = match storage.get_message(&conv.conv_key, msg_id).ok().flatten() {
        Some(m) => Some(m),
        None => match graph.get_message(&conv.target, msg_id, None).await {
            Ok(m) => {
                let _ = storage.upsert_messages(std::slice::from_ref(&m));
                Some(m)
            }
            Err(_) => None,
        },
    };
    msg.map(|m| QuotedReply {
        msg_id: m.msg_id,
        author_name: m.author_name,
        preview_plain: truncate_single_line(&m.body_plain, 120),
    })
}

pub(crate) async fn send_file_or_inline(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    path: &Path,
    caption: Option<&str>,
    thread_root_id: Option<&str>,
    inline: bool,
) -> Result<MessageItem> {
    if !path.exists() || !path.is_file() {
        bail!("File not found: {}", path.display());
    }

    let mut sent = if inline {
        let mime_type = image_mime_for_path(path).with_context(|| {
            format!(
                "--inline only supports image files (png/jpg/jpeg/gif/webp/bmp): {}",
                path.display()
            )
        })?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read image file: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToString::to_string);
        let temp_id = "1".to_string();
        let inline_upload = InlineImageUpload {
            temp_id: temp_id.clone(),
            mime_type: mime_type.to_string(),
            bytes,
            file_name: file_name.clone(),
        };
        let mut outgoing = parser::compile_outgoing_markdown(
            caption.unwrap_or(""),
            None,
            vec![],
            vec![],
            vec![inline_upload],
            None,
        );
        let marker = format!("hostedContents/{temp_id}/$value");
        if !outgoing.body_html.contains(&marker) {
            let alt = file_name.as_deref().unwrap_or("image");
            outgoing.body_html.push_str(&format!(
                "<p><img src=\"../{marker}\" alt=\"{alt}\" /></p>"
            ));
        }
        if let Some(root_id) = thread_root_id {
            graph.reply_message(&conv.target, root_id, &outgoing).await?
        } else {
            graph.send_message(&conv.target, &outgoing).await?
        }
    } else {
        let caption_html = caption
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| {
                parser::compile_outgoing_markdown(c, None, vec![], vec![], vec![], None).body_html
            });
        graph
            .send_file(&conv.target, path, caption_html.as_deref(), thread_root_id)
            .await?
    };

    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
    hydrate_message_attachments(&mut sent);
    Ok(sent)
}

pub(crate) async fn find_message_with_fallback(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    msg_id: &str,
    thread_root_id: Option<&str>,
) -> Result<MessageItem> {
    if let Some(msg) = storage.get_message(&conv.conv_key, msg_id)? {
        return Ok(msg);
    }

    let parent_id = match &conv.target {
        ConversationTarget::Channel { .. } => {
            thread_root_id.map(str::trim).filter(|s| !s.is_empty())
        }
        ConversationTarget::Chat { .. } => None,
    };
    let msg = graph.get_message(&conv.target, msg_id, parent_id).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&msg));
    Ok(msg)
}

pub(crate) async fn download_message_attachment(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    msg_id: &str,
    index: usize,
    thread_root_id: Option<&str>,
    dest_dir: &Path,
) -> Result<DownloadedFile> {
    let msg = find_message_with_fallback(graph, storage, conv, msg_id, thread_root_id).await?;
    let attachments = msg.effective_attachments();
    if attachments.is_empty() {
        bail!("Message has no attachments: {msg_id}");
    }
    let att = attachments.get(index).with_context(|| {
        format!(
            "Attachment index {index} is out of range (total {})",
            attachments.len()
        )
    })?;
    graph.download_attachment(att, dest_dir).await
}
