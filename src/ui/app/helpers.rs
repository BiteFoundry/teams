//! Shared in-memory message ordering, incremental merge, history cursor, and nav-selection helpers.

use std::collections::{HashMap, HashSet};

use crate::models::{ConversationSummary, MessageItem, SyncCursor, ThreadView};

use super::super::nav_pane::{self, NavRow};
use super::{AppState, UiState};

impl AppState {
    pub(super) fn thread_msg_is_current(&self, conv_key: &str, root_msg_id: &str) -> bool {
        if self.active_conv_key.as_deref() != Some(conv_key) {
            return false;
        }
        self.pending_thread
            .as_ref()
            .is_some_and(|(c, r)| c == conv_key && r == root_msg_id)
            || self
                .active_thread
                .as_ref()
                .is_some_and(|t| t.root.msg_id == root_msg_id)
    }

    /// True when the history cursor says Graph has no older page *and* no cached root message
    /// precedes the loaded window. An empty window with an EOF cursor counts as exhausted.
    pub(super) fn cached_history_exhausted(&self, conv_key: &str) -> bool {
        let cursor_eof = self
            .storage
            .get_sync_cursor(&SyncCursor::history_scope(conv_key))
            .ok()
            .flatten()
            .is_some_and(|c| c.is_history_eof());
        cursor_eof
            && self
                .messages
                .iter()
                .find(|m| !m.msg_id.starts_with("optimistic-"))
                .is_none_or(|first| {
                    self.storage
                        .list_older_root_messages(conv_key, 1, &first.created_at, Some(&first.msg_id))
                        .map(|older| older.is_empty())
                        .unwrap_or(true)
                })
    }

    /// Selects a root message of the active conversation. When it is cached but older than
    pub(super) fn prepend_older_messages(&mut self, ui: &mut UiState, messages: Vec<MessageItem>) -> usize {
        if messages.is_empty() {
            return 0;
        }
        let mut existing_pos: HashMap<String, usize> = self
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| (m.msg_id.clone(), i))
            .collect();
        let mut new_older = Vec::new();
        let mut updated_existing = false;
        for msg in messages {
            if let Some(&pos) = existing_pos.get(&msg.msg_id) {
                self.messages[pos] = msg;
                updated_existing = true;
            } else {
                existing_pos.insert(msg.msg_id.clone(), usize::MAX);
                new_older.push(msg);
            }
        }
        if new_older.is_empty() {
            if updated_existing {
                self.bump_messages_gen();
            }
            return 0;
        }
        new_older.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.msg_id.cmp(&b.msg_id))
        });
        let prepended_count = new_older.len();
        if !ui.chat_scroll.follow_tail && ui.chat_scroll.total_lines > 0 {
            ui.pending_prepend_anchor =
                Some((ui.chat_scroll.total_lines, ui.chat_scroll.offset));
        }
        if let Some(sel) = ui.chat_scroll.selected_idx {
            ui.chat_scroll.selected_idx = Some(sel + prepended_count);
        }
        new_older.extend(std::mem::take(&mut self.messages));
        self.messages = new_older;
        self.bump_messages_gen();
        prepended_count
    }

    pub(super) fn emit_scope_warning_once(&mut self, ui: &mut UiState, warning: Option<String>) {
        if self.scope_warned {
            return;
        }
        if let Some(msg) = warning.filter(|s| !s.trim().is_empty()) {
            self.scope_warned = true;
            ui.info(msg);
        }
    }

    pub(super) fn apply_active_read_guard(&self, convs: &mut [ConversationSummary]) {
        if let (Some(active_key), Some((read_key, read_at))) = (
            self.active_conv_key.as_deref(),
            self.active_marked_read_at.as_ref(),
        ) {
            if active_key == read_key {
                if let Some(c) = convs.iter_mut().find(|c| c.conv_key == active_key) {
                    let stale_unread = c
                        .last_updated_at
                        .as_deref()
                        .is_none_or(|ts| ts <= read_at.as_str());
                    if stale_unread {
                        c.unread_count = 0;
                        if c.last_read_at.as_deref().is_none_or(|r| r < read_at.as_str()) {
                            c.last_read_at = Some(read_at.clone());
                        }
                    }
                }
            }
        }
    }

    pub(super) fn update_conversation_preview_from_sent(&mut self, conv_key: &str, sent: &MessageItem) {
        if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == conv_key) {
            let is_newer_or_same = sent.created_at.is_empty()
                || conv
                    .last_updated_at
                    .as_deref()
                    .is_none_or(|ts| sent.created_at.as_str() >= ts);
            if is_newer_or_same {
                conv.last_msg_id = Some(sent.msg_id.clone());
                conv.last_msg_author = Some(sent.author_name.clone());
                let preview = sent
                    .body_plain
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                conv.last_msg_preview = Some(if preview.is_empty() && !sent.attachments.is_empty() {
                    sent.attachments
                        .first()
                        .and_then(|a| a.name.clone())
                        .map(|n| format!("[Attachment: {n}]"))
                        .unwrap_or_else(|| "[Attachment]".to_string())
                } else {
                    preview
                });
                if !sent.created_at.is_empty() {
                    conv.last_updated_at = Some(sent.created_at.clone());
                }
                self.bump_conversations_gen();
            }
        }
    }
}

