//! Async UI message router (`handle_ui_msg`) decomposed into domain handlers.

use crate::models::{
    ConversationKind, ConversationSummary, MemberInfo, MessageItem, PresenceStatus, SearchHit,
    TeamSummary, ThreadView, UserProfile,
};

use super::super::nav_pane;
use super::super::overlays::{Overlay, OverlayKind};
use super::super::toast::ToastKind;
use super::helpers::{
    clamp_chat_selection, clamp_thread_selection, mark_message_deleted_in_memory,
    merge_optimistic_messages, preserve_nav_selection,
};
use super::{AppState, Focus, UiMsg, UiState};

impl AppState {
    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BootstrapDone {
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences,
                scope_warning,
            } => self.handle_bootstrap_done(
                ui,
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences,
                scope_warning,
            ),
            UiMsg::PeerPresencesLoaded(presences) => {
                if !presences.is_empty() {
                    self.presences.extend(presences);
                    self.bump_presences_gen();
                }
            }
            UiMsg::MessagesLoaded {
                req_seq,
                conv_key,
                messages,
                members,
            } => self.handle_messages_loaded(ui, req_seq, conv_key, messages, members),
            UiMsg::MembersLoaded {
                req_seq,
                conv_key,
                members,
                clear_loading,
            } => self.handle_members_loaded(req_seq, conv_key, members, clear_loading),
            UiMsg::ClipboardImageEncoded {
                req_seq,
                conv_key,
                composer_target_thread,
                width: _,
                height: _,
                file_name,
                png_bytes,
                preview,
            } => self.handle_clipboard_image_encoded(
                ui,
                req_seq,
                conv_key,
                composer_target_thread,
                file_name,
                png_bytes,
                preview,
            ),
            UiMsg::OlderMessagesLoaded {
                req_seq,
                conv_key,
                messages,
                reached_eof,
            } => self.handle_older_messages_loaded(ui, req_seq, conv_key, messages, reached_eof),
            UiMsg::OlderMessagesFailed {
                req_seq,
                conv_key,
                local_messages,
                error,
            } => self.handle_older_messages_failed(ui, req_seq, conv_key, local_messages, error),
            UiMsg::OlderChatsLoaded {
                conversations,
                reached_eof,
            } => self.handle_older_chats_loaded(ui, conversations, reached_eof),
            UiMsg::OlderChatsFailed => {
                self.loading_older_chats = false;
            }
            UiMsg::ThreadLoaded {
                req_seq,
                conv_key,
                root_msg_id,
                thread,
            } => self.handle_thread_loaded(ui, req_seq, conv_key, root_msg_id, thread),
            UiMsg::ThreadLoadFailed {
                req_seq,
                conv_key,
                root_msg_id,
                error,
            } => self.handle_thread_load_failed(ui, req_seq, conv_key, root_msg_id, error),
            UiMsg::MessageSent {
                conv_key,
                parent_msg_id,
                optimistic_id,
                sent,
            } => self.handle_message_sent(ui, conv_key, parent_msg_id, optimistic_id, sent),
            UiMsg::MessageSendFailed {
                conv_key,
                parent_msg_id,
                optimistic_id,
                text,
            } => self.handle_message_send_failed(ui, conv_key, parent_msg_id, optimistic_id, text),
            UiMsg::AttachmentSent { conv_key, sent } => {
                self.handle_attachment_sent(ui, conv_key, sent);
            }
            UiMsg::MessageDeleted {
                conv_key,
                msg_id,
                parent_msg_id,
            } => self.handle_message_deleted(conv_key, msg_id, parent_msg_id),
            UiMsg::ReactionUpdated {
                conv_key,
                msg_id,
                reaction_type,
                added,
            } => self.handle_reaction_updated(conv_key, msg_id, reaction_type, added),
            UiMsg::AttachmentDownloaded(_) | UiMsg::UpdateChecked(_) => {}
            UiMsg::SearchResults { query, hits } => self.handle_search_results(ui, query, hits),
            UiMsg::UserSearchResults {
                req_seq,
                query,
                users,
            } => self.handle_user_search_results(ui, req_seq, query, users),
            UiMsg::ChatCreated(conv) => {
                if !self.conversations.iter().any(|c| c.conv_key == conv.conv_key) {
                    self.conversations.insert(0, conv.clone());
                    self.bump_conversations_gen();
                }
                self.select_conversation(ui, conv.target);
            }
            UiMsg::PresenceUpdated(status) => self.handle_presence_updated(status),
            UiMsg::ImageDecoded {
                cache_key,
                rendered,
            } => self.insert_image_cache(cache_key, rendered),
            UiMsg::ImageBytesFetched { raw_key, bytes } => {
                self.insert_raw_image_cache(raw_key, bytes);
            }
            UiMsg::ImageDecodeFailed {
                cache_key,
                owner_id,
                att_id,
                error,
            } => self.handle_image_decode_failed(ui, cache_key, owner_id, att_id, error),
            UiMsg::ImageAutoRoutedToHtml {
                cache_key,
                owner_id,
                att_id,
                title,
                html,
            } => self.handle_image_auto_routed_to_html(ui, cache_key, owner_id, att_id, title, html),
            UiMsg::HtmlAttachmentLoaded {
                req_seq,
                conv_key,
                title,
                html,
                kind,
            } => self.handle_html_attachment_loaded(ui, req_seq, conv_key, title, html, kind),
            UiMsg::HtmlServed { url } => {
                if let Some(Overlay::HtmlPreview(preview)) = ui.overlays.last_mut() {
                    preview.last_served_url = Some(url.clone());
                    preview.session.last_status = Some(format!("Serving: {url}"));
                }
            }
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.handle_daemon_attached(ui, pid, sync_busy);
            }
            UiMsg::DaemonDetached => self.handle_daemon_detached(),
            UiMsg::StartLocalBootstrap => {
                self.daemon_attached = false;
                self.daemon_pid = None;
                self.spawn_bootstrap(ui);
            }
            UiMsg::ProfileLoaded {
                me,
                presence,
                scope_warning,
            } => self.handle_profile_loaded(ui, me, presence, scope_warning),
            UiMsg::TaskDone {
                label,
                error,
                toast,
            } => self.handle_task_done(ui, label, error, toast),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_bootstrap_done(
        &mut self,
        ui: &mut UiState,
        me: Option<UserProfile>,
        presence: Option<PresenceStatus>,
        mut conversations: Vec<ConversationSummary>,
        teams: Vec<TeamSummary>,
        mentions: Vec<MessageItem>,
        presences: std::collections::HashMap<String, PresenceStatus>,
        scope_warning: Option<String>,
    ) {
        if let Some(u) = me {
            self.my_user_id = u.id.clone();
            self.my_profile = Some(u);
        }
        if let Some(p) = presence {
            self.my_presence = Some(p);
        }
        let prev_row = nav_pane::nav_row_at(self, ui, ui.nav_selected);
        self.apply_active_read_guard(&mut conversations);
        self.conversations = conversations;
        self.teams = teams;
        self.activity_mentions = mentions;
        self.presences.extend(presences);
        self.bump_conversations_gen();
        self.bump_teams_gen();
        self.bump_mentions_gen();
        self.bump_presences_gen();
        preserve_nav_selection(self, ui, prev_row);
        self.syncing = false;
        self.sync_summary = "Synced".to_string();
        self.emit_scope_warning_once(ui, scope_warning);

        if self.active_conv_key.is_none() {
            if let Some(first) = self.conversations.first().cloned() {
                self.select_conversation_internal(ui, first.target, true, false);
            }
        }
    }

    fn handle_messages_loaded(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: String,
        messages: Vec<MessageItem>,
        members: Vec<MemberInfo>,
    ) {
        if req_seq == self.conv_req_seq
            && self.active_conv_key.as_deref() == Some(&conv_key)
        {
            self.loading_messages = false;
            if !messages.is_empty() {
                let prev_len = self.messages.len();
                let prev_last_id = self.messages.last().map(|m| m.msg_id.clone());
                merge_optimistic_messages(&mut self.messages, messages);
                self.bump_messages_gen();
                clamp_chat_selection(
                    self.messages.len(),
                    &mut ui.chat_scroll.selected_idx,
                );
                let appended_at_bottom = self.messages.len() > prev_len
                    || self.messages.last().map(|m| m.msg_id.as_str())
                        != prev_last_id.as_deref();
                if appended_at_bottom
                    && !ui.chat_scroll.follow_tail
                    && ui.chat_scroll.scroll_up > 0
                    && ui.chat_scroll.total_lines > 0
                    && ui.pending_prepend_anchor.is_none()
                    && ui.pending_append_anchor.is_none()
                {
                    ui.pending_append_anchor = Some(ui.chat_scroll.offset);
                }
            }
            if self.cached_history_exhausted(&conv_key) {
                self.oldest_reached = true;
            }
            if !members.is_empty() {
                self.members = members;
            }
        }
    }

    fn handle_members_loaded(
        &mut self,
        req_seq: u64,
        conv_key: String,
        members: Vec<MemberInfo>,
        clear_loading: bool,
    ) {
        if req_seq == self.conv_req_seq
            && self.active_conv_key.as_deref() == Some(&conv_key)
        {
            if clear_loading {
                self.loading_messages = false;
            }
            if !members.is_empty() {
                self.members = members;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_clipboard_image_encoded(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: Option<String>,
        composer_target_thread: bool,
        file_name: String,
        png_bytes: Vec<u8>,
        preview: Option<crate::img_preview::RenderedTerminalImage>,
    ) {
        if req_seq == self.clipboard_paste_seq
            && self.active_conv_key == conv_key
            && ui.composer_target_thread == composer_target_thread
        {
            ui.composer.stage_inline_image_with_preview(
                png_bytes,
                "image/png",
                Some(file_name),
                preview,
            );
            ui.set_focus(Focus::Composer);
        }
    }

    fn handle_older_messages_loaded(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: String,
        messages: Vec<MessageItem>,
        reached_eof: bool,
    ) {
        if req_seq == self.older_req_seq
            && self.active_conv_key.as_deref() == Some(&conv_key)
        {
            self.loading_older = false;
            self.oldest_reached = reached_eof;
            self.history_error = None;
            self.prepend_older_messages(ui, messages);
        }
    }

    fn handle_older_messages_failed(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: String,
        local_messages: Vec<MessageItem>,
        error: String,
    ) {
        if req_seq == self.older_req_seq
            && self.active_conv_key.as_deref() == Some(&conv_key)
        {
            self.loading_older = false;
            self.history_error = Some(error);
            self.prepend_older_messages(ui, local_messages);
        }
    }

    fn handle_older_chats_loaded(
        &mut self,
        ui: &mut UiState,
        mut conversations: Vec<ConversationSummary>,
        reached_eof: bool,
    ) {
        self.loading_older_chats = false;
        self.chats_oldest_reached = reached_eof;
        if !conversations.is_empty() {
            let prev_row = nav_pane::nav_row_at(self, ui, ui.nav_selected);
            self.apply_active_read_guard(&mut conversations);
            self.conversations = conversations;
            self.bump_conversations_gen();
            preserve_nav_selection(self, ui, prev_row);
        }
    }

    fn handle_thread_loaded(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: String,
        root_msg_id: String,
        mut thread: ThreadView,
    ) {
        // Accepted only for the latest in-flight thread request matching the active thread.
        if req_seq == self.thread_req_seq
            && self.thread_msg_is_current(&conv_key, &root_msg_id)
        {
            self.loading_thread = false;
            self.pending_thread = None;
            if let Some(existing) = self.active_thread.as_mut() {
                if existing.conv_key == thread.conv_key
                    && existing.root.msg_id == thread.root.msg_id
                {
                    merge_optimistic_messages(
                        &mut existing.replies,
                        std::mem::take(&mut thread.replies),
                    );
                    thread.replies = std::mem::take(&mut existing.replies);
                }
            }
            // The channel view shows `↳ N replies` from its own copy of the root, which
            // delta sync leaves at 0: carry the loaded thread's count over to it.
            let confirmed_replies = thread
                .replies
                .iter()
                .filter(|r| !r.deleted && !r.msg_id.starts_with("optimistic-"))
                .count();
            let reply_count = thread.root.reply_count.max(confirmed_replies);
            if let Some(root) = self
                .messages
                .iter_mut()
                .find(|m| m.msg_id == thread.root.msg_id && m.parent_msg_id.is_none())
            {
                if root.reply_count != reply_count {
                    root.reply_count = reply_count;
                    self.bump_messages_gen();
                }
            }
            let target_reply_id = self.pending_thread_reply_id.take();
            self.active_thread = Some(thread);
            self.bump_thread_gen();
            if let (Some(target_id), Some(active)) =
                (target_reply_id.as_deref(), self.active_thread.as_ref())
            {
                if active.root.msg_id == target_id {
                    ui.thread_scroll.selected_idx = Some(0);
                    ui.pending_thread_reveal = true;
                } else if let Some(pos) =
                    active.replies.iter().position(|r| r.msg_id == target_id)
                {
                    ui.thread_scroll.selected_idx = Some(pos + 1);
                    ui.pending_thread_reveal = true;
                }
            }
            clamp_thread_selection(
                self.active_thread.as_ref(),
                &mut ui.thread_scroll.selected_idx,
            );
        }
    }

    fn handle_thread_load_failed(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: String,
        root_msg_id: String,
        error: String,
    ) {
        if req_seq == self.thread_req_seq
            && self.thread_msg_is_current(&conv_key, &root_msg_id)
        {
            self.loading_thread = false;
            self.pending_thread = None;
            self.pending_thread_reply_id = None;
            let has_cached = self
                .active_thread
                .as_ref()
                .is_some_and(|t| t.root.msg_id == root_msg_id);
            if !has_cached {
                // Nothing to show: an empty drawer would look like it is still loading.
                self.close_thread(ui);
            }
            ui.error(format!("Thread sync failed: {error}"));
        }
    }

    fn handle_message_sent(
        &mut self,
        ui: &mut UiState,
        conv_key: String,
        parent_msg_id: Option<String>,
        optimistic_id: Option<String>,
        sent: MessageItem,
    ) {
        self.update_conversation_preview_from_sent(&conv_key, &sent);
        if self.active_conv_key.as_deref() == Some(&conv_key) {
            if let Some(pid) = parent_msg_id {
                if let Some(thread) = self.active_thread.as_mut() {
                    if thread.root.msg_id == pid {
                        if let Some(opt_id) = optimistic_id.as_deref() {
                            if let Some(pos) =
                                thread.replies.iter().position(|r| r.msg_id == opt_id)
                            {
                                if thread.replies.iter().any(|r| r.msg_id == sent.msg_id) {
                                    thread.replies.remove(pos);
                                } else {
                                    thread.replies[pos] = sent;
                                }
                            } else if !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                            {
                                thread.replies.push(sent);
                                ui.thread_scroll.bottom();
                            }
                        } else if !thread.replies.iter().any(|r| r.msg_id == sent.msg_id) {
                            thread.replies.push(sent);
                            ui.thread_scroll.bottom();
                        }
                        self.bump_thread_gen();
                    }
                }
                if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                    if let Ok(Some(db_root)) = self.storage.get_message(&conv_key, &pid) {
                        root.reply_count = db_root.reply_count;
                    } else {
                        root.reply_count = root.reply_count.saturating_add(1);
                    }
                    self.bump_messages_gen();
                }
            } else if let Some(opt_id) = optimistic_id.as_deref() {
                if let Some(pos) = self.messages.iter().position(|m| m.msg_id == opt_id) {
                    if self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                        self.messages.remove(pos);
                    } else {
                        self.messages[pos] = sent;
                    }
                    self.bump_messages_gen();
                } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                    self.messages.push(sent);
                    self.bump_messages_gen();
                    ui.chat_scroll.bottom();
                }
            } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                self.messages.push(sent);
                self.bump_messages_gen();
                ui.chat_scroll.bottom();
            }
        }
    }

    fn handle_message_send_failed(
        &mut self,
        ui: &mut UiState,
        conv_key: String,
        parent_msg_id: Option<String>,
        optimistic_id: Option<String>,
        text: String,
    ) {
        if self.active_conv_key.as_deref() == Some(&conv_key) {
            if let Some(ref opt_id) = optimistic_id {
                if let Some(ref pid) = parent_msg_id {
                    if let Some(thread) = self.active_thread.as_mut() {
                        if &thread.root.msg_id == pid {
                            thread.replies.retain(|r| &r.msg_id != opt_id);
                            self.bump_thread_gen();
                        }
                    }
                    clamp_thread_selection(
                        self.active_thread.as_ref(),
                        &mut ui.thread_scroll.selected_idx,
                    );
                } else {
                    self.messages.retain(|m| &m.msg_id != opt_id);
                    self.bump_messages_gen();
                    clamp_chat_selection(
                        self.messages.len(),
                        &mut ui.chat_scroll.selected_idx,
                    );
                }
            }
        }
        if !text.trim().is_empty() {
            let orig_draft_key = match parent_msg_id {
                Some(ref pid) => format!("{conv_key}#thread:{pid}"),
                None => conv_key.clone(),
            };
            if self.current_draft_key(ui).as_deref() == Some(orig_draft_key.as_str()) {
                if ui.composer.text().trim().is_empty() {
                    ui.composer.load_text(&text);
                }
            } else if let std::collections::hash_map::Entry::Vacant(entry) =
                self.drafts.entry(orig_draft_key)
            {
                entry.insert(text);
                self.persist_drafts();
            }
        }
    }

    fn handle_attachment_sent(
        &mut self,
        ui: &mut UiState,
        conv_key: String,
        sent: MessageItem,
    ) {
        self.update_conversation_preview_from_sent(&conv_key, &sent);
        if self.active_conv_key.as_deref() == Some(&conv_key) {
            if let Some(pid) = sent.parent_msg_id.clone() {
                if let Some(thread) = self.active_thread.as_mut() {
                    if thread.root.msg_id == pid
                        && !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                    {
                        thread.replies.push(sent);
                        self.bump_thread_gen();
                        ui.thread_scroll.bottom();
                    }
                }
                if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                    if let Ok(Some(db_root)) = self.storage.get_message(&conv_key, &pid) {
                        root.reply_count = db_root.reply_count;
                    } else {
                        root.reply_count = root.reply_count.saturating_add(1);
                    }
                    self.bump_messages_gen();
                }
            } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                self.messages.push(sent);
                self.bump_messages_gen();
                ui.chat_scroll.bottom();
            }
        }
    }

    fn handle_message_deleted(
        &mut self,
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    ) {
        if self.active_conv_key.as_deref() == Some(&conv_key) {
            if let Some(ref pid) = parent_msg_id {
                let mut thread_changed = false;
                if let Some(thread) = self.active_thread.as_mut() {
                    for r in &mut thread.replies {
                        if r.msg_id == msg_id {
                            mark_message_deleted_in_memory(r);
                            thread_changed = true;
                        }
                    }
                }
                if thread_changed {
                    self.bump_thread_gen();
                }
                if let Some(root) = self.messages.iter_mut().find(|m| &m.msg_id == pid) {
                    if let Ok(Some(db_root)) = self.storage.get_message(&conv_key, pid) {
                        root.reply_count = db_root.reply_count;
                        self.bump_messages_gen();
                    }
                }
            } else {
                let mut msg_changed = false;
                for m in &mut self.messages {
                    if m.msg_id == msg_id {
                        mark_message_deleted_in_memory(m);
                        msg_changed = true;
                    }
                }
                if msg_changed {
                    self.bump_messages_gen();
                }
                let mut thread_changed = false;
                if let Some(thread) = self.active_thread.as_mut() {
                    if thread.root.msg_id == msg_id {
                        mark_message_deleted_in_memory(&mut thread.root);
                        thread_changed = true;
                    }
                }
                if thread_changed {
                    self.bump_thread_gen();
                }
            }
        }
        if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == conv_key) {
            if conv.last_msg_id.as_deref() == Some(msg_id.as_str()) {
                conv.last_msg_preview = Some("(deleted message)".to_string());
                self.bump_conversations_gen();
            }
        }
        if !self.my_user_id.is_empty()
            && self
                .activity_mentions
                .iter()
                .any(|am| am.conv_key == conv_key && am.msg_id == msg_id)
        {
            if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
            {
                self.activity_mentions = mentions;
                self.bump_mentions_gen();
            }
        }
    }

    fn handle_reaction_updated(
        &mut self,
        conv_key: String,
        msg_id: String,
        reaction_type: String,
        added: bool,
    ) {
        if self.active_conv_key.as_deref() == Some(&conv_key) {
            let my_name = self.my_profile.as_ref().map(|p| p.display_name.clone());
            let mut msg_changed = false;
            if let Some(m) = self.messages.iter_mut().find(|m| m.msg_id == msg_id) {
                crate::storage::apply_reaction_toggle(m, &reaction_type, added, my_name.as_deref());
                msg_changed = true;
            }
            if msg_changed {
                self.bump_messages_gen();
            }
            let mut thread_changed = false;
            if let Some(thread) = self.active_thread.as_mut() {
                if thread.root.msg_id == msg_id {
                    crate::storage::apply_reaction_toggle(
                        &mut thread.root,
                        &reaction_type,
                        added,
                        my_name.as_deref(),
                    );
                    thread_changed = true;
                }
                if let Some(r) = thread.replies.iter_mut().find(|r| r.msg_id == msg_id) {
                    crate::storage::apply_reaction_toggle(
                        r,
                        &reaction_type,
                        added,
                        my_name.as_deref(),
                    );
                    thread_changed = true;
                }
            }
            if thread_changed {
                self.bump_thread_gen();
            }
        }
    }

    fn handle_search_results(&mut self, ui: &mut UiState, query: String, hits: Vec<SearchHit>) {
        if self.search_query == query {
            self.searching = false;
            self.search_results = hits;
            if let Some(Overlay::Search(search)) = ui.overlays.last_mut() {
                if self.search_results.is_empty() {
                    search.selected = 0;
                } else if search.selected >= self.search_results.len() {
                    search.selected = self.search_results.len() - 1;
                }
            }
        }
    }

    fn handle_user_search_results(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        query: String,
        users: Vec<UserProfile>,
    ) {
        let query_matches_overlay = match ui.overlays.last() {
            Some(Overlay::NewChat(nc)) => {
                nc.peers_input.trim().eq_ignore_ascii_case(query.trim())
            }
            _ => true,
        };
        if req_seq == self.user_search_seq && query_matches_overlay {
            self.user_searching = false;
            self.user_search_results = users;
        }
    }

    fn handle_presence_updated(&mut self, status: PresenceStatus) {
        let uid = if status.user_id.is_empty() {
            self.my_user_id.clone()
        } else {
            status.user_id.clone()
        };
        self.my_presence = Some(PresenceStatus {
            user_id: uid.clone(),
            availability: status.availability.clone(),
            activity: status.activity.clone(),
        });
        if !uid.is_empty() {
            self.presences.insert(uid, status);
        }
        self.bump_presences_gen();
    }

    fn handle_image_decode_failed(
        &mut self,
        ui: &mut UiState,
        cache_key: String,
        owner_id: String,
        att_id: String,
        error: String,
    ) {
        self.image_pending.remove(&cache_key);
        self.insert_image_error(cache_key, error.clone());
        if let Some(Overlay::ImagePreview(preview)) = ui.overlays.last_mut() {
            if let Some(cur) = preview.current() {
                if cur.owner_id() == owner_id && cur.att_id() == att_id {
                    preview.error = Some(error);
                }
            }
        }
    }

    fn handle_image_auto_routed_to_html(
        &mut self,
        ui: &mut UiState,
        cache_key: String,
        owner_id: String,
        att_id: String,
        title: String,
        html: String,
    ) {
        self.image_pending.remove(&cache_key);
        self.clear_image_error(&cache_key);
        let matches_active_preview = matches!(
            ui.overlays.last(),
            Some(Overlay::ImagePreview(preview))
                if preview
                    .current()
                    .is_some_and(|cur| cur.owner_id() == owner_id && cur.att_id() == att_id)
        );
        if matches_active_preview {
            ui.close_top_overlay();
            ui.close_overlays_of(OverlayKind::HtmlPreview);
            ui.push_overlay(Overlay::html_preview(Some(title), html));
            ui.info("HTML content detected — opened in interactive HTML Preview");
        }
    }

    fn handle_html_attachment_loaded(
        &mut self,
        ui: &mut UiState,
        req_seq: u64,
        conv_key: Option<String>,
        title: String,
        html: String,
        kind: bitefoundry_html::ContentKind,
    ) {
        if req_seq == self.html_fetch_seq && self.active_conv_key == conv_key {
            ui.close_overlays_of(OverlayKind::HtmlPreview);
            let overlay = match kind {
                bitefoundry_html::ContentKind::Markdown => {
                    Overlay::markdown_preview(Some(title), html)
                }
                _ => Overlay::html_preview(Some(title), html),
            };
            ui.push_overlay(overlay);
        }
    }

    fn handle_daemon_attached(&mut self, ui: &mut UiState, pid: Option<u32>, sync_busy: bool) {
        self.daemon_attached = true;
        self.daemon_pid = pid;
        self.syncing = sync_busy;
        self.sync_summary = "Daemon connected".to_string();
        self.load_cached_teams();
        if self.active_conv_key.is_none() && self.active_target.is_none() {
            if let Some(first) = self
                .conversations
                .iter()
                .find(|c| c.kind != ConversationKind::Channel)
                .or_else(|| self.conversations.first())
                .cloned()
            {
                self.select_conversation_internal(ui, first.target, true, false);
            } else {
                self.sync_ctrl.set_focus(
                    self.active_target.clone(),
                    self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
                );
            }
        } else {
            self.sync_ctrl.set_focus(
                self.active_target.clone(),
                self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
            );
        }
        if self.my_profile.is_none() {
            self.spawn_me_profile_if_needed();
        }
    }

    fn handle_daemon_detached(&mut self) {
        self.daemon_attached = false;
        self.daemon_pid = None;
        if !self.syncing {
            self.sync_summary = "Local sync".to_string();
        }
        self.sync_ctrl.set_focus(
            self.active_target.clone(),
            self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
        );
    }

    fn handle_profile_loaded(
        &mut self,
        ui: &mut UiState,
        me: Option<UserProfile>,
        presence: Option<PresenceStatus>,
        scope_warning: Option<String>,
    ) {
        if let Some(u) = me {
            self.my_user_id = u.id.clone();
            self.my_profile = Some(u);
        }
        if let Some(p) = presence {
            self.my_presence = Some(p);
            self.bump_presences_gen();
        }
        self.emit_scope_warning_once(ui, scope_warning);
        if !self.my_user_id.is_empty() && self.activity_mentions.is_empty() {
            if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
            {
                self.activity_mentions = mentions;
                self.bump_mentions_gen();
            }
        }
    }

    fn handle_task_done(
        &mut self,
        ui: &mut UiState,
        label: String,
        error: Option<String>,
        toast: Option<(String, ToastKind)>,
    ) {
        ui.tasks.finish(&label, error);
        if let Some((text, kind)) = toast {
            match kind {
                ToastKind::Info => ui.toasts.info(text),
                ToastKind::Success => ui.toasts.success(text),
                ToastKind::Error => ui.toasts.error(text),
            }
        }
    }
}
