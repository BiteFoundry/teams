//! Repository CRUD queries for conversations, teams, messages, threads, members, presences, and sync cursors.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::models::{
    AttachmentInfo, ChannelSummary, ConversationKind, ConversationSummary, ConversationTarget,
    MemberInfo, MentionInfo, MessageItem, PresenceStatus, ReactionSummary, SyncCursor, TeamSummary,
    ThreadView,
};

pub fn upsert_conversations(conn: &mut Connection, items: &[ConversationSummary]) -> Result<()> {
    upsert_conversations_with_null_topics(conn, items, &HashSet::new())
}

pub(crate) fn upsert_conversations_with_null_topics(
    conn: &mut Connection,
    items: &[ConversationSummary],
    clear_null_topic_keys: &HashSet<String>,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO conversations (
                conv_key, target_json, kind, team_id, team_name, title, topic,
                unread_count, last_read_at, last_msg_id, last_msg_author, last_msg_preview,
                last_updated_at, pinned, peer_user_id, web_url
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16
            )
            ON CONFLICT(conv_key) DO UPDATE SET
                target_json = excluded.target_json,
                kind = excluded.kind,
                team_id = COALESCE(excluded.team_id, conversations.team_id),
                team_name = COALESCE(excluded.team_name, conversations.team_name),
                title = excluded.title,
                topic = CASE
                    WHEN ?17 = 1 THEN NULL
                    ELSE COALESCE(excluded.topic, conversations.topic)
                END,
                unread_count = excluded.unread_count,
                last_read_at = COALESCE(excluded.last_read_at, conversations.last_read_at),
                last_msg_id = COALESCE(excluded.last_msg_id, conversations.last_msg_id),
                last_msg_author = CASE
                    WHEN excluded.last_msg_id IS NOT NULL THEN excluded.last_msg_author
                    ELSE conversations.last_msg_author
                END,
                last_msg_preview = CASE
                    WHEN excluded.last_msg_id IS NOT NULL THEN excluded.last_msg_preview
                    ELSE conversations.last_msg_preview
                END,
                last_updated_at = CASE
                    WHEN excluded.last_updated_at IS NOT NULL
                        AND (conversations.last_updated_at IS NULL OR excluded.last_updated_at >= conversations.last_updated_at)
                    THEN excluded.last_updated_at
                    ELSE conversations.last_updated_at
                END,
                pinned = excluded.pinned,
                peer_user_id = COALESCE(excluded.peer_user_id, conversations.peer_user_id),
                web_url = COALESCE(excluded.web_url, conversations.web_url)
            ",
        )?;

        for item in items {
            let target_json = serde_json::to_string(&item.target)?;
            let clear_topic = i32::from(
                item.topic.is_none() && clear_null_topic_keys.contains(&item.conv_key),
            );
            stmt.execute(params![
                item.conv_key,
                target_json,
                item.kind.as_str(),
                item.team_id,
                item.team_name,
                item.title,
                item.topic,
                item.unread_count,
                item.last_read_at,
                item.last_msg_id,
                item.last_msg_author,
                item.last_msg_preview,
                item.last_updated_at,
                if item.pinned { 1i32 } else { 0i32 },
                item.peer_user_id,
                item.web_url,
                clear_topic,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn save_updated_chat_topic(
    conn: &mut Connection,
    chat: &ConversationSummary,
) -> Result<ConversationSummary> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO conversations (conv_key, target_json, kind, title, topic, last_updated_at, web_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(conv_key) DO UPDATE SET title = excluded.title, topic = excluded.topic",
        params![
            chat.conv_key,
            serde_json::to_string(&chat.target)?,
            chat.kind.as_str(),
            chat.title,
            chat.topic,
            chat.last_updated_at,
            chat.web_url,
        ],
    )?;
    let saved = get_conversation(&tx, &chat.conv_key)?.context("Updated chat is missing from the cache")?;
    tx.commit()?;
    Ok(saved)
}

pub fn upsert_teams(conn: &mut Connection, teams: &[TeamSummary]) -> Result<()> {
    if teams.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut team_stmt = tx.prepare_cached(
            "
            INSERT INTO teams (team_id, display_name, description, channels_json)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(team_id) DO UPDATE SET
                display_name = excluded.display_name,
                description = excluded.description,
                channels_json = excluded.channels_json
            ",
        )?;

        let mut conv_stmt = tx.prepare_cached(
            "
            INSERT INTO conversations (
                conv_key, target_json, kind, team_id, team_name, title, topic,
                unread_count, last_read_at, last_msg_id, last_msg_author, last_msg_preview,
                last_updated_at, pinned, peer_user_id, web_url
            ) VALUES (
                ?1, ?2, 'channel', ?3, ?4, ?5, ?6,
                0, NULL, NULL, NULL, NULL,
                NULL, 0, NULL, ?7
            )
            ON CONFLICT(conv_key) DO UPDATE SET
                target_json = excluded.target_json,
                kind = 'channel',
                team_id = excluded.team_id,
                team_name = excluded.team_name,
                title = excluded.title,
                topic = COALESCE(excluded.topic, conversations.topic),
                web_url = COALESCE(excluded.web_url, conversations.web_url)
            ",
        )?;

        for team in teams {
            let channels_json = serde_json::to_string(&team.channels)?;
            team_stmt.execute(params![
                team.id,
                team.display_name,
                team.description,
                channels_json,
            ])?;

            for ch in &team.channels {
                let target = ch.target();
                let conv_key = target.conv_key();
                let target_json = serde_json::to_string(&target)?;
                let team_name = if ch.team_name.trim().is_empty() {
                    team.display_name.as_str()
                } else {
                    ch.team_name.as_str()
                };
                conv_stmt.execute(params![
                    conv_key,
                    target_json,
                    ch.team_id,
                    team_name,
                    ch.display_name,
                    ch.description,
                    ch.web_url,
                ])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_conversations(conn: &Connection) -> Result<Vec<ConversationSummary>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            conv_key, target_json, kind, team_id, team_name, title, topic,
            unread_count, last_read_at, last_msg_id, last_msg_author, last_msg_preview,
            last_updated_at, pinned, peer_user_id, web_url
        FROM conversations
        ORDER BY pinned DESC, COALESCE(last_updated_at, '') DESC, title ASC
        ",
    )?;
    let rows = stmt.query_map([], map_conversation_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_teams(conn: &Connection) -> Result<Vec<TeamSummary>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT team_id, display_name, description, channels_json
        FROM teams
        ORDER BY display_name ASC
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        let channels_json: String = row.get(3)?;
        let channels: Vec<ChannelSummary> =
            serde_json::from_str(&channels_json).unwrap_or_default();
        Ok(TeamSummary {
            id: row.get(0)?,
            display_name: row.get(1)?,
            description: row.get(2)?,
            channels,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn get_conversation(conn: &Connection, conv_key: &str) -> Result<Option<ConversationSummary>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            conv_key, target_json, kind, team_id, team_name, title, topic,
            unread_count, last_read_at, last_msg_id, last_msg_author, last_msg_preview,
            last_updated_at, pinned, peer_user_id, web_url
        FROM conversations
        WHERE conv_key = ?1
        ",
    )?;
    stmt.query_row(params![conv_key], map_conversation_row)
        .optional()
        .map_err(Into::into)
}

pub fn mark_conversation_read(conn: &Connection, conv_key: &str, read_at: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "UPDATE conversations SET unread_count = 0, last_read_at = ?1 WHERE conv_key = ?2",
    )?;
    stmt.execute(params![read_at, conv_key])?;
    Ok(())
}

pub fn increment_conversation_unread(conn: &Connection, conv_key: &str, delta: i32) -> Result<()> {
    if delta <= 0 {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "UPDATE conversations SET unread_count = MAX(0, unread_count) + ?1 WHERE conv_key = ?2",
    )?;
    stmt.execute(params![delta, conv_key])?;
    Ok(())
}

pub fn set_conversation_pinned(conn: &Connection, conv_key: &str, pinned: bool) -> Result<()> {
    let mut stmt =
        conn.prepare_cached("UPDATE conversations SET pinned = ?1 WHERE conv_key = ?2")?;
    stmt.execute(params![if pinned { 1i32 } else { 0i32 }, conv_key])?;
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpsertMessagesOutcome {
    pub changed_ids: Vec<String>,
    pub inserted_ids: Vec<String>,
    pub preview_updated_convs: Vec<String>,
}

#[cfg(test)]
thread_local! {
    static TEST_MAPPED_MESSAGE_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_mapped_message_rows() {
    TEST_MAPPED_MESSAGE_ROWS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn mapped_message_rows() -> usize {
    TEST_MAPPED_MESSAGE_ROWS.with(|c| c.get())
}

fn serialize_json_slice<T: serde::Serialize>(items: &[T]) -> Result<Cow<'static, str>> {
    if items.is_empty() {
        Ok(Cow::Borrowed("[]"))
    } else {
        Ok(Cow::Owned(serde_json::to_string(items)?))
    }
}

pub fn upsert_messages(conn: &mut Connection, messages: &[MessageItem]) -> Result<usize> {
    upsert_messages_inner(conn, messages, None)
}

pub fn upsert_messages_returning_changed_ids(
    conn: &mut Connection,
    messages: &[MessageItem],
) -> Result<Vec<String>> {
    let mut outcome = UpsertMessagesOutcome::default();
    upsert_messages_inner(conn, messages, Some(&mut outcome))?;
    Ok(outcome.changed_ids)
}

pub fn upsert_messages_detailed(
    conn: &mut Connection,
    messages: &[MessageItem],
) -> Result<UpsertMessagesOutcome> {
    let mut outcome = UpsertMessagesOutcome::default();
    upsert_messages_inner(conn, messages, Some(&mut outcome))?;
    Ok(outcome)
}

fn upsert_messages_inner(
    conn: &mut Connection,
    messages: &[MessageItem],
    outcome: Option<&mut UpsertMessagesOutcome>,
) -> Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let changed_count = upsert_messages_in_transaction(&tx, messages, outcome)?;
    tx.commit()?;
    Ok(changed_count)
}

fn upsert_messages_in_transaction(
    tx: &Transaction<'_>,
    messages: &[MessageItem],
    mut outcome: Option<&mut UpsertMessagesOutcome>,
) -> Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let mut changed_count = 0usize;
    let mut touched_threads: HashSet<(String, String)> = HashSet::new();
    let mut newest_by_conv: HashMap<String, &MessageItem> = HashMap::new();
    let mut empty_ts_tombstones: Vec<(String, String)> = Vec::new();

    {
        let mut check_stmt = tx.prepare_cached(
            "
            SELECT
                last_modified_at, deleted, reply_count, reactions_json, body_plain, author_name, subject,
                attachments_json, mentions_json, body_html = ?2, parent_msg_id
            FROM messages
            WHERE msg_key = ?1
            ",
        )?;

        let mut msg_stmt = tx.prepare_cached(
            "
            INSERT INTO messages (
                msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
                subject, author_id, author_name, is_me, created_at, created_at_sort, last_modified_at, deleted,
                importance, message_type, body_html, body_plain, attachments_json, mentions_json,
                reactions_json, reply_count, web_url, has_mentions
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21,
                ?22, ?23, ?24, ?25
            )
            ON CONFLICT(msg_key) DO UPDATE SET
                parent_msg_id = COALESCE(excluded.parent_msg_id, messages.parent_msg_id),
                reply_to_id = excluded.reply_to_id,
                reply_to_author = excluded.reply_to_author,
                reply_to_preview = excluded.reply_to_preview,
                subject = excluded.subject,
                author_id = COALESCE(excluded.author_id, messages.author_id),
                author_name = CASE
                    WHEN excluded.deleted = 1 AND excluded.author_name = 'Unknown' THEN messages.author_name
                    ELSE excluded.author_name
                END,
                is_me = CASE
                    WHEN excluded.deleted = 1 AND excluded.author_id IS NULL THEN messages.is_me
                    ELSE excluded.is_me
                END,
                created_at = CASE
                    WHEN excluded.created_at = '' THEN messages.created_at
                    ELSE excluded.created_at
                END,
                created_at_sort = CASE
                    WHEN excluded.created_at = '' THEN messages.created_at_sort
                    ELSE excluded.created_at_sort
                END,
                last_modified_at = COALESCE(excluded.last_modified_at, messages.last_modified_at),
                deleted = excluded.deleted,
                importance = excluded.importance,
                message_type = excluded.message_type,
                body_html = excluded.body_html,
                body_plain = excluded.body_plain,
                attachments_json = excluded.attachments_json,
                mentions_json = excluded.mentions_json,
                has_mentions = excluded.has_mentions,
                reactions_json = excluded.reactions_json,
                reply_count = MAX(messages.reply_count, excluded.reply_count),
                web_url = COALESCE(excluded.web_url, messages.web_url)
            ",
        )?;

        let mut fts_del_stmt = tx.prepare_cached("DELETE FROM messages_fts WHERE msg_key = ?1")?;
        let mut fts_ins_stmt = tx.prepare_cached(
            "
            INSERT INTO messages_fts (msg_key, conv_key, author_name, body_plain, subject)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ",
        )?;

        for msg in messages {
            let msg_key: Cow<'_, str> = if msg.msg_key.is_empty() {
                Cow::Owned(MessageItem::make_msg_key(&msg.conv_key, &msg.msg_id))
            } else {
                Cow::Borrowed(msg.msg_key.as_str())
            };
            let deleted_int = if msg.deleted { 1i32 } else { 0i32 };
            let reply_count_i64 = msg.reply_count as i64;

            let existing: Option<(
                Option<String>,
                i32,
                i64,
                String,
                String,
                String,
                Option<String>,
                String,
                String,
                bool,
                Option<String>,
            )> = check_stmt
                .query_row(params![msg_key.as_ref(), &msg.body_html], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                    ))
                })
                .optional()?;

            let mut reactions_json_opt: Option<Cow<'static, str>> = None;
            let mut attachments_json_opt: Option<Cow<'static, str>> = None;
            let mut mentions_json_opt: Option<Cow<'static, str>> = None;

            let fts_unchanged = if let Some((
                ref ex_last_mod,
                ex_deleted,
                ex_reply_count,
                ref ex_reactions_json,
                ref ex_body_plain,
                ref ex_author_name,
                ref ex_subject,
                ref ex_attachments_json,
                ref ex_mentions_json,
                body_html_same,
                _,
            )) = existing
            {
                let effective_author_same = (msg.deleted && msg.author_name == "Unknown")
                    || *ex_author_name == msg.author_name;
                let scalars_unchanged = (*ex_last_mod == msg.last_modified_at
                    || (msg.deleted && msg.last_modified_at.is_none()))
                    && ex_deleted == deleted_int
                    && reply_count_i64 <= ex_reply_count
                    && *ex_body_plain == msg.body_plain
                    && body_html_same
                    && effective_author_same
                    && *ex_subject == msg.subject;
                if scalars_unchanged {
                    let rj = serialize_json_slice(&msg.reactions)?;
                    let r_same = *ex_reactions_json == rj.as_ref();
                    reactions_json_opt = Some(rj);
                    if r_same {
                        let aj = serialize_json_slice(&msg.attachments)?;
                        let a_same = *ex_attachments_json == aj.as_ref();
                        attachments_json_opt = Some(aj);
                        if a_same {
                            let mj = serialize_json_slice(&msg.mentions)?;
                            let m_same = *ex_mentions_json == mj.as_ref();
                            mentions_json_opt = Some(mj);
                            if m_same {
                                continue;
                            }
                        }
                    }
                }
                ex_deleted == deleted_int
                    && effective_author_same
                    && *ex_body_plain == msg.body_plain
                    && *ex_subject == msg.subject
            } else {
                false
            };

            let reactions_json = match reactions_json_opt {
                Some(v) => v,
                None => serialize_json_slice(&msg.reactions)?,
            };
            let attachments_json = match attachments_json_opt {
                Some(v) => v,
                None => serialize_json_slice(&msg.attachments)?,
            };
            let mentions_json = match mentions_json_opt {
                Some(v) => v,
                None => serialize_json_slice(&msg.mentions)?,
            };
            let has_mentions_int = i32::from(!msg.mentions.is_empty());
            let created_at_sort = crate::storage::db::normalize_timestamp_sort_key(&msg.created_at);

            msg_stmt.execute(params![
                msg_key.as_ref(),
                msg.conv_key,
                msg.msg_id,
                msg.parent_msg_id,
                msg.reply_to_id,
                msg.reply_to_author,
                msg.reply_to_preview,
                msg.subject,
                msg.author_id,
                msg.author_name,
                if msg.is_me { 1i32 } else { 0i32 },
                msg.created_at,
                created_at_sort,
                msg.last_modified_at,
                deleted_int,
                msg.importance,
                msg.message_type,
                msg.body_html,
                msg.body_plain,
                attachments_json.as_ref(),
                mentions_json.as_ref(),
                reactions_json.as_ref(),
                reply_count_i64,
                msg.web_url,
                has_mentions_int,
            ])?;
            changed_count += 1;
            if let Some(out) = outcome.as_deref_mut() {
                if !out.changed_ids.iter().any(|id| id == &msg.msg_id) {
                    out.changed_ids.push(msg.msg_id.clone());
                }
                if existing.is_none() && !out.inserted_ids.iter().any(|id| id == &msg.msg_id) {
                    out.inserted_ids.push(msg.msg_id.clone());
                }
            }

            if !fts_unchanged {
                if existing.is_some() {
                    fts_del_stmt.execute(params![msg_key.as_ref()])?;
                }
                if !msg.deleted {
                    fts_ins_stmt.execute(params![
                        msg_key.as_ref(),
                        msg.conv_key,
                        msg.author_name,
                        msg.body_plain,
                        msg.subject.as_deref().unwrap_or(""),
                    ])?;
                }
            }

            if msg.deleted && msg.created_at.is_empty() {
                empty_ts_tombstones.push((msg.conv_key.clone(), msg.msg_id.clone()));
            } else {
                newest_by_conv
                    .entry(msg.conv_key.clone())
                    .and_modify(|existing_newest| {
                        let existing_sort = crate::storage::db::normalize_timestamp_sort_key(
                            &existing_newest.created_at,
                        );
                        if created_at_sort > existing_sort
                            || (created_at_sort == existing_sort
                                && msg.msg_id > existing_newest.msg_id)
                        {
                            *existing_newest = msg;
                        }
                    })
                    .or_insert(msg);
            }

            // A sparse delta tombstone can omit the already-stored parent ID.
            if let Some(root_id) = msg
                .parent_msg_id
                .as_ref()
                .or_else(|| existing.as_ref().and_then(|(.., parent)| parent.as_ref()))
            {
                touched_threads.insert((msg.conv_key.clone(), root_id.clone()));
            }
        }
    }

    if !touched_threads.is_empty() {
        let mut reply_count_stmt = tx.prepare_cached(
            "
            UPDATE messages
            SET reply_count = CASE
                WHEN (SELECT COUNT(*) FROM messages WHERE conv_key = ?1 AND parent_msg_id = ?2 AND deleted = 0) > 0
                     OR EXISTS(SELECT 1 FROM messages WHERE conv_key = ?1 AND parent_msg_id = ?2)
                THEN (SELECT COUNT(*) FROM messages WHERE conv_key = ?1 AND parent_msg_id = ?2 AND deleted = 0)
                ELSE reply_count
            END
            WHERE msg_key = ?3
            ",
        )?;
        for (conv_key, root_id) in touched_threads {
            let parent_msg_key = MessageItem::make_msg_key(&conv_key, &root_id);
            reply_count_stmt.execute(params![conv_key, root_id, parent_msg_key])?;
        }
    }

    if !newest_by_conv.is_empty() {
        let mut conv_update_stmt = tx.prepare_cached(
            "
            UPDATE conversations
            SET
                last_msg_id = ?1,
                last_msg_author = CASE
                    WHEN ?6 = 1 AND ?2 = 'Unknown' THEN last_msg_author
                    ELSE ?2
                END,
                last_msg_preview = ?3,
                last_updated_at = CASE
                    WHEN ?4 != '' AND (last_updated_at IS NULL OR last_updated_at < ?4) THEN ?4
                    ELSE last_updated_at
                END
            WHERE conv_key = ?5
              AND (
                  (
                      last_msg_id = ?1
                      AND (
                          last_msg_preview IS NOT ?3
                          OR (
                              CASE
                                  WHEN ?6 = 1 AND ?2 = 'Unknown' THEN last_msg_author
                                  ELSE ?2
                              END
                          ) IS NOT last_msg_author
                      )
                  )
                  OR (
                      ?4 != ''
                      AND (last_updated_at IS NULL OR last_updated_at < ?4)
                  )
              )
            ",
        )?;
        for (conv_key, msg) in newest_by_conv {
            let preview = build_message_preview(msg);
            let rows = conv_update_stmt.execute(params![
                msg.msg_id,
                msg.author_name,
                preview,
                msg.created_at,
                conv_key,
                if msg.deleted { 1i32 } else { 0i32 },
            ])?;
            if rows > 0 {
                if let Some(out) = outcome.as_deref_mut() {
                    if !out.preview_updated_convs.iter().any(|c| c == &conv_key) {
                        out.preview_updated_convs.push(conv_key);
                    }
                }
            }
        }
    }

    if !empty_ts_tombstones.is_empty() {
        let mut tombstone_stmt = tx.prepare_cached(
            "
            UPDATE conversations
            SET last_msg_preview = '(deleted message)'
            WHERE conv_key = ?1 AND last_msg_id = ?2
            ",
        )?;
        for (conv_key, msg_id) in empty_ts_tombstones {
            let rows = tombstone_stmt.execute(params![conv_key, msg_id])?;
            if rows > 0 {
                if let Some(out) = outcome.as_deref_mut() {
                    if !out.preview_updated_convs.iter().any(|c| c == &conv_key) {
                        out.preview_updated_convs.push(conv_key);
                    }
                }
            }
        }
    }

    Ok(changed_count)
}