pub(super) fn mark_message_deleted_in_memory(msg: &mut MessageItem) {
    msg.deleted = true;
    msg.body_html.clear();
    msg.body_plain = "(deleted message)".to_string();
    msg.attachments.clear();
    msg.reactions.clear();
    msg.mentions.clear();
    msg.subject = None;
}

pub(super) fn clamp_chat_selection(messages_len: usize, selected_idx: &mut Option<usize>) {
    if let Some(sel) = *selected_idx {
        *selected_idx = if messages_len == 0 {
            None
        } else {
            Some(sel.min(messages_len - 1))
        };
    }
}

pub(super) fn clamp_thread_selection(thread: Option<&ThreadView>, selected_idx: &mut Option<usize>) {
    let total = thread.map_or(0, |t| 1 + t.replies.len());
    if let Some(sel) = *selected_idx {
        *selected_idx = if total == 0 {
            None
        } else {
            Some(sel.min(total - 1))
        };
    }
}

/// A saved page link that Graph no longer accepts (an expired `$skiptoken`): 400 or 410.
pub(super) fn is_expired_page_link_error(err: &anyhow::Error) -> bool {
    matches!(
        crate::graph::extract_graph_http_status(&format!("{err:#}")),
        Some(400 | 410)
    )
}

pub(crate) fn preserve_nav_selection(
    app: &AppState,
    ui: &mut UiState,
    prev_row: Option<NavRow>,
) {
    let rows = nav_pane::nav_rows_cached(app, ui);
    if rows.is_empty() {
        ui.nav_selected = 0;
        return;
    }
    if let Some(prev) = prev_row {
        let matched = match prev {
            NavRow::Chat { conv_key, .. } => rows.iter().position(
                |r| matches!(r, NavRow::Chat { conv_key: k, .. } if k == &conv_key),
            ),
            NavRow::TeamChannel { conv_key, .. } => rows.iter().position(
                |r| matches!(r, NavRow::TeamChannel { conv_key: k, .. } if k == &conv_key),
            ),
            NavRow::TeamHeader { team_id, .. } => rows.iter().position(
                |r| matches!(r, NavRow::TeamHeader { team_id: t, .. } if t == &team_id),
            ),
            NavRow::ActivityMention {
                conv_key, msg_id, ..
            } => rows.iter().position(|r| {
                matches!(
                    r,
                    NavRow::ActivityMention { conv_key: k, msg_id: m, .. }
                        if k == &conv_key && m == &msg_id
                )
            }),
            NavRow::SectionHeader { .. } => None,
        };
        if let Some(idx) = matched {
            ui.nav_selected = idx;
            return;
        }
    }
    let clamped = ui.nav_selected.min(rows.len() - 1);
    if rows[clamped].is_selectable() {
        ui.nav_selected = clamped;
    } else if let Some(fwd) = rows[clamped..].iter().position(NavRow::is_selectable) {
        ui.nav_selected = clamped + fwd;
    } else if let Some(bwd) = rows[..clamped].iter().rposition(NavRow::is_selectable) {
        ui.nav_selected = bwd;
    } else {
        ui.nav_selected = 0;
    }
}

pub(super) fn cmp_message_order(a: &MessageItem, b: &MessageItem) -> std::cmp::Ordering {
    a.created_at
        .cmp(&b.created_at)
        .then_with(|| a.msg_id.cmp(&b.msg_id))
}

