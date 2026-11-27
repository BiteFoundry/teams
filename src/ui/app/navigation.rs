//! Conversation navigation, history stack, draft persistence, prefetching, and thread drawers.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::config;
use crate::models::{ConversationKind, ConversationTarget, MessageItem, SyncCursor, ThreadView};
use crate::sync;

use super::super::composer::ReplyPreview;
use super::super::nav_pane::{self, NavRow};
use super::{AppState, Focus, NarrowPane, ScrollState, UiMsg, UiState};

impl AppState {
    pub fn current_draft_key(&self, ui: &UiState) -> Option<String> {
        let conv_key = self.active_conv_key.as_deref()?;
        if ui.composer_target_thread {
            if let Some(thread) = &self.active_thread {
                return Some(format!("{conv_key}#thread:{}", thread.root.msg_id));
            }
        }
        Some(conv_key.to_string())
    }

    pub(super) fn persist_drafts(&self) {
        let seq = self.draft_save_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if tokio::runtime::Handle::try_current().is_ok() {
            let snapshot = self.drafts.clone();
            let seq_counter = Arc::clone(&self.draft_save_seq);
            let save_lock = Arc::clone(&self.draft_save_lock);
            tokio::task::spawn_blocking(move || {
                let _guard = save_lock.lock();
                if seq_counter.load(Ordering::SeqCst) != seq {
                    return;
                }
                let _ = config::save_drafts(&snapshot);
            });
        } else {
            let _guard = self.draft_save_lock.lock();
            let _ = config::save_drafts(&self.drafts);
        }
    }

    /// Updates the in-memory draft map from the composer and returns `true` if `self.drafts` changed.
    pub(super) fn stash_draft_in_memory(&mut self, ui: &UiState) -> bool {
        let Some(key) = self.current_draft_key(ui) else {
            return false;
        };
        let text = ui.composer.text().trim_end_matches('\n').to_string();
        if text.is_empty() {
            self.drafts.remove(&key).is_some()
        } else if self.drafts.get(&key) != Some(&text) {
            self.drafts.insert(key, text);
            true
        } else {
            false
        }
    }

    pub fn stash_draft(&mut self, ui: &UiState) {
        if self.stash_draft_in_memory(ui) {
            self.persist_drafts();
        }
    }

    pub fn stash_and_reset_draft(&mut self, ui: &mut UiState) {
        self.stash_draft(ui);
        ui.composer.reply = None;
        ui.composer.subject.clear();
        ui.composer.subject_cursor = 0;
        ui.composer.subject_focused = false;
        ui.composer.clear_pending_images();
        ui.composer.popup.close();
    }

    pub fn restore_draft(&self, ui: &mut UiState) {
        let key = self.current_draft_key(ui);
        let text = key
            .as_deref()
            .and_then(|k| self.drafts.get(k))
            .map(String::as_str)
            .unwrap_or("");
        ui.composer.load_text(text);
    }

    pub fn clear_current_draft(&mut self, ui: &mut UiState) {
        if let Some(key) = self.current_draft_key(ui) {
            if self.drafts.remove(&key).is_some() {
                self.persist_drafts();
            }
        }
        ui.composer.reset_after_send();
    }