pub fn list_root_messages(
    conn: &Connection,
    conv_key: &str,
    limit: usize,
    before_created_at: Option<&str>,
) -> Result<Vec<MessageItem>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if let Some(before) = before_created_at {
        return list_older_root_messages(conn, conv_key, limit, before, None);
    }

    let mut stmt = conn.prepare_cached(
        "
        SELECT
            msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
            subject, author_id, author_name, is_me, created_at, last_modified_at, deleted,
            importance, message_type, body_html, body_plain, attachments_json, mentions_json,
            reactions_json, reply_count, web_url
        FROM messages
        WHERE conv_key = ?1 AND parent_msg_id IS NULL
        ORDER BY created_at_sort DESC, msg_id DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![conv_key, limit as i64], map_message_row)?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    out.reverse();
    Ok(out)
}

/// Lists older root messages strictly preceding `(before_created_at, before_msg_id)` in chronological order (oldest first).
pub fn list_older_root_messages(
    conn: &Connection,
    conv_key: &str,
    limit: usize,
    before_created_at: &str,
    before_msg_id: Option<&str>,
) -> Result<Vec<MessageItem>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let before_sort = crate::storage::db::normalize_timestamp_sort_key(before_created_at);
    let mut out = if let Some(b_id) = before_msg_id {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
                subject, author_id, author_name, is_me, created_at, last_modified_at, deleted,
                importance, message_type, body_html, body_plain, attachments_json, mentions_json,
                reactions_json, reply_count, web_url
            FROM messages
            WHERE parent_msg_id IS NULL
              AND conv_key = ?1
              AND (created_at_sort, msg_id) < (?2, ?3)
            ORDER BY created_at_sort DESC, msg_id DESC
            LIMIT ?4
            ",
        )?;
        let rows = stmt.query_map(
            params![conv_key, before_sort, b_id, limit as i64],
            map_message_row,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
                subject, author_id, author_name, is_me, created_at, last_modified_at, deleted,
                importance, message_type, body_html, body_plain, attachments_json, mentions_json,
                reactions_json, reply_count, web_url
            FROM messages
            WHERE parent_msg_id IS NULL
              AND conv_key = ?1
              AND created_at_sort < ?2
            ORDER BY created_at_sort DESC, msg_id DESC
            LIMIT ?3
            ",
        )?;
        let rows = stmt.query_map(
            params![conv_key, before_sort, limit as i64],
            map_message_row,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    out.reverse();
    Ok(out)
}

