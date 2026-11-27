//! Shared async requests (bootstrap, profile, search, chat creation, HTML preview, updates, presence).

use crate::auth;
use crate::models::{AttachmentInfo, PresenceStatus};
use crate::sync;

use super::super::toast::ToastKind;
use super::{AppState, UiMsg, UiState};

impl AppState {
    pub(super) fn spawn_teams_bootstrap(&self, task_label: Option<String>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (tok, me_res, presence_res, _conv_res) = tokio::join!(
                auth.current_tokens(),
                graph.get_me(),
                graph.get_my_presence(),
                sync::bootstrap_conversations(&graph, &storage),
            );
            let scope_warning = tok.as_ref().and_then(|t| {
                auth::diagnose_access_token_scopes(&t.access_token)
                    .warnings
                    .into_iter()
                    .next()
            });
            let token_uid = tok.and_then(|t| t.user_id);
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.mail.clone())
                    .await;
            }
            let presence = presence_res.ok();
            let my_id_owned = me
                .as_ref()
                .map(|u| u.id.clone())
                .or(token_uid)
                .unwrap_or_default();

            let read_storage = storage.clone();
            let (conversations, teams, mentions, cached_presences) =
                tokio::task::spawn_blocking(move || {
                    let conversations = read_storage.list_conversations().unwrap_or_default();
                    let teams = read_storage.list_teams().unwrap_or_default();
                    let mentions = if !my_id_owned.is_empty() {
                        read_storage
                            .list_mentions_for_user(&my_id_owned, 50)
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let cached_presences = read_storage.get_presences().unwrap_or_default();
                    (conversations, teams, mentions, cached_presences)
                })
                .await
                .unwrap_or_default();

            let peer_ids: Vec<String> = conversations
                .iter()
                .filter_map(|c| c.peer_user_id.clone())
                .collect();

            // Emit bootstrap results immediately with cached presences so the UI unlocks
            // conversations/teams and starts active-room sync without waiting on peer presence RTT.
            let _ = tx.send(UiMsg::BootstrapDone {
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences: cached_presences,
                scope_warning,
            });
            if let Some(label) = task_label {
                let _ = tx.send(UiMsg::TaskDone {
                    label,
                    error: None,
                    toast: None,
                });
            }

            if !peer_ids.is_empty() {
                if let Ok(fetched) = graph.get_presences_by_user_ids(&peer_ids).await {
                    if !fetched.is_empty() {
                        let write_storage = storage.clone();
                        let fetched_for_db = fetched.clone();
                        let presences = tokio::task::spawn_blocking(move || {
                            let _ = write_storage.upsert_presences(&fetched_for_db);
                            write_storage.get_presences()
                        })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_else(|| {
                            fetched
                                .into_iter()
                                .map(|p| (p.user_id.clone(), p))
                                .collect()
                        });
                        let _ = tx.send(UiMsg::PeerPresencesLoaded(presences));
                    }
                }
            }
        });
    }

    pub(super) fn spawn_me_profile_if_needed(&self) {
        if self.my_profile.is_some() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (tok, me_res, presence_res) = tokio::join!(
                auth.current_tokens(),
                graph.get_me(),
                graph.get_my_presence()
            );
            let scope_warning = tok.as_ref().and_then(|t| {
                auth::diagnose_access_token_scopes(&t.access_token)
                    .warnings
                    .into_iter()
                    .next()
            });
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.mail.clone())
                    .await;
            }
            let presence = presence_res.ok();
            let _ = tx.send(UiMsg::ProfileLoaded {
                me,
                presence,
                scope_warning,
            });
        });
    }

    pub fn spawn_bootstrap(&mut self, ui: &mut UiState) {
        let task_label = "Initial sync".to_string();
        ui.tasks.begin(task_label.clone());
        self.syncing = true;
        self.sync_summary = "Syncing...".to_string();
        self.spawn_teams_bootstrap(Some(task_label));
    }

    pub fn search_messages(&mut self, ui: &mut UiState, query: String) {
        self.search_query = query.clone();
        if query.trim().is_empty() {
            self.search_results.clear();
            self.searching = false;
            return;
        }
        self.searching = true;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Search".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let search_query = query.clone();
            let hits = tokio::task::spawn_blocking(move || {
                storage
                    .search_messages(&search_query, None, 100)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let _ = tx.send(UiMsg::SearchResults { query, hits });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn search_users(&mut self, ui: &mut UiState, query: String) {
        self.user_search_seq = self.user_search_seq.wrapping_add(1);
        let req_seq = self.user_search_seq;
        if query.trim().is_empty() {
            self.user_search_results.clear();
            self.user_searching = false;
            return;
        }
        self.user_searching = true;
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Search users".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let users = graph.search_users(&query, 20).await.unwrap_or_default();
            let _ = tx.send(UiMsg::UserSearchResults {
                req_seq,
                query,
                users,
            });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn create_chat(&mut self, ui: &mut UiState, peers: Vec<String>, topic: Option<String>) {
        if peers.is_empty() {
            ui.info("Select at least one recipient");
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Create chat".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.create_chat(&peers, topic.as_deref()).await {
                Ok(conv) => {
                    let _ = storage.upsert_conversations(std::slice::from_ref(&conv));
                    let _ = tx.send(UiMsg::ChatCreated(conv));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("New chat opened".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Failed to create chat: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn fetch_html_attachment(&mut self, ui: &mut UiState, att: AttachmentInfo) {
        let is_md_hint =
            crate::ui::overlays::html_preview::HtmlPreviewOverlay::is_markdown_attachment(&att);
        let title = att.name.clone().unwrap_or_else(|| {
            if is_md_hint {
                format!("Markdown ({})", att.id)
            } else {
                format!("HTML ({})", att.id)
            }
        });
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot fetch attachment preview without async runtime");
            return;
        }
        self.html_fetch_seq = self.html_fetch_seq.wrapping_add(1);
        let req_seq = self.html_fetch_seq;
        let conv_key = self.active_conv_key.clone();
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = if is_md_hint {
            format!("Open Markdown: {title}")
        } else {
            format!("Open HTML: {title}")
        };
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.fetch_attachment_bytes(&att).await {
                Ok((fetched_name, fetched_mime, bytes)) => {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let resolved_title = if fetched_name.trim().is_empty() {
                        title
                    } else {
                        fetched_name
                    };
                    let mime_hint = if !fetched_mime.trim().is_empty() {
                        Some(fetched_mime.as_str())
                    } else if !att.content_type.trim().is_empty() {
                        Some(att.content_type.as_str())
                    } else {
                        None
                    };
                    let mut kind = bitefoundry_html::sniff_content_kind(
                        mime_hint,
                        Some(resolved_title.as_str()),
                        Some(&bytes),
                    );
                    if kind != bitefoundry_html::ContentKind::Markdown
                        && kind != bitefoundry_html::ContentKind::Html
                        && (is_md_hint
                            || bitefoundry_html::sniff_content_kind(
                                None,
                                Some(resolved_title.as_str()),
                                None,
                            ) == bitefoundry_html::ContentKind::Markdown)
                    {
                        kind = bitefoundry_html::ContentKind::Markdown;
                    }
                    let _ = tx.send(UiMsg::HtmlAttachmentLoaded {
                        req_seq,
                        conv_key,
                        title: resolved_title,
                        html,
                        kind,
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
                        toast: Some((format!("Failed to load preview: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn serve_html_preview(&self, ui: &mut UiState, title: String, html: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot start native HTML server without async runtime");
            return;
        }
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("Serve HTML: {title}");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match server.serve_html(&title, &html).await {
                Ok(url) => {
                    #[cfg(not(test))]
                    let open_res = open::that_detached(&url);
                    #[cfg(test)]
                    let open_res: Result<(), std::io::Error> = Ok(());

                    let _ = tx.send(UiMsg::HtmlServed { url: url.clone() });
                    let toast = match open_res {
                        Ok(()) => (format!("Opened in browser: {url}"), ToastKind::Success),
                        Err(err) => (
                            format!("Serving at {url} (browser launch failed: {err})"),
                            ToastKind::Info,
                        ),
                    };
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(toast),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Native HTML server failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn check_for_updates(&mut self, ui: &mut UiState, check_only: bool, force: bool) {
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        let task_label = if check_only {
            "Check updates".to_string()
        } else {
            "Install update".to_string()
        };
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match crate::updater::check_or_apply_update(&repo, check_only, force).await {
                Ok(res) => {
                    let toast_msg = if res.updated {
                        format!(
                            "Updated: {} -> {} (restart recommended)",
                            res.current_version, res.latest_version
                        )
                    } else if res.update_available {
                        format!(
                            "Update available: {} (current {})",
                            res.latest_version, res.current_version
                        )
                    } else {
                        format!("Up to date ({})", res.current_version)
                    };
                    let _ = tx.send(UiMsg::UpdateChecked(res));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((toast_msg, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Update check failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn set_presence(&mut self, ui: &mut UiState, availability: String, activity: String) {
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let my_uid = self.my_user_id.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Update presence".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.set_my_presence(&availability, &activity, None).await {
                Ok(()) => {
                    let mut status = graph.get_my_presence().await.unwrap_or(PresenceStatus {
                        user_id: my_uid.clone(),
                        availability: availability.clone(),
                        activity,
                    });
                    if status.user_id.is_empty() && !my_uid.is_empty() {
                        status.user_id = my_uid;
                    }
                    if !status.user_id.is_empty() {
                        let to_save = status.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = storage.upsert_presences(std::slice::from_ref(&to_save));
                        })
                        .await;
                    }
                    let _ = tx.send(UiMsg::PresenceUpdated(status));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((format!("Presence: {availability}"), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Presence update failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

}