pub(super) fn find_incremental_insert_pos(target: &[MessageItem], msg: &MessageItem) -> usize {
    let mut insert_pos = target.len();
    while insert_pos > 0 {
        let prev = &target[insert_pos - 1];
        if prev.msg_id.starts_with("optimistic-")
            || cmp_message_order(msg, prev) == std::cmp::Ordering::Less
        {
            insert_pos -= 1;
        } else {
            break;
        }
    }
    insert_pos
}

pub(super) fn apply_changed_messages_incremental(
    target: &mut Vec<MessageItem>,
    incoming: Vec<MessageItem>,
    oldest_reached: bool,
    selected_idx: &mut Option<usize>,
) {
    if incoming.is_empty() {
        return;
    }

    let mut has_full_window = target.len() >= 80
        && target
            .iter()
            .filter(|m| !m.msg_id.starts_with("optimistic-"))
            .take(80)
            .count()
            >= 80;
    let mut earliest_loaded: Option<(String, String)> = target
        .iter()
        .find(|m| !m.msg_id.starts_with("optimistic-"))
        .map(|m| (m.created_at.clone(), m.msg_id.clone()));

    for msg in incoming {
        if msg.parent_msg_id.is_some() {
            continue;
        }
        if let Some(pos) = target.iter().rposition(|m| m.msg_id == msg.msg_id) {
            if target[pos].created_at == msg.created_at {
                target[pos] = msg;
                continue;
            }
            let was_selected = *selected_idx == Some(pos);
            target.remove(pos);
            if let Some(sel) = *selected_idx {
                if was_selected {
                    *selected_idx = None;
                } else if sel > pos {
                    *selected_idx = Some(sel - 1);
                }
            }
            let insert_pos = find_incremental_insert_pos(target, &msg);
            target.insert(insert_pos, msg);
            if was_selected {
                *selected_idx = Some(insert_pos);
            } else if let Some(sel) = *selected_idx {
                if insert_pos <= sel {
                    *selected_idx = Some(sel + 1);
                }
            }
            earliest_loaded = target
                .iter()
                .find(|m| !m.msg_id.starts_with("optimistic-"))
                .map(|m| (m.created_at.clone(), m.msg_id.clone()));
            continue;
        }

        if !oldest_reached && has_full_window {
            if let Some((ref min_ts, ref min_id)) = earliest_loaded {
                if (msg.created_at.as_str(), msg.msg_id.as_str())
                    < (min_ts.as_str(), min_id.as_str())
                {
                    continue;
                }
            }
        }

        let insert_pos = find_incremental_insert_pos(target, &msg);
        let key = (msg.created_at.clone(), msg.msg_id.clone());
        target.insert(insert_pos, msg);
        if !has_full_window && target.len() >= 80 {
            has_full_window = target
                .iter()
                .filter(|m| !m.msg_id.starts_with("optimistic-"))
                .take(80)
                .count()
                >= 80;
        }
        if let Some(sel) = *selected_idx {
            if insert_pos <= sel {
                *selected_idx = Some(sel + 1);
            }
        }
        let is_earlier = earliest_loaded
            .as_ref()
            .is_none_or(|(min_ts, min_id)| key.0 < *min_ts || (key.0 == *min_ts && key.1 < *min_id));
        if is_earlier {
            earliest_loaded = Some(key);
        }
    }
}

pub(super) fn merge_optimistic_messages(target: &mut Vec<MessageItem>, incoming: Vec<MessageItem>) {
    let incoming_ids: HashSet<String> = incoming.iter().map(|m| m.msg_id.clone()).collect();
    let earliest_incoming_ts = incoming.first().map(|m| m.created_at.as_str());

    let mut older_preserved: Vec<MessageItem> = target
        .iter()
        .filter(|m| {
            !m.msg_id.starts_with("optimistic-")
                && !incoming_ids.contains(&m.msg_id)
                && earliest_incoming_ts
                    .is_some_and(|ts| m.created_at.as_str() <= ts)
        })
        .cloned()
        .collect();

    let pending: Vec<MessageItem> = target
        .iter()
        .filter(|m| m.msg_id.starts_with("optimistic-"))
        .cloned()
        .collect();

    older_preserved.extend(incoming);
    older_preserved.extend(pending);
    *target = older_preserved;
}