pub fn list_thread_replies(
    conn: &Connection,
    conv_key: &str,
    root_msg_id: &str,
    limit: usize,
) -> Result<Vec<MessageItem>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
            subject, author_id, author_name, is_me, created_at, last_modified_at, deleted,
            importance, message_type, body_html, body_plain, attachments_json, mentions_json,
            reactions_json, reply_count, web_url
        FROM messages
        WHERE conv_key = ?1 AND parent_msg_id = ?2
        ORDER BY created_at_sort DESC, msg_id DESC
        LIMIT ?3
        ",
    )?;
    let rows = stmt.query_map(params![conv_key, root_msg_id, limit as i64], map_message_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    out.reverse();
    Ok(out)
}

pub fn get_message(
    conn: &Connection,
    conv_key: &str,
    msg_id: &str,
) -> Result<Option<MessageItem>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
            subject, author_id, author_name, is_me, created_at, last_modified_at, deleted,
            importance, message_type, body_html, body_plain, attachments_json, mentions_json,
            reactions_json, reply_count, web_url
        FROM messages
        WHERE conv_key = ?1 AND msg_id = ?2
        ",
    )?;
    stmt.query_row(params![conv_key, msg_id], map_message_row)
        .optional()
        .map_err(Into::into)
}

pub fn get_messages_by_ids(
    conn: &Connection,
    conv_key: &str,
    msg_ids: &[String],
) -> Result<Vec<MessageItem>> {
    let mut out = Vec::with_capacity(msg_ids.len());
    for (idx, msg_id) in msg_ids.iter().enumerate() {
        if msg_ids[..idx].iter().any(|prev| prev == msg_id) {
            continue;
        }
        if let Some(msg) = get_message(conn, conv_key, msg_id)? {
            out.push(msg);
        }
    }
    Ok(out)
}

