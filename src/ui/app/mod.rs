//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

mod cache;
mod events;
mod helpers;
mod messages;
mod navigation;
mod requests_messages;
mod requests_shared;
mod state;

#[cfg(test)]
pub(super) mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{self, AppConfig};
use crate::graph::GraphClient;
use crate::img_preview::RenderedTerminalImage;
use crate::models::{
    ChannelSummary, ConversationKind, ConversationSummary, ConversationTarget, MemberInfo,
    MessageItem, PresenceStatus, SearchHit, TeamSummary, ThreadView, UserProfile,
};
use crate::storage::Storage;
use crate::sync::SyncController;

use super::action::Action;
use super::hitmap::MessageScope;
use super::overlays::context_menu::{ContextMenu, ContextTarget, MenuItem};
use super::overlays::OverlayKind;

pub(crate) use self::helpers::preserve_nav_selection;
pub use self::state::{
    Focus, NarrowPane, PendingPixelGraphic, ScrollState, UiMsg, UiState,
    MAX_RAW_IMAGE_CACHE_BYTES, MAX_RAW_IMAGE_CACHE_ENTRIES,
};

pub struct AppState {
    pub config: AppConfig,
    pub auth: AuthManager,
    pub graph: GraphClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub my_profile: Option<UserProfile>,
    pub my_user_id: String,
    pub my_presence: Option<PresenceStatus>,
    pub conversations: Vec<ConversationSummary>,
    pub teams: Vec<TeamSummary>,
    pub active_conv_key: Option<String>,
    pub active_target: Option<ConversationTarget>,
    pub messages: Vec<MessageItem>,
    /// A single changed row replaced the last message at the same position.
    pub(crate) tail_message_updated: bool,
    pub active_thread: Option<ThreadView>,
    pub activity_mentions: Vec<MessageItem>,
    pub members: Vec<MemberInfo>,
    pub member_fetched_at: HashMap<String, Instant>,
    pub presences: HashMap<String, PresenceStatus>,
    pub drafts: HashMap<String, String>,
    pub draft_save_seq: Arc<AtomicU64>,
    pub draft_save_lock: Arc<Mutex<()>>,
    pub download_dir: PathBuf,
    pub loading_messages: bool,
    pub loading_older: bool,
    pub loading_thread: bool,
    /// `(conv_key, root_msg_id)` of the thread fetch in flight. The drawer can be open on a
    /// root that is not cached yet, so `active_thread` alone cannot match the answer.
    pub pending_thread: Option<(String, String)>,
    /// Target reply message ID to select & reveal once the active thread finishes loading.
    pub pending_thread_reply_id: Option<String>,
    /// Monotonic sequence counters guarding async UI loaders against stale out-of-order answers.
    pub conv_req_seq: u64,
    pub older_req_seq: u64,
    pub thread_req_seq: u64,
    pub user_search_seq: u64,
    pub clipboard_paste_seq: u64,
    pub html_fetch_seq: u64,
    pub loading_older_chats: bool,
    pub chats_oldest_reached: bool,
    pub syncing: bool,
    pub sync_summary: String,
    pub daemon_attached: bool,
    pub daemon_pid: Option<u32>,
    pub searching: bool,
    pub search_query: String,
    pub search_results: Vec<SearchHit>,
    pub user_searching: bool,
    pub user_search_results: Vec<UserProfile>,
    pub oldest_reached: bool,
    pub history_error: Option<String>,
    pub scope_warned: bool,
    // Terminal inline image preview cache (bounded LRU-style FIFO + 8 MiB byte budget)
    pub image_cache: HashMap<String, RenderedTerminalImage>,
    pub image_cache_order: VecDeque<String>,
    pub image_cache_bytes: usize,
    pub image_cache_gen: u64,
    pub image_pending: HashSet<String>,
    pub image_errors: HashMap<String, String>,
    pub image_errors_order: VecDeque<String>,
    // Bounded raw attachment byte cache (8 entries, 16 MiB) for offline zoom/protocol re-render
    pub image_raw_cache: HashMap<String, Arc<[u8]>>,
    pub image_raw_order: VecDeque<String>,
    pub image_raw_bytes: usize,
    /// Lazy localhost Axum server for viewing HTML documents in the default browser (`0 MB` idle).
    pub html_server: bitefoundry_html::NativeHtmlServer,
    /// `(conv_key, read_at_rfc3339)` last marked read while active, guarding against stale Lane 2 unread flips.
    pub active_marked_read_at: Option<(String, String)>,
    /// Conversations already warmed (or opened) in this session to avoid duplicate neighbor prefetch.
    pub prefetched_convs: HashSet<String>,
    /// Channels whose root page was fetched with `$expand=replies` in this session. Delta sync
    /// carries no replies, so cached roots are refreshed once per session to fix reply counts.
    pub reply_hydrated_convs: HashSet<String>,
    /// Rate-limit timestamp for interactive neighbor prefetch.
    pub last_prefetch_at: Option<Instant>,
    /// Concurrency cap for background interactive neighbor prefetch.
    pub prefetch_sem: Arc<tokio::sync::Semaphore>,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        auth: AuthManager,
        graph: GraphClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let download_dir = config.resolved_download_dir();
        let drafts = config::load_drafts();
        let my_user_id = graph.my_user_id().unwrap_or_default().to_string();
        Self {
            config,
            auth,
            graph,
            storage,
            sync_ctrl,
            my_profile: None,
            my_user_id,
            my_presence: None,
            conversations: Vec::new(),
            teams: Vec::new(),
            active_conv_key: None,
            active_target: None,
            messages: Vec::new(),
            tail_message_updated: false,
            active_thread: None,
            activity_mentions: Vec::new(),
            members: Vec::new(),
            member_fetched_at: HashMap::new(),
            presences: HashMap::new(),
            drafts,
            draft_save_seq: Arc::new(AtomicU64::new(0)),
            draft_save_lock: Arc::new(Mutex::new(())),
            download_dir,
            loading_messages: false,
            loading_older: false,
            loading_thread: false,
            pending_thread: None,
            pending_thread_reply_id: None,
            conv_req_seq: 0,
            older_req_seq: 0,
            thread_req_seq: 0,
            user_search_seq: 0,
            clipboard_paste_seq: 0,
            html_fetch_seq: 0,
            loading_older_chats: false,
            chats_oldest_reached: false,
            syncing: false,
            sync_summary: "Ready".to_string(),
            daemon_attached: false,
            daemon_pid: None,
            searching: false,
            search_query: String::new(),
            search_results: Vec::new(),
            user_searching: false,
            user_search_results: Vec::new(),
            oldest_reached: false,
            history_error: None,
            scope_warned: false,
            image_cache: HashMap::new(),
            image_cache_order: VecDeque::new(),
            image_cache_bytes: 0,
            image_cache_gen: 0,
            image_pending: HashSet::new(),
            image_errors: HashMap::new(),
            image_errors_order: VecDeque::new(),
            image_raw_cache: HashMap::new(),
            image_raw_order: VecDeque::new(),
            image_raw_bytes: 0,
            html_server: bitefoundry_html::NativeHtmlServer::new(),
            active_marked_read_at: None,
            prefetched_convs: HashSet::new(),
            reply_hydrated_convs: HashSet::new(),
            last_prefetch_at: None,
            prefetch_sem: Arc::new(tokio::sync::Semaphore::new(2)),
            ui_tx,
        }
    }

    pub fn active_conversation(&self) -> Option<&ConversationSummary> {
        let key = self.active_conv_key.as_deref()?;
        self.conversations.iter().find(|c| c.conv_key == key)
    }

    pub fn active_channel(&self) -> Option<(&TeamSummary, &ChannelSummary)> {
        let target = self.active_target.as_ref()?;
        match target {
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => {
                let team = self.teams.iter().find(|t| &t.id == team_id)?;
                let ch = team.channels.iter().find(|c| &c.id == channel_id)?;
                Some((team, ch))
            }
            ConversationTarget::Chat { .. } => None,
        }
    }

    pub fn active_title(&self) -> String {
        if let Some(conv) = self.active_conversation() {
            return conv.title.clone();
        }
        if let Some((team, ch)) = self.active_channel() {
            return format!("{} › #{}", team.display_name, ch.display_name);
        }
        "Select a conversation".to_string()
    }

    pub fn message_at(&self, scope: MessageScope, msg_index: usize) -> Option<&MessageItem> {
        match scope {
            MessageScope::Chat => self.messages.get(msg_index),
            MessageScope::ThreadRoot => self.active_thread.as_ref().map(|t| &t.root),
            MessageScope::ThreadReply => self
                .active_thread
                .as_ref()
                .and_then(|t| t.replies.get(msg_index)),
        }
    }

    pub fn message_at_mut(
        &mut self,
        scope: MessageScope,
        msg_index: usize,
    ) -> Option<&mut MessageItem> {
        match scope {
            MessageScope::Chat => self.messages.get_mut(msg_index),
            MessageScope::ThreadRoot => self.active_thread.as_mut().map(|t| &mut t.root),
            MessageScope::ThreadReply => self
                .active_thread
                .as_mut()
                .and_then(|t| t.replies.get_mut(msg_index)),
        }
    }


    pub fn unread_chat_count(&self) -> i32 {
        self.conversations
            .iter()
            .filter(|c| c.kind != ConversationKind::Channel)
            .map(|c| c.unread_count.max(0))
            .sum()
    }

    pub fn unread_channel_count(&self) -> i32 {
        self.conversations
            .iter()
            .filter(|c| c.kind == ConversationKind::Channel)
            .map(|c| c.unread_count.max(0))
            .sum()
    }

    pub fn conversation_kind_label(kind: ConversationKind) -> &'static str {
        match kind {
            ConversationKind::OneOnOne => "1:1",
            ConversationKind::Group => "Group",
            ConversationKind::Meeting => "Meeting",
            ConversationKind::Channel => "Channel",
        }
    }

    /// Builds a right-click / `m` context menu for the given target using current app state.
    pub fn build_context_menu(
        &self,
        target: ContextTarget,
        anchor: (u16, u16),
        ui: &UiState,
    ) -> ContextMenu {
        let items = match target {
            ContextTarget::NavRow(index) => vec![
                MenuItem::new("Open", Some("Enter"), Action::SelectNavRow(index)),
                MenuItem::new("Refresh", Some("F5"), Action::RefreshAll),
                MenuItem::new(
                    "New Chat",
                    Some("Ctrl+N"),
                    Action::OpenOverlay(OverlayKind::NewChat),
                ),
            ],
            ContextTarget::Message { scope, msg_index } => {
                let msg = self.message_at(scope, msg_index);
                let is_channel_root = scope == MessageScope::Chat
                    && self
                        .active_conversation()
                        .is_some_and(|c| c.kind == ConversationKind::Channel)
                    && msg.is_some_and(|m| m.parent_msg_id.is_none());
                let eff_att_len = msg
                    .filter(|m| !m.deleted)
                    .map_or(0, |m| m.effective_attachments().len());
                let has_attach = eff_att_len > 0;
                let is_me = msg.is_some_and(|m| m.is_me);

                let mut items = Vec::new();
                if is_channel_root {
                    items.push(MenuItem::new(
                        "Open Thread",
                        Some("t"),
                        Action::OpenThread { msg_index },
                    ));
                }
                items.push(MenuItem::new(
                    "Reply / Quote",
                    Some("r"),
                    Action::ReplyTo { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Add Reaction",
                    Some("+"),
                    Action::OpenReactions { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Copy Message",
                    Some("y"),
                    Action::CopyMessage { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Inspect HTML / Card",
                    Some("H"),
                    Action::OpenHtmlPreviewForMessage { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Extract Links (URLs)",
                    Some("u"),
                    Action::OpenLinksForMessage { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "Toggle Inline Link URLs",
                    Some("h"),
                    Action::ToggleShowHrefs,
                ));
                if has_attach {
                    let attach_index = ui.attach_index.min(eff_att_len.saturating_sub(1));
                    items.push(MenuItem::new(
                        "Download Attachment",
                        Some("d"),
                        Action::DownloadAttachment {
                            scope,
                            msg_index,
                            attach_index,
                        },
                    ));
                }
                if is_me {
                    items.push(MenuItem::new(
                        "Delete Message",
                        Some("Del"),
                        Action::RequestDeleteMessage { scope, msg_index },
                    ));
                }
                items
            }
            ContextTarget::Composer => vec![
                MenuItem::new("Send", Some("Enter"), Action::SendMessage),
                MenuItem::new("Toggle Subject", Some("Ctrl+S"), Action::ToggleSubject),
                MenuItem::new("External Editor", Some("Ctrl+E"), Action::OpenExternalEditor),
                MenuItem::new("Cancel Reply", Some("Esc"), Action::CancelReply),
                MenuItem::new(
                    "Clear Pending Images",
                    Some("Ctrl+Shift+X"),
                    Action::ClearPendingImages,
                ),
                MenuItem::new("Clear Draft", None, Action::ClearComposer),
            ],
            ContextTarget::Global => vec![
                MenuItem::new("Search", Some("/"), Action::OpenSearch),
                MenuItem::new("Browse & Attach File", Some("Ctrl+F"), Action::OpenFilePicker),
                MenuItem::new(
                    "New Chat",
                    Some("Ctrl+N"),
                    Action::OpenOverlay(OverlayKind::NewChat),
                ),
                MenuItem::new(
                    "Change Presence",
                    Some("Ctrl+P"),
                    Action::OpenOverlay(OverlayKind::Presence),
                ),
                MenuItem::new("Refresh", Some("F5"), Action::RefreshAll),
                MenuItem::new(
                    "Settings",
                    Some("Ctrl+,"),
                    Action::OpenOverlay(OverlayKind::Settings),
                ),
                MenuItem::new("Help", Some("?"), Action::OpenOverlay(OverlayKind::Help)),
                MenuItem::new("Quit", Some("Ctrl+Q"), Action::QuitConfirm),
            ],
        };
        ContextMenu::from_items(target, anchor, items)
    }
}
