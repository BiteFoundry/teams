//! Core sync engine event handling (`CoreEvent`) and desktop notifications.

use chrono::Utc;

use crate::models::{ConversationKind, ConversationTarget, CoreEvent, NotificationCandidate};

use super::super::nav_pane;
use super::super::notification;
use super::helpers::{
    apply_changed_messages_incremental, clamp_chat_selection, clamp_thread_selection,
    merge_optimistic_messages, preserve_nav_selection,
};
use super::{AppState, UiState};

impl AppState {
    pub fn handle_core_event(&mut self, ui: &mut UiState, event: CoreEvent) {
        match event {
            CoreEvent::SyncStateChanged { busy, summary } => {
                if summary == crate::models::POLL_RETRY_SUMMARY {
                    ui.poll_retrying = true;
                    self.syncing = false;
                } else {
                    if !busy && summary == "Synced" {
                        ui.poll_retrying = false;
                    }
                    self.syncing = busy;
                    self.sync_summary = summary;
                }
            }
            CoreEvent::SyncError(err) => {
                ui.error(format!("Sync error: {err}"));
            }
            CoreEvent::ConversationsUpdated(mut convs) => {
                ui.poll_retrying = false;
                let prev_row = nav_pane::nav_row_at(self, ui, ui.nav_selected);
                self.apply_active_read_guard(&mut convs);
                self.conversations = convs;
                preserve_nav_selection(self, ui, prev_row);
                if self.daemon_attached
                    && self.active_conv_key.is_none()
                    && self.active_target.is_none()
                {
                    if let Some(first) = self
                        .conversations
                        .iter()
                        .find(|c| c.kind != ConversationKind::Channel)
                        .or_else(|| self.conversations.first())
                        .cloned()
                    {
                        self.select_conversation_internal(ui, first.target, true, false);
                    }
                }
            }
            CoreEvent::TeamsUpdated(teams) => {
                // Fewer teams or channels shorten the Teams tab: keep the selected row when it
                // still exists, otherwise clamp so the selection never points past the list.
                let prev_row = nav_pane::nav_row_at(self, ui, ui.nav_selected);
                self.teams = teams;
                preserve_nav_selection(self, ui, prev_row);
            }
            CoreEvent::MessagesUpdated {
                conv_key,
                parent_msg_id,
                messages,
                notify_candidate,
                changed_msg_ids,
            } => {
                ui.poll_retrying = false;
                let mut fetched_mentions_or_deleted = false;
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if parent_msg_id.is_none() {
                        self.loading_messages = false;
                    }
                    self.tail_message_updated = false;
                    let prev_len = self.messages.len();
                    let prev_last_id = self.messages.last().map(|m| m.msg_id.clone());
                    let is_channel = self
                        .active_target
                        .as_ref()
                        .map(ConversationTarget::is_channel)
                        .or_else(|| {
                            self.target_for_conv_key(&conv_key).map(|t| t.is_channel())
                        });
                    let changed_rows = changed_msg_ids
                        .as_deref()
                        .filter(|_| parent_msg_id.is_none() && self.active_thread.is_none())
                        .and_then(|ids| {
                            if is_channel == Some(false)
                                && self
                                    .messages
                                    .iter()
                                    .any(|m| !m.msg_id.starts_with("optimistic-"))
                            {
                                return if ids.is_empty() {
                                    Some(Vec::new())
                                } else {
                                    self.storage.get_messages_by_ids(&conv_key, ids).ok()
                                };
                            }

                            let [id] = ids else { return None };
                            if is_channel != Some(true) || id.starts_with("optimistic-") {
                                return None;
                            }
                            // Replies and tombstones can change a different row's reply count.
                            if !messages.iter().any(|m| {
                                &m.msg_id == id && m.parent_msg_id.is_none() && !m.deleted
                            }) {
                                return None;
                            }
                            let root = self.messages.iter().find(|m| {
                                &m.msg_id == id && m.parent_msg_id.is_none() && !m.deleted
                            })?;
                            let rows = self.storage.get_messages_by_ids(&conv_key, ids).ok()?;
                            if matches!(rows.as_slice(), [row]
                                if row.parent_msg_id.is_none() && !row.deleted
                                    && row.created_at == root.created_at)
                            {
                                Some(rows)
                            } else {
                                None
                            }
                        });
                    if let Some(changed_rows) = changed_rows {
                        fetched_mentions_or_deleted = changed_rows
                            .iter()
                            .any(|m| !m.mentions.is_empty() || m.deleted);
                        self.tail_message_updated = matches!(changed_msg_ids.as_deref(), Some([_]))
                            && matches!(changed_rows.as_slice(), [row]
                                if row.parent_msg_id.is_none() && !row.deleted
                                    && self.messages.last().is_some_and(|last|
                                        last.msg_id == row.msg_id
                                            && last.created_at == row.created_at
                                            && !last.deleted));
                        apply_changed_messages_incremental(
                            &mut self.messages,
                            changed_rows,
                            self.oldest_reached,
                            &mut ui.chat_scroll.selected_idx,
                        );
                    } else {
                        let window = self.messages.len().max(80);
                        if let Ok(msgs) = self.storage.list_root_messages(&conv_key, window, None) {
                            merge_optimistic_messages(&mut self.messages, msgs);
                        }
                    }
                    clamp_chat_selection(self.messages.len(), &mut ui.chat_scroll.selected_idx);

                    let appended_at_bottom = self.messages.len() > prev_len
                        || self.messages.last().map(|m| m.msg_id.as_str())
                            != prev_last_id.as_deref();
                    if appended_at_bottom {
                        if !ui.chat_scroll.follow_tail
                            && ui.chat_scroll.scroll_up > 0
                            && ui.chat_scroll.total_lines > 0
                            && ui.pending_prepend_anchor.is_none()
                            && ui.pending_append_anchor.is_none()
                        {
                            ui.pending_append_anchor = Some(ui.chat_scroll.offset);
                        }
                        if self
                            .messages
                            .last()
                            .is_some_and(|m| !m.is_me && !m.deleted)
                        {
                            let read_ts = self
                                .messages
                                .last()
                                .map(|m| m.created_at.clone())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| Utc::now().to_rfc3339());
                            let now_ts = Utc::now().to_rfc3339();
                            let effective_read = if now_ts >= read_ts { now_ts } else { read_ts };
                            self.active_marked_read_at =
                                Some((conv_key.clone(), effective_read.clone()));
                            if let Some(conv) =
                                self.conversations.iter_mut().find(|c| c.conv_key == conv_key)
                            {
                                conv.unread_count = 0;
                                conv.last_read_at = Some(effective_read.clone());
                            }
                            if tokio::runtime::Handle::try_current().is_ok() {
                                let mark_storage = self.storage.clone();
                                let mark_key = conv_key.clone();
                                let graph = self.graph.clone();
                                let target = self.active_target.clone();
                                tokio::spawn(async move {
                                    let _ = tokio::task::spawn_blocking(move || {
                                        let _ = mark_storage
                                            .mark_conversation_read(&mark_key, &effective_read);
                                    })
                                    .await;
                                    if let Some(ConversationTarget::Chat { chat_id }) = target {
                                        let _ = graph.mark_chat_read(&chat_id).await;
                                    }
                                });
                            }
                        }
                    }
                    if let Some(root_id) =
                        self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
                    {
                        if let Ok(Some(mut thread)) =
                            self.storage.get_thread(&conv_key, &root_id, 200)
                        {
                            if let Some(existing) = self.active_thread.as_mut() {
                                merge_optimistic_messages(
                                    &mut existing.replies,
                                    std::mem::take(&mut thread.replies),
                                );
                                thread.replies = std::mem::take(&mut existing.replies);
                            }
                            self.active_thread = Some(thread);
                            clamp_thread_selection(
                                self.active_thread.as_ref(),
                                &mut ui.thread_scroll.selected_idx,
                            );
                        }
                    }
                }
                let mentions_may_have_changed = if changed_msg_ids.as_deref() == Some(&[]) {
                    false
                } else {
                    let touches_existing_mention = !self.activity_mentions.is_empty()
                        && (changed_msg_ids.as_ref().is_some_and(|ids| {
                            self.activity_mentions.iter().any(|am| {
                                am.conv_key == conv_key
                                    && ids.iter().any(|id| id == &am.msg_id)
                            })
                        }) || messages.iter().any(|m| {
                            self.activity_mentions
                                .iter()
                                .any(|am| am.conv_key == conv_key && am.msg_id == m.msg_id)
                        }));
                    fetched_mentions_or_deleted
                        || touches_existing_mention
                        || messages.iter().any(|m| !m.mentions.is_empty() || m.deleted)
                };
                if !self.my_user_id.is_empty() && mentions_may_have_changed {
                    if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
                    {
                        self.activity_mentions = mentions;
                    }
                }
                if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == conv_key) {
                    if let Some(newest_in_event) = messages.iter().max_by(|a, b| {
                        a.created_at
                            .cmp(&b.created_at)
                            .then_with(|| a.msg_id.cmp(&b.msg_id))
                    }) {
                        let matches_last =
                            conv.last_msg_id.as_deref() == Some(newest_in_event.msg_id.as_str());
                        let is_newer = !newest_in_event.created_at.is_empty()
                            && conv
                                .last_updated_at
                                .as_deref()
                                .is_none_or(|ts| newest_in_event.created_at.as_str() > ts);
                        if matches_last || is_newer {
                            conv.last_msg_id = Some(newest_in_event.msg_id.clone());
                            if !(newest_in_event.deleted && newest_in_event.author_name == "Unknown")
                            {
                                conv.last_msg_author = Some(newest_in_event.author_name.clone());
                            }
                            conv.last_msg_preview = Some(if newest_in_event.deleted {
                                "(deleted message)".to_string()
                            } else {
                                newest_in_event
                                    .body_plain
                                    .split_whitespace()
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            });
                            if is_newer {
                                conv.last_updated_at = Some(newest_in_event.created_at.clone());
                            }
                        }
                    }
                }
                if let Some(candidate) = notify_candidate {
                    self.maybe_notify(ui, &candidate);
                }
            }
            CoreEvent::PresenceUpdated(list) => {
                for p in list {
                    if p.user_id == self.my_user_id {
                        self.my_presence = Some(p.clone());
                    }
                    self.presences.insert(p.user_id.clone(), p);
                }
            }
        }
    }

    fn maybe_notify(&self, ui: &mut UiState, candidate: &NotificationCandidate) {
        let is_active = self.active_conv_key.as_deref() == Some(&candidate.conv_key);
        let dnd = self
            .my_presence
            .as_ref()
            .is_some_and(|p| matches!(p.availability.as_str(), "DoNotDisturb"));
        let is_self = !self.my_user_id.is_empty()
            && self
                .my_profile
                .as_ref()
                .is_some_and(|u| u.display_name == candidate.sender);
        if dnd || !notification::should_notify(ui.notify, is_active, is_self) {
            return;
        }
        let body = notification::chat_body(&candidate.sender, &candidate.preview);
        notification::show(&candidate.conv_title, &body);
    }
}
