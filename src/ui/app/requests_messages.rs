//! Async message mutations and pagination requests (send, older history, reactions, delete, files).

use chrono::Utc;

use crate::auth;
use crate::models::{
    ConversationSummary, ConversationTarget, MessageItem, OutgoingMessage, QuotedReply, SyncCursor,
};
use crate::parser;
use super::super::hitmap::MessageScope;
use super::super::toast::ToastKind;
use super::helpers::is_expired_page_link_error;
use super::{AppState, UiMsg, UiState};

impl AppState {
    pub fn send_composer(&mut self, ui: &mut UiState) {
        let Some(target) = self.active_target.clone() else {
            ui.info("Select a conversation first");
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let text = ui.composer.text().trim().to_string();
        if text.is_empty() && ui.composer.pending_images.is_empty() {
            return;
        }

        let is_channel = target.is_channel();
        let thread_root_id = if ui.composer_target_thread {
            self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
        } else {
            None
        };
        let subject = if is_channel && thread_root_id.is_none() && ui.show_subject {
            ui.composer.subject_to_send()
        } else {
            None
        };
        let mentions = ui.composer.compiled_mentions();
        let inline_images = ui.composer.pending_images.clone();
        let quote = if thread_root_id.is_none() {
            ui.composer.reply().map(|r| QuotedReply {
                msg_id: r.msg_id.clone(),
                author_name: r.author.clone(),
                preview_plain: r.text.clone(),
            })
        } else {
            None
        };

        let outgoing: OutgoingMessage = parser::compile_outgoing_markdown(
            &text,
            subject,
            mentions,
            Vec::new(),
            inline_images.clone(),
            quote.clone(),
        );

        let optimistic_id = if inline_images.is_empty() {
            let opt_id = format!("optimistic-{}", uuid::Uuid::new_v4());
            let author_name = self
                .my_profile
                .as_ref()
                .map(|p| p.display_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Me".to_string());
            let optimistic_msg = MessageItem {
                msg_key: format!("{conv_key}|{opt_id}"),
                conv_key: conv_key.clone(),
                msg_id: opt_id.clone(),
                parent_msg_id: thread_root_id.clone(),
                reply_to_id: quote.as_ref().map(|q| q.msg_id.clone()),
                reply_to_author: quote.as_ref().map(|q| q.author_name.clone()),
                reply_to_preview: quote.as_ref().map(|q| q.preview_plain.clone()),
                subject: outgoing.subject.clone(),
                author_id: (!self.my_user_id.is_empty()).then(|| self.my_user_id.clone()),
                author_name,
                is_me: true,
                created_at: Utc::now().to_rfc3339(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: outgoing.body_html.clone(),
                body_plain: text.clone(),
                attachments: Vec::new(),
                mentions: Vec::new(),
                reactions: Vec::new(),
                reply_count: 0,
                web_url: None,
            };
            if let Some(ref pid) = thread_root_id {
                let mut pushed_thread = false;
                if let Some(thread) = self.active_thread.as_mut() {
                    if &thread.root.msg_id == pid {
                        thread.replies.push(optimistic_msg);
                        ui.thread_scroll.bottom();
                        pushed_thread = true;
                    }
                }
                if pushed_thread {
                    self.bump_thread_gen();
                }
            } else {
                self.messages.push(optimistic_msg);
                self.bump_messages_gen();
                ui.chat_scroll.bottom();
            }
            Some(opt_id)
        } else {
            None
        };

        let task_label = "Send message".to_string();
        ui.tasks.begin(task_label.clone());
        self.clear_current_draft(ui);
        ui.show_subject = false;

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let quote_id = quote.map(|q| q.msg_id);

        tokio::spawn(async move {
            let result = if let Some(ref root_id) = thread_root_id {
                graph.reply_message(&target, root_id, &outgoing).await
            } else if let Some(ref reply_id) = quote_id {
                if !target.is_channel() {
                    graph.reply_message(&target, reply_id, &outgoing).await
                } else {
                    graph.send_message(&target, &outgoing).await
                }
            } else {
                graph.send_message(&target, &outgoing).await
            };

            match result {
                Ok(sent) => {
                    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
                    let _ = tx.send(UiMsg::MessageSent {
                        conv_key,
                        parent_msg_id: thread_root_id,
                        optimistic_id,
                        sent,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    if optimistic_id.is_some() || !text.is_empty() {
                        let _ = tx.send(UiMsg::MessageSendFailed {
                            conv_key,
                            parent_msg_id: thread_root_id,
                            optimistic_id,
                            text,
                        });
                    }
                    let msg = format!("Send failed: {err}");
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((msg, ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn load_older(&mut self, ui: &mut UiState) {
        self.load_older_internal(ui, 50, true);
    }

    pub fn load_older_auto(&mut self, ui: &mut UiState) {
        self.load_older_internal(ui, 50, false);
    }

    pub fn load_older_with_depth(&mut self, ui: &mut UiState, batch_size: usize) {
        self.load_older_internal(ui, batch_size.max(1), true);
    }

    fn load_older_internal(&mut self, ui: &mut UiState, batch_size: usize, explicit: bool) {
        if self.loading_older {
            return;
        }
        if self.oldest_reached && batch_size <= 50 && !(explicit && self.history_error.is_some()) {
            if explicit {
                ui.info("All older messages loaded.");
            }
            return;
        }
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let Some(target) = self
            .active_target
            .clone()
            .or_else(|| ConversationTarget::from_conv_key(&conv_key))
        else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let earliest = self
            .messages
            .iter()
            .find(|m| !m.msg_id.starts_with("optimistic-"));
        let oldest_ts = earliest.map(|m| m.created_at.clone());
        let oldest_id = earliest.map(|m| m.msg_id.clone());

        self.older_req_seq = self.older_req_seq.wrapping_add(1);
        let req_seq = self.older_req_seq;
        self.loading_older = true;
        self.history_error = None;
        let task_label = "Load older messages".to_string();
        ui.tasks.begin(task_label.clone());

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let force_deep = batch_size > 50;

        tokio::spawn(async move {
            let db_storage = storage.clone();
            let db_conv = conv_key.clone();
            let db_ts = oldest_ts.clone();
            let db_id = oldest_id.clone();
            let (local_older, cursor) = tokio::task::spawn_blocking(move || {
                let older = match db_ts.as_deref() {
                    Some(ts) => db_storage
                        .list_older_root_messages(&db_conv, batch_size, ts, db_id.as_deref())
                        .unwrap_or_default(),
                    None => db_storage
                        .list_root_messages(&db_conv, batch_size, None)
                        .unwrap_or_default(),
                };
                let cur = db_storage
                    .get_sync_cursor(&SyncCursor::history_scope(&db_conv))
                    .ok()
                    .flatten();
                (older, cur)
            })
            .await
            .unwrap_or_default();

            let cursor_is_eof = cursor.as_ref().is_some_and(|c| c.is_history_eof());
            if local_older.len() >= batch_size || (!force_deep && cursor_is_eof) {
                // Revalidate the cached older window against Graph online so edits, soft-deletes,
                // and reactions on older history pages appear when re-opening them. Fall back to
                // the cached local rows if offline or if the network request fails.
                // The walk starts at the newest page, so it is capped: a window deep in the
                // history is served from the cache instead of costing up to 15 requests.
                // An explicit history sync keeps the full budget.
                const REVALIDATE_MAX_PAGES: usize = 2;
                let refreshed_older = if !local_older.is_empty() || explicit {
                    match graph
                        .fetch_older_messages_bounded(
                            &target,
                            None,
                            oldest_ts.as_deref(),
                            batch_size,
                            REVALIDATE_MAX_PAGES,
                        )
                        .await
                    {
                        Ok((fetched_msgs, _)) if !fetched_msgs.is_empty() => {
                            let write_storage = storage.clone();
                            let db_conv = conv_key.clone();
                            let db_ts = oldest_ts.clone();
                            let db_id = oldest_id.clone();
                            tokio::task::spawn_blocking(move || {
                                let _ = write_storage.upsert_messages(&fetched_msgs);
                                match db_ts.as_deref() {
                                    Some(ts) => write_storage
                                        .list_older_root_messages(
                                            &db_conv,
                                            batch_size,
                                            ts,
                                            db_id.as_deref(),
                                        )
                                        .unwrap_or(fetched_msgs),
                                    None => write_storage
                                        .list_root_messages(&db_conv, batch_size, None)
                                        .unwrap_or(fetched_msgs),
                                }
                            })
                            .await
                            .unwrap_or(local_older)
                        }
                        _ => local_older,
                    }
                } else {
                    local_older
                };

                let reached_eof = if !cursor_is_eof {
                    false
                } else if refreshed_older.len() < batch_size {
                    true
                } else {
                    let check_storage = storage.clone();
                    let check_conv = conv_key.clone();
                    let earliest_in_batch = refreshed_older
                        .first()
                        .map(|m| (m.created_at.clone(), m.msg_id.clone()));
                    tokio::task::spawn_blocking(move || match earliest_in_batch {
                        Some((ts, id)) => check_storage
                            .list_older_root_messages(&check_conv, 1, &ts, Some(&id))
                            .map(|v| v.is_empty())
                            .unwrap_or(true),
                        None => true,
                    })
                    .await
                    .unwrap_or(true)
                };
                let _ = tx.send(UiMsg::OlderMessagesLoaded {
                    req_seq,
                    conv_key,
                    messages: refreshed_older,
                    reached_eof,
                });
                let _ = tx.send(UiMsg::TaskDone {
                    label: task_label,
                    error: None,
                    toast: None,
                });
                return;
            }

            let saved_link = cursor.and_then(|c| c.delta_link);
            let needed = batch_size.saturating_sub(local_older.len()).max(30);
            match graph
                .fetch_older_messages(&target, saved_link.as_deref(), oldest_ts.as_deref(), needed)
                .await
            {
                Ok((fetched_msgs, new_next_link)) => {
                    let write_storage = storage.clone();
                    let db_conv = conv_key.clone();
                    let db_ts = oldest_ts.clone();
                    let db_id = oldest_id.clone();
                    let next_link_clone = new_next_link.clone();
                    let (combined_older, reached_eof) = tokio::task::spawn_blocking(move || {
                        if !fetched_msgs.is_empty() {
                            let _ = write_storage.upsert_messages(&fetched_msgs);
                        }
                        let oldest_fetched_ts = if next_link_clone.is_none() {
                            Some(SyncCursor::HISTORY_EOF.to_string())
                        } else {
                            fetched_msgs
                                .iter()
                                .filter(|m| m.parent_msg_id.is_none())
                                .map(|m| m.created_at.clone())
                                .min()
                        };
                        let updated_cursor = SyncCursor {
                            scope_key: SyncCursor::history_scope(&db_conv),
                            delta_link: next_link_clone.clone(),
                            oldest_msg_ts: oldest_fetched_ts,
                            synced_at: Utc::now().to_rfc3339(),
                        };
                        let _ = write_storage.save_sync_cursor(&updated_cursor);

                        let combined = match db_ts.as_deref() {
                            Some(ts) => write_storage
                                .list_older_root_messages(&db_conv, batch_size, ts, db_id.as_deref())
                                .unwrap_or_default(),
                            None => write_storage
                                .list_root_messages(&db_conv, batch_size, None)
                                .unwrap_or_default(),
                        };
                        let reached_eof = if next_link_clone.is_some() {
                            false
                        } else if combined.len() < batch_size {
                            true
                        } else if let Some(earliest) = combined.first() {
                            write_storage
                                .list_older_root_messages(
                                    &db_conv,
                                    1,
                                    &earliest.created_at,
                                    Some(&earliest.msg_id),
                                )
                                .map(|v| v.is_empty())
                                .unwrap_or(true)
                        } else {
                            true
                        };
                        (combined, reached_eof)
                    })
                    .await
                    .unwrap_or_default();

                    let _ = tx.send(UiMsg::OlderMessagesLoaded {
                        req_seq,
                        conv_key,
                        messages: combined_older,
                        reached_eof,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let raw_err = err.to_string();
                    let tok = graph.auth().current_tokens().await.map(|t| t.access_token);
                    let friendly_err =
                        auth::explain_graph_permission_error(&raw_err, tok.as_deref())
                            .unwrap_or_else(|| format!("Failed to load older messages: {raw_err}"));
                    let _ = tx.send(UiMsg::OlderMessagesFailed {
                        req_seq,
                        conv_key,
                        local_messages: local_older,
                        error: friendly_err.clone(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(raw_err),
                        toast: if explicit {
                            Some((friendly_err, ToastKind::Error))
                        } else {
                            None
                        },
                    });
                }
            }
        });
    }

    pub fn load_older_chats(&mut self) {
        if self.loading_older_chats {
            return;
        }
        let cursor = self
            .storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .ok()
            .flatten();
        // The stored cursor decides, not the cached flag: a fresh page-1 sync can replace a
        // stale EOF with a nextLink, and the flag would otherwise block that page for good.
        self.chats_oldest_reached = cursor.as_ref().is_some_and(SyncCursor::is_history_eof);
        if self.chats_oldest_reached {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.loading_older_chats = true;
        let saved_next_link = cursor
            .and_then(|c| c.delta_link)
            .filter(|link| !link.trim().is_empty());
        let fallback_target_count = self.conversations.len() + 50;
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            type PagedChatsResult = anyhow::Result<(Vec<ConversationSummary>, Option<String>, bool)>;
            let mut paged: Option<PagedChatsResult> = None;
            if let Some(ref link) = saved_next_link {
                match graph.list_chats_page(50, Some(link)).await {
                    Ok((chats, next)) => {
                        let eof = next.is_none();
                        paged = Some(Ok((chats, next, eof)));
                    }
                    Err(err) if is_expired_page_link_error(&err) => {
                        // An expired `$skiptoken` fails the same way on every retry: drop the
                        // cursor and let the count-based listing below take over.
                        let clear_storage = storage.clone();
                        let dead_link = link.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            clear_storage.delete_sync_cursor_with_link(
                                SyncCursor::CHATS_LIST_SCOPE,
                                &dead_link,
                            )
                        })
                        .await;
                    }
                    Err(err) => paged = Some(Err(err)),
                }
            }
            let res = match paged {
                Some(res) => res,
                None => graph
                    .list_chats(fallback_target_count)
                    .await
                    .map(|chats| {
                        let eof = chats.len() < fallback_target_count;
                        (chats, None, eof)
                    }),
            };

            match res {
                Ok((chats, next_link, reached_eof)) => {
                    let conversations = tokio::task::spawn_blocking(move || {
                        if !chats.is_empty() {
                            let _ = storage.upsert_conversations(&chats);
                        }
                        if next_link.is_some() || reached_eof {
                            let cursor = SyncCursor {
                                scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
                                delta_link: next_link,
                                oldest_msg_ts: if reached_eof {
                                    Some(SyncCursor::HISTORY_EOF.to_string())
                                } else {
                                    None
                                },
                                synced_at: Utc::now().to_rfc3339(),
                            };
                            let _ = storage.save_sync_cursor(&cursor);
                        }
                        storage.list_conversations().unwrap_or_default()
                    })
                    .await
                    .unwrap_or_default();
                    let _ = tx.send(UiMsg::OlderChatsLoaded {
                        conversations,
                        reached_eof,
                    });
                }
                Err(_) => {
                    let _ = tx.send(UiMsg::OlderChatsFailed);
                }
            }
        });
    }

    pub fn toggle_reaction(
        &mut self,
        ui: &mut UiState,
        scope: MessageScope,
        msg_index: usize,
        reaction_type: String,
    ) {
        let Some(target) = self.active_target.clone() else {
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let Some(msg) = self.message_at(scope, msg_index).cloned() else {
            return;
        };
        let parent_msg_id = match scope {
            MessageScope::ThreadReply => self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
            _ => msg.parent_msg_id.clone(),
        };
        let currently_set = msg
            .reactions
            .iter()
            .any(|r| r.reaction_type == reaction_type && r.me_reacted);
        let enable = !currently_set;

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let msg_id = msg.msg_id.clone();
        let my_name = self.my_profile.as_ref().map(|p| p.display_name.clone());
        let task_label = "Reaction".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .set_reaction(
                    &target,
                    &msg_id,
                    parent_msg_id.as_deref(),
                    &reaction_type,
                    enable,
                )
                .await
            {
                Ok(()) => {
                    let db_ck = conv_key.clone();
                    let db_mid = msg_id.clone();
                    let db_rx = reaction_type.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.apply_local_reaction(
                            &db_ck,
                            &db_mid,
                            &db_rx,
                            enable,
                            my_name.as_deref(),
                        );
                    })
                    .await;
                    let _ = tx.send(UiMsg::ReactionUpdated {
                        conv_key,
                        msg_id,
                        reaction_type,
                        added: enable,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Reaction failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_message(
        &mut self,
        ui: &mut UiState,
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    ) {
        let Some(target) = self.active_target.clone() else {
            return;
        };
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Delete message".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .delete_message(&target, &msg_id, parent_msg_id.as_deref())
                .await
            {
                Ok(()) => {
                    let db_ck = conv_key.clone();
                    let db_mid = msg_id.clone();
                    let now = Utc::now().to_rfc3339();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.apply_local_soft_delete(&db_ck, &db_mid, &now);
                    })
                    .await;
                    let _ = tx.send(UiMsg::MessageDeleted {
                        conv_key,
                        msg_id,
                        parent_msg_id,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Message deleted".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Delete failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn download_attachment(
        &mut self,
        ui: &mut UiState,
        scope: MessageScope,
        msg_index: usize,
        attach_index: usize,
    ) {
        let Some(msg) = self.message_at(scope, msg_index) else {
            return;
        };
        let Some(att) = msg.effective_attachments().get(attach_index).cloned() else {
            ui.info("Attachment not found");
            return;
        };
        let att_name = att.name.clone().unwrap_or_else(|| "attachment".to_string());
        let graph = self.graph.clone();
        let dest = self.download_dir.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("Download: {att_name}");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.download_attachment(&att, &dest).await {
                Ok(file) => {
                    let msg = format!("Saved: {}", file.path.display());
                    let _ = tx.send(UiMsg::AttachmentDownloaded(file));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((msg, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Download failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn send_file(&mut self, ui: &mut UiState, path: String, caption: String) {
        let Some(target) = self.active_target.clone() else {
            ui.info("Select a conversation first");
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let thread_root_id = if ui.composer_target_thread {
            self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
        } else {
            None
        };
        let caption_html = if caption.trim().is_empty() {
            None
        } else {
            Some(
                parser::compile_outgoing_markdown(
                    caption.trim(),
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    None,
                )
                .body_html,
            )
        };

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Upload file".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .send_file(
                    &target,
                    std::path::Path::new(&path),
                    caption_html.as_deref(),
                    thread_root_id.as_deref(),
                )
                .await
            {
                Ok(sent) => {
                    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
                    let _ = tx.send(UiMsg::AttachmentSent { conv_key, sent });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("File sent".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("File send failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }
}