pub fn get_thread(
    conn: &Connection,
    conv_key: &str,
    root_msg_id: &str,
    limit: usize,
) -> Result<Option<ThreadView>> {
    let Some(root) = get_message(conn, conv_key, root_msg_id)? else {
        return Ok(None);
    };
    let replies = list_thread_replies(conn, conv_key, root_msg_id, limit)?;
    Ok(Some(ThreadView {
        conv_key: conv_key.to_string(),
        root,
        replies,
    }))
}

pub(crate) fn save_fetched_thread(
    conn: &mut Connection,
    thread: &ThreadView,
    complete: bool,
) -> Result<()> {
    let messages: Vec<_> = std::iter::once(&thread.root)
        .chain(&thread.replies)
        .cloned()
        .collect();
    let reply_ids: Vec<_> = thread.replies.iter().map(|reply| &reply.msg_id).collect();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    upsert_messages_in_transaction(&tx, &messages, None)?;
    tx.execute(
        "INSERT INTO thread_reply_windows (conv_key, root_msg_id, reply_ids_json, complete)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(conv_key, root_msg_id) DO UPDATE SET
             reply_ids_json = excluded.reply_ids_json, complete = excluded.complete",
        params![thread.conv_key, thread.root.msg_id, serde_json::to_string(&reply_ids)?, complete],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn get_fetched_thread(
    conn: &Connection,
    conv_key: &str,
    root_msg_id: &str,
    limit: usize,
) -> Result<Option<ThreadView>> {
    let tx = conn.unchecked_transaction()?;
    let window: Option<(String, bool)> = tx
        .query_row(
            "SELECT reply_ids_json, complete FROM thread_reply_windows
             WHERE conv_key = ?1 AND root_msg_id = ?2",
            params![conv_key, root_msg_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((reply_ids_json, complete)) = window else {
        return Ok(None);
    };
    let ids: Vec<String> = serde_json::from_str(&reply_ids_json)?;
    if !complete && ids.len() < limit {
        return Ok(None);
    }
    let Some(mut root) = get_message(&tx, conv_key, root_msg_id)? else {
        return Ok(None);
    };
    if complete {
        root.reply_count = ids.len();
    }
    let requested_ids = &ids[ids.len().saturating_sub(limit)..];
    let replies = get_messages_by_ids(&tx, conv_key, requested_ids)?;
    if replies.len() != requested_ids.len() {
        return Ok(None);
    }
    tx.commit()?;
    Ok(Some(ThreadView {
        conv_key: conv_key.to_string(),
        root,
        replies,
    }))
}

pub fn list_mentions_for_user(
    conn: &Connection,
    my_user_id: &str,
    limit: usize,
) -> Result<Vec<MessageItem>> {
    if my_user_id.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let pattern = format!("%{}%", escape_like(my_user_id));
    let sql_limit = i64::try_from(limit.saturating_mul(4)).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            msg_key, conv_key, msg_id, parent_msg_id, reply_to_id, reply_to_author, reply_to_preview,
            subject, author_id, author_name, is_me, created_at, last_modified_at, deleted,
            importance, message_type, body_html, body_plain, attachments_json, mentions_json,
            reactions_json, reply_count, web_url
        FROM messages
        WHERE deleted = 0
          AND has_mentions = 1
          AND mentions_json LIKE ?1 ESCAPE '\\'
        ORDER BY created_at_sort DESC, msg_id DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![pattern, sql_limit], map_message_row)?;
    let mut out = Vec::new();
    for row in rows {
        let msg = row?;
        if msg.mentions.iter().any(|m| m.user_id == my_user_id) {
            out.push(msg);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

pub fn upsert_members(conn: &mut Connection, conv_key: &str, members: &[MemberInfo]) -> Result<()> {
    if members.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO members (conv_key, user_id, member_id, display_name, email, roles_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(conv_key, user_id) DO UPDATE SET
                member_id = COALESCE(excluded.member_id, members.member_id),
                display_name = excluded.display_name,
                email = COALESCE(excluded.email, members.email),
                roles_json = excluded.roles_json
            ",
        )?;
        for m in members {
            let roles_json = serde_json::to_string(&m.roles)?;
            stmt.execute(params![
                conv_key,
                m.user_id,
                m.member_id,
                m.display_name,
                m.email,
                roles_json,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_members(conn: &Connection, conv_key: &str) -> Result<Vec<MemberInfo>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            m.user_id, m.member_id, m.display_name, m.email, m.roles_json,
            p.availability, p.activity
        FROM members m
        LEFT JOIN presences p ON m.user_id = p.user_id
        WHERE m.conv_key = ?1
        ORDER BY m.display_name ASC
        ",
    )?;
    let rows = stmt.query_map(params![conv_key], |row| {
        let user_id: String = row.get(0)?;
        let roles_json: String = row.get(4)?;
        let roles: Vec<String> = serde_json::from_str(&roles_json).unwrap_or_default();
        let availability: Option<String> = row.get(5)?;
        let activity: Option<String> = row.get(6)?;
        let presence = availability.map(|avail| PresenceStatus {
            user_id: user_id.clone(),
            availability: avail,
            activity: activity.unwrap_or_default(),
        });
        Ok(MemberInfo {
            user_id,
            member_id: row.get(1)?,
            display_name: row.get(2)?,
            email: row.get(3)?,
            roles,
            presence,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn upsert_presences(conn: &mut Connection, presences: &[PresenceStatus]) -> Result<()> {
    if presences.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO presences (user_id, availability, activity, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(user_id) DO UPDATE SET
                availability = excluded.availability,
                activity = excluded.activity,
                updated_at = excluded.updated_at
            ",
        )?;
        for p in presences {
            stmt.execute(params![p.user_id, p.availability, p.activity, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn get_presences(conn: &Connection) -> Result<HashMap<String, PresenceStatus>> {
    let mut stmt = conn.prepare_cached("SELECT user_id, availability, activity FROM presences")?;
    let rows = stmt.query_map([], |row| {
        Ok(PresenceStatus {
            user_id: row.get(0)?,
            availability: row.get(1)?,
            activity: row.get(2)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let p = row?;
        out.insert(p.user_id.clone(), p);
    }
    Ok(out)
}

pub fn get_sync_cursor(conn: &Connection, scope_key: &str) -> Result<Option<SyncCursor>> {
    let mut stmt = conn.prepare_cached(
        "SELECT scope_key, delta_link, oldest_msg_ts, synced_at FROM sync_cursors WHERE scope_key = ?1",
    )?;
    stmt.query_row(params![scope_key], |row| {
        Ok(SyncCursor {
            scope_key: row.get(0)?,
            delta_link: row.get(1)?,
            oldest_msg_ts: row.get(2)?,
            synced_at: row.get(3)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

pub fn save_sync_cursor(conn: &Connection, cursor: &SyncCursor) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "
        INSERT INTO sync_cursors (scope_key, delta_link, oldest_msg_ts, synced_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(scope_key) DO UPDATE SET
            delta_link = excluded.delta_link,
            oldest_msg_ts = COALESCE(excluded.oldest_msg_ts, sync_cursors.oldest_msg_ts),
            synced_at = excluded.synced_at
        ",
    )?;
    stmt.execute(params![
        cursor.scope_key,
        cursor.delta_link,
        cursor.oldest_msg_ts,
        cursor.synced_at,
    ])?;
    Ok(())
}

pub(crate) fn apply_channel_delta(
    conn: &mut Connection,
    expected: Option<&SyncCursor>,
    cursor: &SyncCursor,
    messages: &[MessageItem],
    eligible_unread: &HashSet<&str>,
) -> Result<Option<UpsertMessagesOutcome>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if get_sync_cursor(&tx, &cursor.scope_key)?.as_ref() != expected {
        return Ok(None);
    }

    let mut outcome = UpsertMessagesOutcome::default();
    upsert_messages_in_transaction(&tx, messages, Some(&mut outcome))?;
    let unread = outcome
        .inserted_ids
        .iter()
        .filter(|id| eligible_unread.contains(id.as_str()))
        .count() as i32;
    if unread > 0 {
        increment_conversation_unread(&tx, &cursor.scope_key, unread)?;
    }
    save_sync_cursor(&tx, cursor)?;
    tx.commit()?;
    Ok(Some(outcome))
}

fn map_conversation_row(row: &Row<'_>) -> rusqlite::Result<ConversationSummary> {
    let conv_key: String = row.get(0)?;
    let target_json: String = row.get(1)?;
    let target: ConversationTarget = serde_json::from_str(&target_json)
        .ok()
        .or_else(|| ConversationTarget::from_conv_key(&conv_key))
        .unwrap_or_else(|| ConversationTarget::Chat {
            chat_id: conv_key.clone(),
        });
    let kind_str: String = row.get(2)?;
    let pinned_int: i32 = row.get(13)?;
    Ok(ConversationSummary {
        conv_key,
        target,
        kind: ConversationKind::from_db_str(&kind_str),
        team_id: row.get(3)?,
        team_name: row.get(4)?,
        title: row.get(5)?,
        topic: row.get(6)?,
        unread_count: row.get(7)?,
        last_read_at: row.get(8)?,
        last_msg_id: row.get(9)?,
        last_msg_author: row.get(10)?,
        last_msg_preview: row.get(11)?,
        last_updated_at: row.get(12)?,
        pinned: pinned_int != 0,
        peer_user_id: row.get(14)?,
        web_url: row.get(15)?,
    })
}

fn map_message_row(row: &Row<'_>) -> rusqlite::Result<MessageItem> {
    #[cfg(test)]
    TEST_MAPPED_MESSAGE_ROWS.with(|c| c.set(c.get() + 1));

    let is_me_int: i32 = row.get(10)?;
    let deleted_int: i32 = row.get(13)?;
    let attachments_json: String = row.get(18)?;
    let mentions_json: String = row.get(19)?;
    let reactions_json: String = row.get(20)?;
    let reply_count_i64: i64 = row.get(21)?;

    let attachments: Vec<AttachmentInfo> =
        serde_json::from_str(&attachments_json).unwrap_or_default();
    let mentions: Vec<MentionInfo> = serde_json::from_str(&mentions_json).unwrap_or_default();
    let reactions: Vec<ReactionSummary> =
        serde_json::from_str(&reactions_json).unwrap_or_default();

    Ok(MessageItem {
        msg_key: row.get(0)?,
        conv_key: row.get(1)?,
        msg_id: row.get(2)?,
        parent_msg_id: row.get(3)?,
        reply_to_id: row.get(4)?,
        reply_to_author: row.get(5)?,
        reply_to_preview: row.get(6)?,
        subject: row.get(7)?,
        author_id: row.get(8)?,
        author_name: row.get(9)?,
        is_me: is_me_int != 0,
        created_at: row.get(11)?,
        last_modified_at: row.get(12)?,
        deleted: deleted_int != 0,
        importance: row.get(14)?,
        message_type: row.get(15)?,
        body_html: row.get(16)?,
        body_plain: row.get(17)?,
        attachments,
        mentions,
        reactions,
        reply_count: reply_count_i64.max(0) as usize,
        web_url: row.get(22)?,
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(text.len()));
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= max_chars {
            while out.chars().count() >= max_chars {
                out.pop();
            }
            out.push('…');
            return out;
        }
        out.push(ch);
        count += 1;
    }
    out
}

fn build_message_preview(msg: &MessageItem) -> String {
    if msg.deleted {
        return "(deleted message)".to_string();
    }
    const MAX_CHARS: usize = 120;
    let mut single_line = String::with_capacity(MAX_CHARS.min(msg.body_plain.len()));
    let mut char_count = 0usize;
    let mut truncated = false;

    for word in msg.body_plain.split_whitespace() {
        if char_count > 0 {
            if char_count >= MAX_CHARS - 1 {
                truncated = true;
                break;
            }
            single_line.push(' ');
            char_count += 1;
        }
        for ch in word.chars() {
            if char_count >= MAX_CHARS {
                truncated = true;
                break;
            }
            single_line.push(ch);
            char_count += 1;
        }
        if truncated {
            break;
        }
    }

    if truncated {
        while single_line.chars().count() >= MAX_CHARS {
            single_line.pop();
        }
        single_line.push('…');
        return single_line;
    }
    if !single_line.is_empty() {
        return single_line;
    }

    if let Some(subj) = msg.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        truncate_chars(subj, MAX_CHARS)
    } else if let Some(att) = msg.attachments.first() {
        let label = format!(
            "[Attachment: {}]",
            att.name.as_deref().unwrap_or(att.id.as_str())
        );
        truncate_chars(&label, MAX_CHARS)
    } else {
        String::new()
    }
}

pub(crate) fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}