    pub fn load_cached_teams(&mut self) {
        if let Ok(convs) = self.storage.list_conversations() {
            self.conversations = convs;
            self.bump_conversations_gen();
        }
        if let Ok(teams) = self.storage.list_teams() {
            self.teams = teams;
            self.bump_teams_gen();
        }
        if let Ok(presences) = self.storage.get_presences() {
            self.presences = presences;
            self.bump_presences_gen();
        }
        if !self.my_user_id.is_empty() {
            if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50) {
                self.activity_mentions = mentions;
                self.bump_mentions_gen();
            }
        }
        self.chats_oldest_reached = self
            .storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .ok()
            .flatten()
            .is_some_and(|c| c.is_history_eof());
    }

    pub fn load_cached_initial(&mut self) {
        self.load_cached_teams();
    }

    pub fn ensure_tab_domain_activated(&mut self, _ui: &mut UiState, _tab: usize) {}

    pub fn select_initial_cached_conversation(&mut self, ui: &mut UiState) {
        if self.active_conv_key.is_some() || self.active_target.is_some() {
            return;
        }
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

    pub fn select_conversation(&mut self, ui: &mut UiState, target: ConversationTarget) {
        self.select_conversation_internal(ui, target, true, true);
    }

    pub fn history_back(&mut self, ui: &mut UiState) {
        if ui.conv_history_pos == 0 || ui.conv_history.is_empty() {
            return;
        }
        ui.conv_history_pos -= 1;
        let key = ui.conv_history[ui.conv_history_pos].clone();
        if let Some(target) = self.target_for_conv_key(&key) {
            self.select_conversation_internal(ui, target, false, true);
        }
    }

    pub fn history_forward(&mut self, ui: &mut UiState) {
        if ui.conv_history_pos + 1 >= ui.conv_history.len() {
            return;
        }
        ui.conv_history_pos += 1;
        let key = ui.conv_history[ui.conv_history_pos].clone();
        if let Some(target) = self.target_for_conv_key(&key) {
            self.select_conversation_internal(ui, target, false, true);
        }
    }

    pub fn target_for_conv_key(&self, conv_key: &str) -> Option<ConversationTarget> {
        self.conversations
            .iter()
            .find(|c| c.conv_key == conv_key)
            .map(|c| c.target.clone())
            .or_else(|| ConversationTarget::from_conv_key(conv_key))
    }

    pub fn maybe_autoload_older_on_scroll_top(&mut self, ui: &mut UiState) {
        let height = ui.chat_scroll.area.height as usize;
        let near_or_at_top = if ui.chat_scroll.total_lines > 0 && height > 0 {
            let max_up = ui.chat_scroll.total_lines.saturating_sub(height);
            ui.chat_scroll.offset(height) <= 1 || ui.chat_scroll.scroll_up as usize >= max_up
        } else {
            ui.chat_scroll.offset == 0
        };
        if near_or_at_top
            && !self.oldest_reached
            && !self.loading_older
            && self.active_conv_key.is_some()
        {
            self.load_older_auto(ui);
        }
    }

    pub(super) fn select_conversation_internal(
        &mut self,
        ui: &mut UiState,
        target: ConversationTarget,
        record_history: bool,
        allow_prefetch: bool,
    ) {
        let new_key = target.conv_key();
        if self.active_conv_key.as_deref() == Some(&new_key) {
            self.focus_conversation_pane(ui);
            return;
        }

        let prev_key = self.active_conv_key.clone();
        self.stash_and_reset_draft(ui);

        if let Some(prev) = prev_key {
            ui.last_conv_key = Some(prev);
        }
        if record_history {
            if !ui.conv_history.is_empty() {
                let keep = (ui.conv_history_pos + 1).min(ui.conv_history.len());
                ui.conv_history.truncate(keep);
            }
            if ui.conv_history.last().map(String::as_str) != Some(new_key.as_str()) {
                ui.conv_history.push(new_key.clone());
            }
            if ui.conv_history.len() > 64 {
                let excess = ui.conv_history.len() - 64;
                ui.conv_history.drain(..excess);
            }
            ui.conv_history_pos = ui.conv_history.len().saturating_sub(1);
        }

        let (prev_unread, prev_last_read_at) = self
            .conversations
            .iter()
            .find(|c| c.conv_key == new_key)
            .map(|c| (c.unread_count, c.last_read_at.clone()))
            .unwrap_or((0, None));
        ui.unread_anchor = if prev_unread > 0 {
            prev_last_read_at.map(|ts| (new_key.clone(), ts))
        } else {
            None
        };

        self.active_conv_key = Some(new_key.clone());
        self.active_target = Some(target.clone());
        self.tail_message_updated = false;
        self.active_thread = None;
        self.bump_thread_gen();
        self.pending_thread = None;
        self.pending_thread_reply_id = None;
        self.conv_req_seq = self.conv_req_seq.wrapping_add(1);
        self.older_req_seq = self.older_req_seq.wrapping_add(1);
        self.thread_req_seq = self.thread_req_seq.wrapping_add(1);
        self.clipboard_paste_seq = self.clipboard_paste_seq.wrapping_add(1);
        self.html_fetch_seq = self.html_fetch_seq.wrapping_add(1);
        let conv_req_seq = self.conv_req_seq;
        self.loading_thread = false;
        self.loading_older = false;
        self.history_error = None;
        ui.thread_drawer_open = false;
        ui.composer_target_thread = false;
        ui.show_subject = false;
        ui.chat_scroll = ScrollState::default();
        ui.thread_scroll = ScrollState::default();
        ui.pending_prepend_anchor = None;
        ui.pending_append_anchor = None;
        ui.pending_reveal_selection = false;
        ui.pending_thread_reveal = false;

        self.messages = self
            .storage
            .list_root_messages(&new_key, 80, None)
            .unwrap_or_default();
        self.bump_messages_gen();
        self.bump_thread_gen();
        // Decided after the window is loaded: an EOF cursor only says Graph has no older page,
        // while the cache can hold more rows than the 80 shown.
        self.oldest_reached = self.cached_history_exhausted(&new_key);
        if prev_unread > 0 && ui.unread_anchor.is_none() {
            let unread_n = prev_unread as usize;
            if self.messages.len() > unread_n {
                let anchor_idx = self.messages.len() - unread_n - 1;
                ui.unread_anchor = Some((
                    new_key.clone(),
                    self.messages[anchor_idx].created_at.clone(),
                ));
            }
        }
        self.members = self.storage.list_members(&new_key).unwrap_or_default();
        self.restore_draft(ui);
        self.focus_conversation_pane(ui);

        let now_ts = Utc::now().to_rfc3339();
        self.active_marked_read_at = Some((new_key.clone(), now_ts.clone()));
        if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == new_key) {
            conv.unread_count = 0;
            conv.last_read_at = Some(now_ts.clone());
            self.bump_conversations_gen();
        }

        self.sync_ctrl.set_focus(Some(target.clone()), None);

        const MEMBER_CACHE_TTL: Duration = Duration::from_secs(300);
        let need_messages_fetch = self.messages.is_empty();
        // Cached channel roots may come only from delta sync, which never carries replies, so
        // their `↳ N replies` pills stay hidden. Refresh one `$expand=replies` page in the
        // background on the first open this session; a cold open fetches that page anyway.
        let need_reply_hydration = target.is_channel()
            && self.reply_hydrated_convs.insert(new_key.clone())
            && !need_messages_fetch;
        let need_members_fetch = self.members.is_empty()
            || self
                .member_fetched_at
                .get(&new_key)
                .is_none_or(|t| t.elapsed() >= MEMBER_CACHE_TTL);
        self.loading_messages = need_messages_fetch;

        self.prefetched_convs.insert(new_key.clone());

        if tokio::runtime::Handle::try_current().is_err() {
            self.loading_messages = false;
            return;
        }

        if need_members_fetch {
            self.member_fetched_at.insert(new_key.clone(), Instant::now());
        }

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let conv_key = new_key.clone();
        tokio::spawn(async move {
            let mark_storage = storage.clone();
            let mark_conv_key = conv_key.clone();
            let mark_read_fut = tokio::task::spawn_blocking(move || {
                let _ = mark_storage.mark_conversation_read(&mark_conv_key, &now_ts);
            });
            let chat_read_fut = async {
                if let ConversationTarget::Chat { ref chat_id } = target {
                    let _ = graph.mark_chat_read(chat_id).await;
                }
            };
            let msgs_fut = async {
                if need_messages_fetch || need_reply_hydration {
                    // Fetch a single Graph page (`$top=50`, Graph's max page size) instead of
                    // requesting 60 messages across 2 sequential round-trips.
                    Some(graph.list_messages_page(&target, 50, None).await)
                } else {
                    None
                }
            };
            let members_fut = async {
                if need_members_fetch {
                    Some(graph.list_members(&target).await)
                } else {
                    None
                }
            };
            let (msgs_res_opt, members_res_opt, _, _) =
                tokio::join!(msgs_fut, members_fut, mark_read_fut, chat_read_fut);

            let fetched_page = msgs_res_opt.and_then(Result::ok);
            let fetched_members = members_res_opt.and_then(Result::ok);

            if fetched_page.is_some() || fetched_members.is_some() {
                let db_conv_key = conv_key.clone();
                let had_msgs = fetched_page.is_some();
                let (messages, members) = tokio::task::spawn_blocking(move || {
                    if let Some((ref msgs, next_link)) = fetched_page {
                        if !msgs.is_empty() {
                            let _ = storage.upsert_messages(msgs);
                        }
                        let _ = sync::save_initial_history_cursor_if_absent(
                            &storage,
                            &db_conv_key,
                            msgs,
                            next_link,
                        );
                    }
                    if let Some(ref members) = fetched_members {
                        let _ = storage.replace_members(&db_conv_key, members);
                    }
                    let messages = if had_msgs {
                        Some(
                            storage
                                .list_root_messages(&db_conv_key, 80, None)
                                .unwrap_or_default(),
                        )
                    } else {
                        None
                    };
                    let members = storage.list_members(&db_conv_key).unwrap_or_default();
                    (messages, members)
                })
                .await
                .unwrap_or_default();

                if let Some(messages) = messages {
                    let _ = tx.send(UiMsg::MessagesLoaded {
                        req_seq: conv_req_seq,
                        conv_key,
                        messages,
                        members,
                    });
                } else {
                    let _ = tx.send(UiMsg::MembersLoaded {
                        req_seq: conv_req_seq,
                        conv_key,
                        members,
                        clear_loading: need_messages_fetch,
                    });
                }
            } else if need_messages_fetch {
                let _ = tx.send(UiMsg::MembersLoaded {
                    req_seq: conv_req_seq,
                    conv_key,
                    members: Vec::new(),
                    clear_loading: true,
                });
            }
        });

        if allow_prefetch {
            self.maybe_prefetch_neighbors(ui, &new_key);
        }
    }

    /// Bounded, rate-limited background prefetch of up to 2 uncached adjacent conversations in the
    /// visible nav pane. Triggered ONLY on interactive conversation navigation (never on startup
    /// auto-selection or periodic polling ticks) so automated polling budgets stay untouched.
    fn maybe_prefetch_neighbors(&mut self, ui: &UiState, active_key: &str) {
        const MAX_PREFETCH_NEIGHBORS: usize = 2;
        const PREFETCH_COOLDOWN: Duration = Duration::from_secs(3);

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if self
            .last_prefetch_at
            .is_some_and(|t| t.elapsed() < PREFETCH_COOLDOWN)
        {
            return;
        }

        let rows = nav_pane::nav_rows_cached(self, ui);
        let mut visible_keys: Vec<String> = Vec::new();
        let mut seen_visible: HashSet<String> = HashSet::new();
        for row in rows.iter() {
            let key = match row {
                NavRow::Chat { conv_key, .. } | NavRow::TeamChannel { conv_key, .. } => conv_key,
                _ => continue,
            };
            if seen_visible.insert(key.clone()) {
                visible_keys.push(key.clone());
            }
        }
        let Some(pos) = visible_keys.iter().position(|k| k == active_key) else {
            return;
        };

        let mut targets: Vec<(String, ConversationTarget)> =
            Vec::with_capacity(MAX_PREFETCH_NEIGHBORS);
        for delta in [1isize, -1, 2, -2] {
            if targets.len() >= MAX_PREFETCH_NEIGHBORS {
                break;
            }
            let idx = pos as isize + delta;
            if idx < 0 || (idx as usize) >= visible_keys.len() {
                continue;
            }
            let neighbor_key = &visible_keys[idx as usize];
            if neighbor_key == active_key || self.prefetched_convs.contains(neighbor_key) {
                continue;
            }
            if let Some(target) = self.target_for_conv_key(neighbor_key) {
                targets.push((neighbor_key.clone(), target));
            }
        }
        if targets.is_empty() {
            return;
        }

        self.last_prefetch_at = Some(Instant::now());
        for (key, _) in &targets {
            self.prefetched_convs.insert(key.clone());
        }

        let sem = Arc::clone(&self.prefetch_sem);
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        tokio::spawn(async move {
            let Ok(_permit) = sem.try_acquire_owned() else {
                return;
            };
            let check_storage = storage.clone();
            let uncached: Vec<(String, ConversationTarget)> = tokio::task::spawn_blocking(move || {
                targets
                    .into_iter()
                    .filter(|(key, _)| {
                        check_storage
                            .list_root_messages(key, 1, None)
                            .map(|m| m.is_empty())
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();

            if uncached.is_empty() {
                return;
            }

            let mut chat_ids: Vec<String> = Vec::new();
            let mut channel_targets: Vec<(String, ConversationTarget)> = Vec::new();
            for (key, target) in uncached {
                match target {
                    ConversationTarget::Chat { chat_id } => chat_ids.push(chat_id),
                    ConversationTarget::Channel { .. } => channel_targets.push((key, target)),
                }
            }

            let chats_fut = async {
                if chat_ids.is_empty() {
                    return;
                }
                if let Ok(results) = graph.batch_fetch_chat_messages(&chat_ids, 20).await {
                    let all_msgs: Vec<MessageItem> = results
                        .into_iter()
                        .filter_map(|(_, msgs_opt)| msgs_opt)
                        .flatten()
                        .collect();
                    if !all_msgs.is_empty() {
                        let st = storage.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = st.upsert_messages(&all_msgs);
                        })
                        .await;
                    }
                }
            };

            let channels_fut = async {
                if channel_targets.is_empty() {
                    return;
                }
                let futs = channel_targets.into_iter().map(|(conv_key, target)| {
                    let graph = graph.clone();
                    let storage = storage.clone();
                    async move {
                        if let Ok((msgs, next_link)) =
                            graph.list_messages_page(&target, 50, None).await
                        {
                            let _ = tokio::task::spawn_blocking(move || {
                                if !msgs.is_empty() {
                                    let _ = storage.upsert_messages(&msgs);
                                }
                                let _ = sync::save_initial_history_cursor_if_absent(
                                    &storage,
                                    &conv_key,
                                    &msgs,
                                    next_link,
                                );
                            })
                            .await;
                        }
                    }
                });
                futures_util::future::join_all(futs).await;
            };

            tokio::join!(chats_fut, channels_fut);
        });
    }

    fn focus_conversation_pane(&self, ui: &mut UiState) {
        ui.narrow_pane = NarrowPane::Chat;
        ui.set_focus(Focus::Composer);
    }

    pub fn select_nav_row(&mut self, ui: &mut UiState, idx: usize) {
        let Some(row) = nav_pane::nav_row_at(self, ui, idx) else {
            return;
        };
        ui.nav_selected = idx;
        match row {
            NavRow::SectionHeader { .. } => {}
            NavRow::Chat { conv_key, .. } => {
                if let Some(target) = self
                    .conversations
                    .iter()
                    .find(|c| c.conv_key == conv_key)
                    .map(|c| c.target.clone())
                    .or_else(|| ConversationTarget::from_conv_key(&conv_key))
                {
                    self.select_conversation(ui, target);
                }
            }
            NavRow::TeamHeader { team_id, .. } => {
                if !ui.collapsed_team_ids.remove(&team_id) {
                    ui.collapsed_team_ids.insert(team_id);
                }
                ui.bump_nav_prefs_gen();
            }
            NavRow::TeamChannel { conv_key, .. } => {
                if let Some(target) = ConversationTarget::from_conv_key(&conv_key) {
                    self.select_conversation(ui, target);
                }
            }
            NavRow::ActivityMention {
                conv_key,
                msg_id,
                parent_msg_id,
                created_at,
                ..
            } => {
                if let Some(target) = ConversationTarget::from_conv_key(&conv_key) {
                    self.select_conversation(ui, target);
                    if let Some(root) = parent_msg_id {
                        self.open_thread_by_id_with_reply(ui, root, Some(msg_id));
                    } else {
                        self.reveal_message(ui, &msg_id, &created_at);
                    }
                }
            }
        }
    }

    pub fn open_thread(&mut self, ui: &mut UiState, msg_index: usize) {
        let Some(msg) = self.messages.get(msg_index).cloned() else {
            return;
        };
        let is_channel = self
            .active_target
            .as_ref()
            .is_some_and(ConversationTarget::is_channel);
        if !is_channel {
            ui.composer.set_reply(ReplyPreview {
                msg_id: msg.msg_id.clone(),
                author: msg.author_name.clone(),
                text: msg.body_plain.chars().take(80).collect(),
            });
            ui.set_focus(Focus::Composer);
            return;
        }
        let root_id = msg.parent_msg_id.unwrap_or(msg.msg_id);
        self.open_thread_by_id(ui, root_id);
    }

    pub fn open_thread_by_id(&mut self, ui: &mut UiState, root_msg_id: String) {
        self.open_thread_by_id_with_reply(ui, root_msg_id, None);
    }

    pub fn open_thread_by_id_with_reply(
        &mut self,
        ui: &mut UiState,
        root_msg_id: String,
        target_reply_id: Option<String>,
    ) {
        let Some(ConversationTarget::Channel {
            team_id,
            channel_id,
        }) = self.active_target.clone()
        else {
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };

        self.stash_and_reset_draft(ui);
        self.thread_req_seq = self.thread_req_seq.wrapping_add(1);
        let req_seq = self.thread_req_seq;
        self.loading_thread = true;
        self.pending_thread = Some((conv_key.clone(), root_msg_id.clone()));
        self.pending_thread_reply_id = target_reply_id.clone();
        if let Ok(Some(cached)) = self.storage.get_thread(&conv_key, &root_msg_id, 200) {
            self.active_thread = Some(cached);
        } else if let Some(root) = self
            .messages
            .iter()
            .find(|m| m.msg_id == root_msg_id)
            .cloned()
        {
            self.active_thread = Some(ThreadView {
                conv_key: conv_key.clone(),
                root,
                replies: Vec::new(),
            });
        } else {
            // Nothing cached for this root: do not keep showing the previously open thread.
            self.active_thread = None;
        }
        self.bump_thread_gen();
        ui.thread_drawer_open = true;
        ui.composer_target_thread = true;
        ui.narrow_pane = NarrowPane::Thread;
        ui.thread_scroll = ScrollState::default();
        ui.pending_thread_reveal = false;
        if let (Some(ref target_id), Some(ref thread)) = (&target_reply_id, &self.active_thread) {
            if thread.root.msg_id == *target_id {
                ui.thread_scroll.selected_idx = Some(0);
                ui.pending_thread_reveal = true;
            } else if let Some(pos) = thread.replies.iter().position(|r| r.msg_id == *target_id) {
                ui.thread_scroll.selected_idx = Some(pos + 1);
                ui.pending_thread_reveal = true;
            }
        }
        self.restore_draft(ui);
        ui.set_focus(Focus::Thread);

        self.sync_ctrl
            .set_focus(self.active_target.clone(), Some(root_msg_id.clone()));

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph
                .get_thread_with_coverage(&team_id, &channel_id, &root_msg_id, 200)
                .await
            {
                Ok((thread, complete)) => {
                    let cached = thread.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.save_fetched_thread(&cached, complete);
                    })
                    .await;
                    let _ = tx.send(UiMsg::ThreadLoaded {
                        req_seq,
                        conv_key,
                        root_msg_id,
                        thread,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ThreadLoadFailed {
                        req_seq,
                        conv_key,
                        root_msg_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn refresh_active_thread(&mut self, ui: &mut UiState) {
        let Some(ConversationTarget::Channel {
            team_id,
            channel_id,
        }) = self.active_target.clone()
        else {
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let Some(root_msg_id) = self.active_thread.as_ref().map(|t| t.root.msg_id.clone()) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        self.thread_req_seq = self.thread_req_seq.wrapping_add(1);
        let req_seq = self.thread_req_seq;
        self.loading_thread = true;
        self.pending_thread = Some((conv_key.clone(), root_msg_id.clone()));
        let task_label = "Sync thread replies".to_string();
        ui.tasks.begin(task_label.clone());

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph
                .get_thread_with_coverage(&team_id, &channel_id, &root_msg_id, 200)
                .await
            {
                Ok((fetched_thread, complete)) => {
                    let db_storage = storage.clone();
                    let db_conv = conv_key.clone();
                    let db_root = root_msg_id.clone();
                    let thread_opt = tokio::task::spawn_blocking(move || {
                        let _ = db_storage.save_fetched_thread(&fetched_thread, complete);
                        db_storage
                            .get_thread(&db_conv, &db_root, 200)
                            .ok()
                            .flatten()
                            .or(Some(fetched_thread))
                    })
                    .await
                    .ok()
                    .flatten();
                    if let Some(thread) = thread_opt {
                        let _ = tx.send(UiMsg::ThreadLoaded {
                            req_seq,
                            conv_key,
                            root_msg_id,
                            thread,
                        });
                    }
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    // The failure handler reports the error and ends the loading state.
                    let _ = tx.send(UiMsg::ThreadLoadFailed {
                        req_seq,
                        conv_key,
                        root_msg_id,
                        error: err.to_string(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: None,
                    });
                }
            }
        });
    }

    pub fn close_thread(&mut self, ui: &mut UiState) {
        self.stash_and_reset_draft(ui);
        ui.thread_drawer_open = false;
        ui.composer_target_thread = false;
        self.active_thread = None;
        self.bump_thread_gen();
        self.pending_thread = None;
        self.pending_thread_reply_id = None;
        self.thread_req_seq = self.thread_req_seq.wrapping_add(1);
        self.clipboard_paste_seq = self.clipboard_paste_seq.wrapping_add(1);
        self.loading_thread = false;
        ui.pending_thread_reveal = false;
        ui.narrow_pane = NarrowPane::Chat;
        self.sync_ctrl.set_focus(self.active_target.clone(), None);
        self.restore_draft(ui);
        ui.set_focus(Focus::Chat);
    }

    pub fn reveal_message(&mut self, ui: &mut UiState, msg_id: &str, created_at: &str) {
        /// Longest cached span loaded in one jump; beyond it the user pages with `o`.
        const REVEAL_SPAN_CAP: usize = 500;
        const OUTSIDE: &str =
            "Message is outside the loaded history — press o to load older messages.";

        if let Some(idx) = self.messages.iter().position(|m| m.msg_id == msg_id) {
            ui.chat_scroll.selected_idx = Some(idx);
            ui.pending_reveal_selection = true;
            return;
        }
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let window_start = self
            .messages
            .iter()
            .find(|m| !m.msg_id.starts_with("optimistic-"))
            .map(|m| (m.created_at.clone(), m.msg_id.clone()));
        if window_start
            .as_ref()
            .is_some_and(|(ts, id)| (created_at, msg_id) >= (ts.as_str(), id.as_str()))
        {
            // Not older than the window, yet not in it: nothing the cache span can add.
            ui.info(OUTSIDE);
            return;
        }
        let span = self
            .storage
            .list_root_messages_from(&conv_key, created_at, msg_id, REVEAL_SPAN_CAP + 1)
            .unwrap_or_default();
        if span.first().map(|m| m.msg_id.as_str()) != Some(msg_id) {
            ui.info(OUTSIDE);
            return;
        }
        let older: Vec<MessageItem> = span
            .into_iter()
            .take_while(|m| window_start.as_ref().is_none_or(|(_, id)| m.msg_id != *id))
            .collect();
        if older.len() > REVEAL_SPAN_CAP {
            ui.info(OUTSIDE);
            return;
        }
        self.prepend_older_messages(ui, older);
        self.oldest_reached = self.cached_history_exhausted(&conv_key);
        if let Some(idx) = self.messages.iter().position(|m| m.msg_id == msg_id) {
            ui.chat_scroll.selected_idx = Some(idx);
            ui.pending_reveal_selection = true;
        }
    }
}
