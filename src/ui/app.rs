//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::{self, AuthManager};
use crate::config::{self, AppConfig};
use crate::graph::GraphClient;
use crate::img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::{
    AttachmentInfo, ChannelSummary, ConversationKind, ConversationSummary, ConversationTarget,
    CoreEvent, DownloadedFile, MemberInfo, MessageItem, NotificationCandidate, OutgoingMessage,
    PresenceStatus, QuotedReply, ReactionSummary, SearchHit, SyncCursor, TeamSummary, ThreadView,
    UpdateCheckResult, UserProfile,
};
use crate::parser;
use crate::storage::Storage;
use crate::sync::{self, SyncController};

use super::composer::{ComposerState, ReplyPreview};
use super::hitmap::{HitMap, MessageScope};
use super::keymap::Keymap;
use super::nav_pane::{self, NavRow};
use super::notification;
use super::overlays::image_preview::PreviewTarget;
use super::overlays::{Overlay, OverlayKind};
use super::theme::Theme;
use super::toast::{TaskBoard, ToastKind, ToastQueue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    Chat,
    Thread,
    Composer,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NarrowPane {
    #[default]
    List,
    Chat,
    Thread,
}

#[derive(Debug, Clone)]
pub struct ScrollState {
    pub follow_tail: bool,
    pub scroll_up: u16,
    pub selected_idx: Option<usize>,
    pub area: Rect,
    pub total_lines: usize,
    pub offset: usize,
    /// `(start_line, end_line_exclusive)` per rendered item (`0..N` in Chat, `0` = root and `1..=N` in Thread).
    pub message_ranges: Arc<[(usize, usize)]>,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            follow_tail: true,
            scroll_up: 0,
            selected_idx: None,
            area: Rect::default(),
            total_lines: 0,
            offset: 0,
            message_ranges: Arc::from([]),
        }
    }
}

impl ScrollState {
    pub fn clamp_to_viewport(&mut self, height: usize) {
        let max_off = self
            .total_lines
            .saturating_sub(height)
            .min(u16::MAX as usize) as u16;
        if self.scroll_up > max_off {
            self.scroll_up = max_off;
        }
        if self.scroll_up == 0 {
            self.follow_tail = true;
        }
    }

    pub fn scroll_up(&mut self, lines: u16) {
        self.follow_tail = false;
        self.scroll_up = self.scroll_up.saturating_add(lines);
        if self.area.height > 0 && self.total_lines > 0 {
            self.clamp_to_viewport(self.area.height as usize);
        }
    }

    pub fn scroll_down(&mut self, lines: u16) {
        if lines >= self.scroll_up {
            self.scroll_up = 0;
            self.follow_tail = true;
        } else {
            self.scroll_up -= lines;
        }
    }

    pub fn bottom(&mut self) {
        self.scroll_up = 0;
        self.follow_tail = true;
    }

    pub fn top(&mut self) {
        self.follow_tail = false;
        let h = self.area.height as usize;
        let max_off = if h > 0 {
            self.total_lines.saturating_sub(h)
        } else {
            self.total_lines
        };
        self.scroll_up = max_off.min(u16::MAX as usize) as u16;
        if self.scroll_up == 0 && h > 0 && self.total_lines > 0 {
            self.follow_tail = true;
        }
    }

    pub fn offset(&self, height: usize) -> usize {
        if self.total_lines <= height {
            return 0;
        }
        let max_off = self.total_lines - height;
        if self.follow_tail {
            max_off
        } else {
            max_off.saturating_sub(self.scroll_up as usize)
        }
    }

    /// Adjusts `scroll_up` / `follow_tail` so item `idx` in `message_ranges` stays visible inside `viewport_height`.
    pub fn ensure_visible(&mut self, idx: usize, viewport_height: usize) {
        if viewport_height == 0 || self.total_lines <= viewport_height {
            return;
        }
        let Some(&(start, end)) = self.message_ranges.get(idx) else {
            return;
        };
        let max_off = self.total_lines - viewport_height;
        let cur_off = self.offset(viewport_height);
        let new_off = if start < cur_off {
            start
        } else if end > cur_off + viewport_height {
            if end.saturating_sub(start) >= viewport_height {
                start
            } else {
                end - viewport_height
            }
        } else {
            return;
        };
        let clamped = new_off.min(max_off);
        let up = max_off.saturating_sub(clamped);
        if up == 0 {
            self.scroll_up = 0;
            self.follow_tail = true;
        } else {
            self.follow_tail = false;
            self.scroll_up = up.min(u16::MAX as usize) as u16;
        }
    }
}

pub enum UiMsg {
    BootstrapDone {
        me: Option<UserProfile>,
        presence: Option<PresenceStatus>,
        conversations: Vec<ConversationSummary>,
        teams: Vec<TeamSummary>,
        mentions: Vec<MessageItem>,
        presences: HashMap<String, PresenceStatus>,
        scope_warning: Option<String>,
    },
    MessagesLoaded {
        conv_key: String,
        messages: Vec<MessageItem>,
        members: Vec<MemberInfo>,
    },
    MembersLoaded {
        conv_key: String,
        members: Vec<MemberInfo>,
        clear_loading: bool,
    },
    ClipboardImageEncoded {
        width: u32,
        height: u32,
        file_name: String,
        png_bytes: Vec<u8>,
        preview: Option<RenderedTerminalImage>,
    },
    OlderMessagesLoaded {
        conv_key: String,
        messages: Vec<MessageItem>,
        reached_eof: bool,
    },
    OlderMessagesFailed {
        conv_key: String,
        local_messages: Vec<MessageItem>,
        error: String,
    },
    OlderChatsLoaded {
        conversations: Vec<ConversationSummary>,
        reached_eof: bool,
    },
    OlderChatsFailed,
    ThreadLoaded {
        conv_key: String,
        root_msg_id: String,
        thread: ThreadView,
    },
    MessageSent {
        conv_key: String,
        parent_msg_id: Option<String>,
        optimistic_id: Option<String>,
        sent: MessageItem,
    },
    MessageSendFailed {
        conv_key: String,
        parent_msg_id: Option<String>,
        optimistic_id: String,
        text: String,
    },
    MessageDeleted {
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    },
    ReactionUpdated {
        conv_key: String,
        msg_id: String,
        reaction_type: String,
        added: bool,
    },
    AttachmentDownloaded(DownloadedFile),
    AttachmentSent {
        conv_key: String,
        sent: MessageItem,
    },
    SearchResults {
        query: String,
        hits: Vec<SearchHit>,
    },
    UserSearchResults {
        query: String,
        users: Vec<UserProfile>,
    },
    ChatCreated(ConversationSummary),
    PresenceUpdated(PresenceStatus),
    ImageDecoded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    ImageBytesFetched {
        raw_key: String,
        bytes: Arc<[u8]>,
    },
    ImageDecodeFailed {
        cache_key: String,
        owner_id: String,
        att_id: String,
        error: String,
    },
    ImageAutoRoutedToHtml {
        cache_key: String,
        owner_id: String,
        att_id: String,
        title: String,
        html: String,
    },
    HtmlAttachmentLoaded {
        title: String,
        html: String,
    },
    HtmlServed {
        url: String,
    },
    UpdateChecked(UpdateCheckResult),
    DaemonAttached {
        pid: Option<u32>,
        sync_busy: bool,
    },
    DaemonDetached,
    StartLocalBootstrap,
    ProfileLoaded {
        me: Option<UserProfile>,
        presence: Option<PresenceStatus>,
        scope_warning: Option<String>,
    },
    TaskDone {
        label: String,
        error: Option<String>,
        toast: Option<(String, ToastKind)>,
    },
}

pub const MAX_RAW_IMAGE_CACHE_ENTRIES: usize = 8;
pub const MAX_RAW_IMAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
}

pub struct UiState {
    pub focus: Focus,
    pub overlays: Vec<Overlay>,
    pub composer: ComposerState,
    pub hitmap: HitMap,
    pub toasts: ToastQueue,
    pub tasks: TaskBoard,
    pub keymap: Keymap,
    pub theme: Theme,
    pub active_nav_tab: usize,
    pub nav_selected: usize,
    pub nav_area: Rect,
    pub collapsed_team_ids: HashSet<String>,
    pub chat_scroll: ScrollState,
    pub thread_scroll: ScrollState,
    /// `(old_total_lines, old_offset)` recorded right before prepending older messages.
    pub pending_prepend_anchor: Option<(usize, usize)>,
    /// `old_offset` recorded right before appending new messages at the bottom while scrolled up.
    pub pending_append_anchor: Option<usize>,
    pub attach_index: usize,
    pub show_nav_rail: bool,
    pub auto_thread_pane: bool,
    pub inline_images: bool,
    pub show_hrefs: bool,
    pub thread_drawer_open: bool,
    pub narrow_pane: NarrowPane,
    pub editor_requested: bool,
    pub notify: bool,
    pub composer_target_thread: bool,
    pub show_subject: bool,
    pub term_width: u16,
    pub term_height: u16,
    pub density: crate::config::DensityMode,
    pub theme_preset: crate::config::ThemePreset,
    pub kitty_keyboard_supported: bool,
    pub nav_unread_only: bool,
    pub pinned_convs: HashSet<String>,
    /// `(conv_key, last_read_at_iso)` captured right before marking a conversation read so `chat_view` renders the unread divider.
    pub unread_anchor: Option<(String, String)>,
    pub conv_history: Vec<String>,
    pub conv_history_pos: usize,
    pub last_conv_key: Option<String>,
    pub last_click: Option<(std::time::Instant, u16, u16, super::hitmap::HitTarget)>,
    pub thread_composer_area: Rect,
    pub poll_retrying: bool,
    pub spinner_tick: u8,
    pub graphics_caps: TerminalGraphicsCaps,
    pub pending_pixel_graphic: Option<PendingPixelGraphic>,
    pub active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        let mut keymap = Keymap::default_teams();
        keymap.bind(
            Some(Focus::Composer),
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
            super::action::Action::PrevFocus,
        );
        keymap.bind(
            Some(Focus::Composer),
            KeyCode::BackTab,
            KeyModifiers::NONE,
            super::action::Action::PrevFocus,
        );
        Self {
            focus: Focus::Nav,
            overlays: Vec::new(),
            composer: ComposerState::new(),
            hitmap: HitMap::default(),
            toasts: ToastQueue::new(),
            tasks: TaskBoard::default(),
            keymap,
            theme: Theme::default(),
            active_nav_tab: 0,
            nav_selected: 0,
            nav_area: Rect::default(),
            collapsed_team_ids: HashSet::new(),
            chat_scroll: ScrollState::default(),
            thread_scroll: ScrollState::default(),
            pending_prepend_anchor: None,
            pending_append_anchor: None,
            attach_index: 0,
            show_nav_rail: true,
            auto_thread_pane: true,
            inline_images: true,
            show_hrefs: false,
            thread_drawer_open: false,
            narrow_pane: NarrowPane::List,
            editor_requested: false,
            notify: true,
            composer_target_thread: false,
            show_subject: false,
            term_width: 120,
            term_height: 40,
            density: crate::config::DensityMode::default(),
            theme_preset: crate::config::ThemePreset::default(),
            kitty_keyboard_supported: false,
            nav_unread_only: false,
            pinned_convs: HashSet::new(),
            unread_anchor: None,
            conv_history: Vec::new(),
            conv_history_pos: 0,
            last_conv_key: None,
            last_click: None,
            thread_composer_area: Rect::default(),
            poll_retrying: false,
            spinner_tick: 0,
            graphics_caps: TerminalGraphicsCaps::default(),
            pending_pixel_graphic: None,
            active_pixel_graphic: None,
        }
    }

    pub fn from_config(config: &AppConfig) -> Self {
        let mut s = Self::new();
        s.show_nav_rail = config.show_nav_rail;
        s.auto_thread_pane = config.auto_thread_pane;
        s.notify = config.notify;
        s.inline_images = config.inline_images;
        s.density = config.density;
        s.theme_preset = config.theme;
        s.theme = Theme::detect(config.theme);
        s.pinned_convs = config.pinned_convs.iter().cloned().collect();
        s.graphics_caps = img_preview::detect_graphics_caps(config.image_protocol);
        s
    }

    pub fn focus(&self) -> Focus {
        if !self.overlays.is_empty() {
            Focus::Overlay
        } else {
            self.focus
        }
    }

    pub fn accepts_text_input(&self) -> bool {
        matches!(self.focus(), Focus::Composer)
            || self.overlays.last().is_some_and(Overlay::accepts_text)
    }

    pub fn push_overlay(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
    }

    pub fn close_top_overlay(&mut self) -> Option<Overlay> {
        self.overlays.pop()
    }

    pub fn close_overlays_of(&mut self, kind: OverlayKind) {
        self.overlays.retain(|o| o.kind() != kind);
    }

    pub fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
    }

    pub fn toggle_composer_focus(&mut self, active: bool) {
        if active {
            self.set_focus(Focus::Composer);
        } else if self.thread_drawer_open {
            self.set_focus(Focus::Thread);
        } else {
            self.set_focus(Focus::Chat);
        }
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toasts.info(msg);
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.toasts.success(msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toasts.error(msg);
    }
}

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
        Self {
            config,
            auth,
            graph,
            storage,
            sync_ctrl,
            my_profile: None,
            my_user_id: String::new(),
            my_presence: None,
            conversations: Vec::new(),
            teams: Vec::new(),
            active_conv_key: None,
            active_target: None,
            messages: Vec::new(),
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

    pub fn current_draft_key(&self, ui: &UiState) -> Option<String> {
        let conv_key = self.active_conv_key.as_deref()?;
        if ui.composer_target_thread {
            if let Some(thread) = &self.active_thread {
                return Some(format!("{conv_key}#thread:{}", thread.root.msg_id));
            }
        }
        Some(conv_key.to_string())
    }

    fn persist_drafts(&self) {
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
    fn stash_draft_in_memory(&mut self, ui: &UiState) -> bool {
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
        }
        if let Ok(teams) = self.storage.list_teams() {
            self.teams = teams;
        }
        if let Ok(presences) = self.storage.get_presences() {
            self.presences = presences;
        }
        if !self.my_user_id.is_empty() {
            if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50) {
                self.activity_mentions = mentions;
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

    fn spawn_teams_bootstrap(&self, task_label: Option<String>) {
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
            let presences = if !peer_ids.is_empty() {
                let fetched = graph
                    .get_presences_by_user_ids(&peer_ids)
                    .await
                    .unwrap_or_default();
                let write_storage = storage.clone();
                let fetched_for_db = fetched.clone();
                tokio::task::spawn_blocking(move || {
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
                })
            } else {
                cached_presences
            };

            let _ = tx.send(UiMsg::BootstrapDone {
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences,
                scope_warning,
            });
            if let Some(label) = task_label {
                let _ = tx.send(UiMsg::TaskDone {
                    label,
                    error: None,
                    toast: None,
                });
            }
        });
    }

    fn spawn_me_profile_if_needed(&self) {
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

    pub fn select_conversation(&mut self, ui: &mut UiState, target: ConversationTarget) {
        self.select_conversation_internal(ui, target, true);
    }

    pub fn history_back(&mut self, ui: &mut UiState) {
        if ui.conv_history_pos == 0 || ui.conv_history.is_empty() {
            return;
        }
        ui.conv_history_pos -= 1;
        let key = ui.conv_history[ui.conv_history_pos].clone();
        if let Some(target) = self.target_for_conv_key(&key) {
            self.select_conversation_internal(ui, target, false);
        }
    }

    pub fn history_forward(&mut self, ui: &mut UiState) {
        if ui.conv_history_pos + 1 >= ui.conv_history.len() {
            return;
        }
        ui.conv_history_pos += 1;
        let key = ui.conv_history[ui.conv_history_pos].clone();
        if let Some(target) = self.target_for_conv_key(&key) {
            self.select_conversation_internal(ui, target, false);
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

    fn select_conversation_internal(
        &mut self,
        ui: &mut UiState,
        target: ConversationTarget,
        record_history: bool,
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
        self.active_thread = None;
        self.loading_older = false;
        self.history_error = None;
        self.oldest_reached = self
            .storage
            .get_sync_cursor(&SyncCursor::history_scope(&new_key))
            .ok()
            .flatten()
            .is_some_and(|c| c.is_history_eof());
        ui.thread_drawer_open = false;
        ui.composer_target_thread = false;
        ui.show_subject = false;
        ui.chat_scroll = ScrollState::default();
        ui.thread_scroll = ScrollState::default();
        ui.pending_prepend_anchor = None;
        ui.pending_append_anchor = None;

        self.messages = self
            .storage
            .list_root_messages(&new_key, 80, None)
            .unwrap_or_default();
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
        if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == new_key) {
            conv.unread_count = 0;
            conv.last_read_at = Some(now_ts.clone());
        }

        self.sync_ctrl.set_focus(Some(target.clone()), None);

        const MEMBER_CACHE_TTL: Duration = Duration::from_secs(300);
        let need_messages_fetch = self.messages.is_empty();
        let need_members_fetch = self.members.is_empty()
            || self
                .member_fetched_at
                .get(&new_key)
                .is_none_or(|t| t.elapsed() >= MEMBER_CACHE_TTL);
        self.loading_messages = need_messages_fetch;

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
                if need_messages_fetch {
                    Some(graph.list_messages(&target, 60).await)
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

            let fetched_msgs = msgs_res_opt.and_then(Result::ok);
            let fetched_members = members_res_opt.and_then(Result::ok);

            if fetched_msgs.is_some() || fetched_members.is_some() {
                let db_conv_key = conv_key.clone();
                let had_msgs = fetched_msgs.is_some();
                let (messages, members) = tokio::task::spawn_blocking(move || {
                    if let Some(ref msgs) = fetched_msgs {
                        if !msgs.is_empty() {
                            let _ = storage.upsert_messages(msgs);
                        }
                    }
                    if let Some(ref members) = fetched_members {
                        if !members.is_empty() {
                            let _ = storage.upsert_members(&db_conv_key, members);
                        }
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
                        conv_key,
                        messages,
                        members,
                    });
                } else {
                    let _ = tx.send(UiMsg::MembersLoaded {
                        conv_key,
                        members,
                        clear_loading: need_messages_fetch,
                    });
                }
            } else if need_messages_fetch {
                let _ = tx.send(UiMsg::MembersLoaded {
                    conv_key,
                    members: Vec::new(),
                    clear_loading: true,
                });
            }
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
                ..
            } => {
                if let Some(target) = ConversationTarget::from_conv_key(&conv_key) {
                    self.select_conversation(ui, target);
                    if let Some(root) = parent_msg_id {
                        self.open_thread_by_id(ui, root);
                    } else if let Some(m_idx) =
                        self.messages.iter().position(|m| m.msg_id == msg_id)
                    {
                        ui.chat_scroll.selected_idx = Some(m_idx);
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
        self.loading_thread = true;
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
        }
        ui.thread_drawer_open = true;
        ui.composer_target_thread = true;
        ui.narrow_pane = NarrowPane::Thread;
        ui.thread_scroll = ScrollState::default();
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
            if let Ok(thread) = graph
                .get_thread(&team_id, &channel_id, &root_msg_id, 200)
                .await
            {
                let mut all = Vec::with_capacity(1 + thread.replies.len());
                all.push(thread.root.clone());
                all.extend(thread.replies.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = storage.upsert_messages(&all);
                })
                .await;
                let _ = tx.send(UiMsg::ThreadLoaded {
                    conv_key,
                    root_msg_id,
                    thread,
                });
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

        self.loading_thread = true;
        let task_label = "Sync thread replies".to_string();
        ui.tasks.begin(task_label.clone());

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph
                .list_thread_replies(&team_id, &channel_id, &root_msg_id, 200)
                .await
            {
                Ok(replies) => {
                    let db_storage = storage.clone();
                    let db_conv = conv_key.clone();
                    let db_root = root_msg_id.clone();
                    let thread_opt = tokio::task::spawn_blocking(move || {
                        if !replies.is_empty() {
                            let _ = db_storage.upsert_messages(&replies);
                        }
                        db_storage.get_thread(&db_conv, &db_root, 200).ok().flatten()
                    })
                    .await
                    .ok()
                    .flatten();
                    if let Some(thread) = thread_opt {
                        let _ = tx.send(UiMsg::ThreadLoaded {
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
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Thread sync failed: {err}"), ToastKind::Error)),
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
        ui.narrow_pane = NarrowPane::Chat;
        self.sync_ctrl.set_focus(self.active_target.clone(), None);
        self.restore_draft(ui);
        ui.set_focus(Focus::Chat);
    }

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
                if let Some(thread) = self.active_thread.as_mut() {
                    if &thread.root.msg_id == pid {
                        thread.replies.push(optimistic_msg);
                        ui.thread_scroll.bottom();
                    }
                }
            } else {
                self.messages.push(optimistic_msg);
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
                    if let Some(opt_id) = optimistic_id {
                        let _ = tx.send(UiMsg::MessageSendFailed {
                            conv_key,
                            parent_msg_id: thread_root_id,
                            optimistic_id: opt_id,
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
                let reached_eof = if !cursor_is_eof {
                    false
                } else if local_older.len() < batch_size {
                    true
                } else {
                    let check_storage = storage.clone();
                    let check_conv = conv_key.clone();
                    let earliest_in_batch = local_older
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
                    conv_key,
                    messages: local_older,
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
        if self.loading_older_chats || self.chats_oldest_reached {
            return;
        }
        let cursor = self
            .storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .ok()
            .flatten();
        if cursor.as_ref().is_some_and(|c| c.is_history_eof()) {
            self.chats_oldest_reached = true;
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.loading_older_chats = true;
        let saved_next_link = cursor.and_then(|c| c.delta_link);
        let fallback_target_count = self.conversations.len() + 50;
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let res: anyhow::Result<(Vec<ConversationSummary>, Option<String>, bool)> =
                if let Some(ref link) = saved_next_link {
                    graph
                        .list_chats_page(50, Some(link))
                        .await
                        .map(|(chats, next)| {
                            let eof = next.is_none();
                            (chats, next, eof)
                        })
                } else {
                    graph
                        .list_chats(fallback_target_count)
                        .await
                        .map(|chats| {
                            let eof = chats.len() < fallback_target_count;
                            (chats, None, eof)
                        })
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
        let tx = self.ui_tx.clone();
        let msg_id = msg.msg_id.clone();
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
        let tx = self.ui_tx.clone();
        let task_label = "Delete message".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .delete_message(&target, &msg_id, parent_msg_id.as_deref())
                .await
            {
                Ok(()) => {
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
            let _ = tx.send(UiMsg::UserSearchResults { query, users });
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

    pub fn image_cache_key(owner_id: &str, att_id: &str, cols: u16, rows: u16) -> String {
        format!("{owner_id}:{att_id}:{cols}x{rows}")
    }

    pub fn preview_image_cache_key(
        owner_id: &str,
        att_id: &str,
        cols: u16,
        rows: u16,
        protocol: TerminalImageProtocol,
    ) -> String {
        format!("{owner_id}:{att_id}:{cols}x{rows}:{}", protocol.as_str())
    }

    pub fn raw_image_cache_key(owner_id: &str, att_id: &str) -> String {
        format!("{owner_id}:{att_id}")
    }

    pub fn insert_raw_image_cache(&mut self, raw_key: String, bytes: Arc<[u8]>) {
        let entry_bytes = bytes.len();
        if entry_bytes > MAX_RAW_IMAGE_CACHE_BYTES {
            return;
        }
        if let Some(prev) = self.image_raw_cache.remove(&raw_key) {
            self.image_raw_bytes = self.image_raw_bytes.saturating_sub(prev.len());
            self.image_raw_order.retain(|k| k != &raw_key);
        }
        self.image_raw_cache.insert(raw_key.clone(), bytes);
        self.image_raw_order.push_back(raw_key);
        self.image_raw_bytes = self.image_raw_bytes.saturating_add(entry_bytes);

        while self.image_raw_order.len() > MAX_RAW_IMAGE_CACHE_ENTRIES {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }

        while self.image_raw_bytes > MAX_RAW_IMAGE_CACHE_BYTES && self.image_raw_order.len() > 1 {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
    }

    fn touch_raw_image_cache(&mut self, raw_key: &str) -> Option<Arc<[u8]>> {
        let bytes = self.image_raw_cache.get(raw_key).cloned()?;
        self.image_raw_order.retain(|k| k != raw_key);
        self.image_raw_order.push_back(raw_key.to_string());
        Some(bytes)
    }

    fn rendered_image_bytes(rendered: &RenderedTerminalImage) -> usize {
        rendered.approx_heap_bytes().max(64)
    }

    fn insert_bounded_image_cache(
        cache: &mut HashMap<String, RenderedTerminalImage>,
        order: &mut VecDeque<String>,
        bytes_total: &mut usize,
        max_entries: usize,
        cache_key: String,
        rendered: RenderedTerminalImage,
    ) {
        if let Some(prev) = cache.remove(&cache_key) {
            *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&prev));
            order.retain(|k| k != &cache_key);
        }
        let entry_bytes = Self::rendered_image_bytes(&rendered);
        cache.insert(cache_key.clone(), rendered);
        order.push_back(cache_key);
        *bytes_total = bytes_total.saturating_add(entry_bytes);

        while order.len() > max_entries {
            let Some(evicted) = order.pop_front() else {
                break;
            };
            if let Some(removed) = cache.remove(&evicted) {
                *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }

        while *bytes_total > img_preview::MAX_IMAGE_CACHE_BYTES && order.len() > 1 {
            let Some(evicted) = order.pop_front() else {
                break;
            };
            if let Some(removed) = cache.remove(&evicted) {
                *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }
    }

    pub fn insert_image_cache(&mut self, cache_key: String, rendered: RenderedTerminalImage) {
        const MAX_IMAGE_CACHE: usize = 32;
        self.image_pending.remove(&cache_key);
        self.clear_image_error(&cache_key);
        Self::insert_bounded_image_cache(
            &mut self.image_cache,
            &mut self.image_cache_order,
            &mut self.image_cache_bytes,
            MAX_IMAGE_CACHE,
            cache_key,
            rendered,
        );
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn insert_image_error(&mut self, cache_key: String, error: String) {
        const MAX_IMAGE_ERRORS: usize = 64;
        if self.image_errors.contains_key(&cache_key) {
            self.image_errors_order.retain(|k| k != &cache_key);
        }
        self.image_errors.insert(cache_key.clone(), error);
        self.image_errors_order.push_back(cache_key);
        while self.image_errors_order.len() > MAX_IMAGE_ERRORS {
            if let Some(evicted) = self.image_errors_order.pop_front() {
                self.image_errors.remove(&evicted);
            }
        }
    }

    pub fn clear_image_error(&mut self, cache_key: &str) {
        if self.image_errors.remove(cache_key).is_some() {
            self.image_errors_order.retain(|k| k != cache_key);
        }
    }

    pub fn clear_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
        self.image_errors.clear();
        self.image_errors_order.clear();
        self.image_raw_cache.clear();
        self.image_raw_order.clear();
        self.image_raw_bytes = 0;
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn clear_image_caches(&mut self) {
        self.clear_image_cache();
    }

    fn spawn_attachment_image_decode(
        &mut self,
        key: String,
        msg_id: String,
        att: AttachmentInfo,
        cols: u16,
        rows: u16,
        bg: (u8, u8, u8),
        caps: Option<TerminalGraphicsCaps>,
    ) {
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        self.image_pending.insert(key.clone());
        let raw_key = Self::raw_image_cache_key(&msg_id, &att.id);
        let cached_raw = self.touch_raw_image_cache(&raw_key);
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let att_id = att.id.clone();

        tokio::spawn(async move {
            let raw_bytes = match cached_raw {
                Some(bytes) => Ok(bytes),
                None => match graph.fetch_attachment_bytes(&att).await {
                    Ok((_, _, bytes)) => {
                        let arc: Arc<[u8]> = Arc::from(bytes);
                        let _ = tx.send(UiMsg::ImageBytesFetched {
                            raw_key,
                            bytes: Arc::clone(&arc),
                        });
                        Ok(arc)
                    }
                    Err(err) => Err(err),
                },
            };
            match raw_bytes {
                Ok(bytes) => {
                    let sniffed = bitefoundry_html::sniff_content_kind(
                        Some(att.content_type.as_str()),
                        att.name.as_deref(),
                        Some(&bytes),
                    );
                    if sniffed == bitefoundry_html::ContentKind::Html {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let title = att
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("HTML ({att_id})"));
                        if caps.is_some() {
                            let _ = tx.send(UiMsg::ImageAutoRoutedToHtml {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                title,
                                html,
                            });
                        } else {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: "HTML content detected (press H to view in HTML Preview)"
                                    .to_string(),
                            });
                        }
                        return;
                    }
                    let decode_res = tokio::task::spawn_blocking(move || match caps {
                        Some(c) => img_preview::decode_and_render_for_protocol(&bytes, cols, rows, bg, c),
                        None => img_preview::decode_and_render_halfblocks(&bytes, cols, rows, bg),
                    })
                    .await;
                    match decode_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::ImageDecoded {
                                cache_key: key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageDecodeFailed {
                        cache_key: key,
                        owner_id: msg_id,
                        att_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_teams_image(
        &mut self,
        msg_id: String,
        att: AttachmentInfo,
        cols: u16,
        rows: u16,
    ) {
        let key = Self::image_cache_key(&msg_id, &att.id, cols, rows);
        self.spawn_attachment_image_decode(key, msg_id, att, cols, rows, (20, 22, 26), None);
    }

    pub fn request_preview_image(
        &mut self,
        target: &PreviewTarget,
        cols: u16,
        rows: u16,
        caps: TerminalGraphicsCaps,
        bg_rgb: (u8, u8, u8),
    ) {
        match target {
            PreviewTarget::Teams { msg_id, attachment } => {
                let key = Self::preview_image_cache_key(
                    msg_id,
                    &attachment.id,
                    cols,
                    rows,
                    caps.protocol,
                );
                self.spawn_attachment_image_decode(
                    key,
                    msg_id.clone(),
                    attachment.clone(),
                    cols,
                    rows,
                    bg_rgb,
                    Some(caps),
                );
            }
        }
    }

    pub fn fetch_html_attachment(&self, ui: &mut UiState, att: AttachmentInfo) {
        let title = att
            .name
            .clone()
            .unwrap_or_else(|| format!("HTML ({})", att.id));
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot fetch HTML attachment without async runtime");
            return;
        }
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("Open HTML: {title}");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.fetch_attachment_bytes(&att).await {
                Ok((fetched_name, _, bytes)) => {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let resolved_title = if fetched_name.trim().is_empty() {
                        title
                    } else {
                        fetched_name
                    };
                    let _ = tx.send(UiMsg::HtmlAttachmentLoaded {
                        title: resolved_title,
                        html,
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
                        toast: Some((format!("Failed to load HTML: {err}"), ToastKind::Error)),
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
        let tx = self.ui_tx.clone();
        let task_label = "Update presence".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.set_my_presence(&availability, &activity, None).await {
                Ok(()) => {
                    let status = graph.get_my_presence().await.unwrap_or(PresenceStatus {
                        user_id: String::new(),
                        availability: availability.clone(),
                        activity,
                    });
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

    fn prepend_older_messages(&mut self, ui: &mut UiState, messages: Vec<MessageItem>) -> usize {
        if messages.is_empty() {
            return 0;
        }
        let existing_ids: HashSet<String> =
            self.messages.iter().map(|m| m.msg_id.clone()).collect();
        let mut new_older: Vec<MessageItem> = messages
            .into_iter()
            .filter(|m| !existing_ids.contains(&m.msg_id))
            .collect();
        if new_older.is_empty() {
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
        prepended_count
    }

    fn emit_scope_warning_once(&mut self, ui: &mut UiState, warning: Option<String>) {
        if self.scope_warned {
            return;
        }
        if let Some(msg) = warning.filter(|s| !s.trim().is_empty()) {
            self.scope_warned = true;
            ui.info(msg);
        }
    }

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
            CoreEvent::ConversationsUpdated(convs) => {
                ui.poll_retrying = false;
                self.conversations = convs;
            }
            CoreEvent::TeamsUpdated(teams) => {
                self.teams = teams;
            }
            CoreEvent::MessagesUpdated {
                conv_key,
                parent_msg_id: _,
                messages,
                notify_candidate,
            } => {
                ui.poll_retrying = false;
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    let prev_len = self.messages.len();
                    let prev_last_id = self.messages.last().map(|m| m.msg_id.clone());
                    let window = self.messages.len().max(80);
                    if let Ok(msgs) = self.storage.list_root_messages(&conv_key, window, None) {
                        merge_optimistic_messages(&mut self.messages, msgs);
                    }
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
                        }
                    }
                }
                if !self.my_user_id.is_empty()
                    && messages.iter().any(|m| !m.mentions.is_empty() || m.deleted)
                {
                    if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
                    {
                        self.activity_mentions = mentions;
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
            } => {
                if let Some(u) = me {
                    self.my_user_id = u.id.clone();
                    self.my_profile = Some(u);
                }
                if let Some(p) = presence {
                    self.my_presence = Some(p);
                }
                self.conversations = conversations;
                self.teams = teams;
                self.activity_mentions = mentions;
                self.presences.extend(presences);
                self.syncing = false;
                self.sync_summary = "Synced".to_string();
                self.emit_scope_warning_once(ui, scope_warning);

                if self.active_conv_key.is_none() {
                    if let Some(first) = self.conversations.first().cloned() {
                        self.select_conversation(ui, first.target);
                    }
                }
            }
            UiMsg::MessagesLoaded {
                conv_key,
                messages,
                members,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    self.loading_messages = false;
                    if !messages.is_empty() {
                        let prev_len = self.messages.len();
                        let prev_last_id = self.messages.last().map(|m| m.msg_id.clone());
                        merge_optimistic_messages(&mut self.messages, messages);
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
                    if !members.is_empty() {
                        self.members = members;
                    }
                }
            }
            UiMsg::MembersLoaded {
                conv_key,
                members,
                clear_loading,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if clear_loading {
                        self.loading_messages = false;
                    }
                    if !members.is_empty() {
                        self.members = members;
                    }
                }
            }
            UiMsg::ClipboardImageEncoded {
                width: _,
                height: _,
                file_name,
                png_bytes,
                preview,
            } => {
                ui.composer.stage_inline_image_with_preview(
                    png_bytes,
                    "image/png",
                    Some(file_name),
                    preview,
                );
                ui.set_focus(Focus::Composer);
            }
            UiMsg::OlderMessagesLoaded {
                conv_key,
                messages,
                reached_eof,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    self.loading_older = false;
                    self.oldest_reached = reached_eof;
                    self.history_error = None;
                    self.prepend_older_messages(ui, messages);
                }
            }
            UiMsg::OlderMessagesFailed {
                conv_key,
                local_messages,
                error,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    self.loading_older = false;
                    self.history_error = Some(error);
                    self.prepend_older_messages(ui, local_messages);
                }
            }
            UiMsg::OlderChatsLoaded {
                conversations,
                reached_eof,
            } => {
                self.loading_older_chats = false;
                self.chats_oldest_reached = reached_eof;
                if !conversations.is_empty() {
                    self.conversations = conversations;
                }
            }
            UiMsg::OlderChatsFailed => {
                self.loading_older_chats = false;
            }
            UiMsg::ThreadLoaded {
                conv_key,
                root_msg_id,
                thread,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key)
                    && self
                        .active_thread
                        .as_ref()
                        .is_some_and(|t| t.root.msg_id == root_msg_id)
                {
                    self.loading_thread = false;
                    self.active_thread = Some(thread);
                }
            }
            UiMsg::MessageSent {
                conv_key,
                parent_msg_id,
                optimistic_id,
                sent,
            } => {
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
                                    }
                                } else if !thread.replies.iter().any(|r| r.msg_id == sent.msg_id) {
                                    thread.replies.push(sent);
                                }
                            }
                        }
                        if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                            root.reply_count = root.reply_count.saturating_add(1);
                        }
                    } else if let Some(opt_id) = optimistic_id.as_deref() {
                        if let Some(pos) = self.messages.iter().position(|m| m.msg_id == opt_id) {
                            if self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                                self.messages.remove(pos);
                            } else {
                                self.messages[pos] = sent;
                            }
                        } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                            self.messages.push(sent);
                            ui.chat_scroll.bottom();
                        }
                    } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                        self.messages.push(sent);
                        ui.chat_scroll.bottom();
                    }
                }
            }
            UiMsg::MessageSendFailed {
                conv_key,
                parent_msg_id,
                optimistic_id,
                text,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = parent_msg_id {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid {
                                thread.replies.retain(|r| r.msg_id != optimistic_id);
                            }
                        }
                    } else {
                        self.messages.retain(|m| m.msg_id != optimistic_id);
                    }
                    if ui.composer.text().trim().is_empty() {
                        ui.composer.load_text(&text);
                    }
                }
            }
            UiMsg::AttachmentSent { conv_key, sent } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = sent.parent_msg_id.clone() {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid
                                && !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                            {
                                thread.replies.push(sent);
                            }
                        }
                    } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                        self.messages.push(sent);
                        ui.chat_scroll.bottom();
                    }
                }
            }
            UiMsg::MessageDeleted {
                conv_key,
                msg_id,
                parent_msg_id,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if parent_msg_id.is_some() {
                        if let Some(thread) = self.active_thread.as_mut() {
                            for r in &mut thread.replies {
                                if r.msg_id == msg_id {
                                    r.deleted = true;
                                }
                            }
                        }
                    } else {
                        for m in &mut self.messages {
                            if m.msg_id == msg_id {
                                m.deleted = true;
                            }
                        }
                    }
                }
            }
            UiMsg::ReactionUpdated {
                conv_key,
                msg_id,
                reaction_type,
                added,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    let apply = |msg: &mut MessageItem| {
                        if let Some(r) = msg
                            .reactions
                            .iter_mut()
                            .find(|r| r.reaction_type == reaction_type)
                        {
                            r.me_reacted = added;
                            if added {
                                r.count = r.count.saturating_add(1);
                            } else {
                                r.count = r.count.saturating_sub(1);
                            }
                            msg.reactions.retain(|r| r.count > 0);
                        } else if added {
                            msg.reactions.push(ReactionSummary {
                                reaction_type: reaction_type.clone(),
                                count: 1,
                                me_reacted: true,
                                users: Vec::new(),
                            });
                        }
                    };
                    if let Some(m) = self.messages.iter_mut().find(|m| m.msg_id == msg_id) {
                        apply(m);
                    }
                    if let Some(thread) = self.active_thread.as_mut() {
                        if thread.root.msg_id == msg_id {
                            apply(&mut thread.root);
                        }
                        if let Some(r) = thread.replies.iter_mut().find(|r| r.msg_id == msg_id) {
                            apply(r);
                        }
                    }
                }
            }
            UiMsg::AttachmentDownloaded(_) => {}
            UiMsg::SearchResults { query, hits } => {
                if self.search_query == query {
                    self.searching = false;
                    self.search_results = hits;
                }
            }
            UiMsg::UserSearchResults { query: _, users } => {
                self.user_searching = false;
                self.user_search_results = users;
            }
            UiMsg::ChatCreated(conv) => {
                if !self.conversations.iter().any(|c| c.conv_key == conv.conv_key) {
                    self.conversations.insert(0, conv.clone());
                }
                self.select_conversation(ui, conv.target);
            }
            UiMsg::PresenceUpdated(status) => {
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
            }
            UiMsg::ImageDecoded {
                cache_key,
                rendered,
            } => {
                self.insert_image_cache(cache_key, rendered);
            }
            UiMsg::ImageBytesFetched { raw_key, bytes } => {
                self.insert_raw_image_cache(raw_key, bytes);
            }
            UiMsg::ImageDecodeFailed {
                cache_key,
                owner_id,
                att_id,
                error,
            } => {
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
            UiMsg::ImageAutoRoutedToHtml {
                cache_key,
                owner_id,
                att_id,
                title,
                html,
            } => {
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
            UiMsg::HtmlAttachmentLoaded { title, html } => {
                ui.close_overlays_of(OverlayKind::HtmlPreview);
                ui.push_overlay(Overlay::html_preview(Some(title), html));
            }
            UiMsg::HtmlServed { url } => {
                if let Some(Overlay::HtmlPreview(preview)) = ui.overlays.last_mut() {
                    preview.last_served_url = Some(url.clone());
                    preview.session.last_status = Some(format!("Serving: {url}"));
                }
            }
            UiMsg::UpdateChecked(_res) => {}
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.daemon_attached = true;
                self.daemon_pid = pid;
                self.syncing = sync_busy;
                self.sync_summary = "Daemon connected".to_string();
                self.sync_ctrl.set_focus(
                    self.active_target.clone(),
                    self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
                );
                if self.my_profile.is_none() {
                    self.spawn_me_profile_if_needed();
                }
            }
            UiMsg::DaemonDetached => {
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
            UiMsg::StartLocalBootstrap => {
                self.daemon_attached = false;
                self.daemon_pid = None;
                self.spawn_bootstrap(ui);
            }
            UiMsg::ProfileLoaded {
                me,
                presence,
                scope_warning,
            } => {
                if let Some(u) = me {
                    self.my_user_id = u.id.clone();
                    self.my_profile = Some(u);
                }
                if let Some(p) = presence {
                    self.my_presence = Some(p);
                }
                self.emit_scope_warning_once(ui, scope_warning);
                if !self.my_user_id.is_empty() && self.activity_mentions.is_empty() {
                    if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
                    {
                        self.activity_mentions = mentions;
                    }
                }
            }
            UiMsg::TaskDone {
                label,
                error,
                toast,
            } => {
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
}

fn merge_optimistic_messages(target: &mut Vec<MessageItem>, incoming: Vec<MessageItem>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::SyncCommand;

    #[test]
    fn image_cache_key_formats_stably() {
        assert_eq!(
            AppState::image_cache_key("msg-1", "att-9", 40, 14),
            "msg-1:att-9:40x14"
        );
    }

    fn make_test_msg(conv_key: &str, msg_id: &str, created_at: &str, body: &str) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: created_at.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: format!("<p>{body}</p>"),
            body_plain: body.to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }
    }

    fn make_test_app() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    #[tokio::test]
    async fn stash_draft_noop_does_not_mark_dirty_and_ignores_stale_loads() {
        let (mut app, mut ui) = make_test_app();
        app.active_conv_key = Some("chat:room-a".to_string());
        app.drafts.clear();

        // Empty composer -> no-op (must not mark dirty or trigger disk write)
        assert!(!app.stash_draft_in_memory(&ui));
        app.stash_draft(&ui);

        // Non-empty draft -> marks dirty; repeating unchanged text -> no-op
        ui.composer.load_text("hello world");
        assert!(app.stash_draft_in_memory(&ui));
        assert_eq!(app.drafts.get("chat:room-a").map(String::as_str), Some("hello world"));
        assert!(!app.stash_draft_in_memory(&ui));

        // Clearing composer -> marks dirty, removes draft; repeating -> no-op
        ui.composer.load_text("");
        assert!(app.stash_draft_in_memory(&ui));
        assert!(!app.drafts.contains_key("chat:room-a"));
        assert!(!app.stash_draft_in_memory(&ui));

        // Stale MessagesLoaded for another room must NOT overwrite active room's messages
        app.loading_messages = true;
        let stale_msg = make_test_msg("chat:room-b", "m1", "2026-11-18T00:00:00Z", "stale");
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessagesLoaded {
                conv_key: "chat:room-b".to_string(),
                messages: vec![stale_msg.clone()],
                members: Vec::new(),
            },
        );
        assert!(app.messages.is_empty());
        assert!(app.loading_messages);

        // Matching MessagesLoaded updates state
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessagesLoaded {
                conv_key: "chat:room-a".to_string(),
                messages: vec![stale_msg],
                members: Vec::new(),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert!(!app.loading_messages);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn optimistic_message_send_with_rollback() {
        let (mut app, mut ui) = make_test_app();

        // Optimistic message send -> replacement -> failure restore
        app.active_conv_key = Some("chat:c1".to_string());
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "c1".to_string(),
        });
        ui.composer.load_text("hello optimistic");
        app.send_composer(&mut ui);
        assert_eq!(app.messages.len(), 1);
        let opt_id = app.messages[0].msg_id.clone();
        assert!(opt_id.starts_with("optimistic-"));
        assert!(ui.composer.text().is_empty());

        let mut real_msg = app.messages[0].clone();
        real_msg.msg_id = "server-1".to_string();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSent {
                conv_key: "chat:c1".to_string(),
                parent_msg_id: None,
                optimistic_id: Some(opt_id),
                sent: real_msg,
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].msg_id, "server-1");

        // Failure rollback restores composer text when empty
        ui.composer.load_text("retry me");
        app.send_composer(&mut ui);
        assert_eq!(app.messages.len(), 2);
        let failed_opt_id = app.messages[1].msg_id.clone();
        assert!(ui.composer.text().is_empty());
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSendFailed {
                conv_key: "chat:c1".to_string(),
                parent_msg_id: None,
                optimistic_id: failed_opt_id,
                text: "retry me".to_string(),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(ui.composer.text(), "retry me");

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    fn make_sized_test_image(label_bytes: usize) -> RenderedTerminalImage {
        RenderedTerminalImage {
            width_px: 10,
            height_px: 10,
            cols: 20,
            rows: 10,
            format_label: "X".repeat(label_bytes),
            lines: vec![],
            protocol: img_preview::TerminalImageProtocol::Halfblock,
            pixel_payload: None,
            scroll_backing: None,
        }
    }

    #[tokio::test]
    async fn image_cache_byte_budget_evicts_and_clear_resets_bytes() {
        let (mut app, _ui) = make_test_app();
        let two_mib = 2 * 1024 * 1024 - 128;

        for i in 0..5 {
            app.insert_image_cache(format!("msg:{i}:20x10"), make_sized_test_image(two_mib));
        }
        assert!(app.image_cache_bytes <= img_preview::MAX_IMAGE_CACHE_BYTES);
        assert_eq!(app.image_cache.len(), 4);

        let expected_bytes: usize = app
            .image_cache
            .values()
            .map(AppState::rendered_image_bytes)
            .sum();
        assert_eq!(app.image_cache_bytes, expected_bytes);

        // Single image larger than MAX_IMAGE_CACHE_BYTES still stays cached as the sole entry.
        let ten_mib = 10 * 1024 * 1024;
        app.insert_image_cache("oversized:0:20x10".to_string(), make_sized_test_image(ten_mib));
        assert_eq!(app.image_cache.len(), 1);
        assert!(app.image_cache.contains_key("oversized:0:20x10"));

        // Clearing resets map, order, and byte counter to zero.
        app.clear_image_caches();
        assert!(app.image_cache.is_empty());
        assert!(app.image_cache_order.is_empty());
        assert_eq!(app.image_cache_bytes, 0);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn older_messages_loaded_deduplicates_prepends_shifts_selection_and_sets_anchor() {
        let (mut app, mut ui) = make_test_app();
        let conv = "chat:c1";
        app.active_conv_key = Some(conv.to_string());
        app.loading_older = true;
        app.messages = vec![
            make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three"),
            make_test_msg(conv, "m4", "2026-11-18T04:00:00Z", "four"),
        ];
        ui.chat_scroll.follow_tail = false;
        ui.chat_scroll.total_lines = 12;
        ui.chat_scroll.offset = 0;
        ui.chat_scroll.selected_idx = Some(0);

        app.handle_ui_msg(
            &mut ui,
            UiMsg::OlderMessagesLoaded {
                conv_key: conv.to_string(),
                messages: vec![
                    make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two"),
                    make_test_msg(conv, "m1", "2026-11-18T01:00:00Z", "one"),
                    make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three"),
                ],
                reached_eof: false,
            },
        );

        assert!(!app.loading_older);
        assert!(!app.oldest_reached);
        assert_eq!(app.history_error, None);
        let ids: Vec<&str> = app.messages.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2", "m3", "m4"]);
        assert_eq!(ui.chat_scroll.selected_idx, Some(2));
        assert_eq!(ui.pending_prepend_anchor, Some((12, 0)));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn older_messages_failed_preserves_oldest_reached_false_and_sets_error_banner() {
        let (mut app, mut ui) = make_test_app();
        let conv = "chat:c1";
        app.active_conv_key = Some(conv.to_string());
        app.loading_older = true;
        app.oldest_reached = false;
        app.messages = vec![make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three")];

        let err_msg = format!("{} missing Chat.Read", crate::auth::INSUFFICIENT_PERMISSIONS_PREFIX);
        app.handle_ui_msg(
            &mut ui,
            UiMsg::OlderMessagesFailed {
                conv_key: conv.to_string(),
                local_messages: vec![make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two")],
                error: err_msg.clone(),
            },
        );

        assert!(!app.loading_older);
        assert!(!app.oldest_reached);
        assert_eq!(app.history_error.as_deref(), Some(err_msg.as_str()));
        let ids: Vec<&str> = app.messages.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["m2", "m3"]);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[test]
    fn merge_optimistic_messages_preserves_loaded_older_history() {
        let conv = "chat:c1";
        let mut target = vec![
            make_test_msg(conv, "m1", "2026-11-18T01:00:00Z", "one"),
            make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two"),
            make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three"),
            make_test_msg(conv, "optimistic-1", "2026-11-18T05:00:00Z", "pending"),
        ];
        let incoming = vec![
            make_test_msg(conv, "m3", "2026-11-18T03:00:00Z", "three-updated"),
            make_test_msg(conv, "m4", "2026-11-18T04:00:00Z", "four"),
        ];

        merge_optimistic_messages(&mut target, incoming);
        let ids: Vec<&str> = target.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2", "m3", "m4", "optimistic-1"]);
        assert_eq!(target[2].body_plain, "three-updated");
    }

    #[tokio::test]
    async fn daemon_attached_skips_duplicate_bootstrap_and_preserves_focus() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        app.my_profile = Some(UserProfile {
            id: "user-1".to_string(),
            display_name: "Alice".to_string(),
            mail: Some("alice@example.com".to_string()),
            user_principal_name: Some("alice@example.com".to_string()),
        });
        app.my_user_id = "user-1".to_string();
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "chat-42".to_string(),
        });

        app.handle_ui_msg(
            &mut ui,
            UiMsg::DaemonAttached {
                pid: Some(4242),
                sync_busy: false,
            },
        );

        assert!(app.daemon_attached);
        assert_eq!(app.daemon_pid, Some(4242));
        assert!(!app.syncing);
        assert_eq!(app.sync_summary, "Daemon connected");
        assert!(ui.tasks.first_running().is_none());

        assert!(nudge_rx.try_recv().is_err(), "unexpected extra bootstrap command");
        assert_eq!(
            focus_rx.borrow().target,
            Some(ConversationTarget::Chat {
                chat_id: "chat-42".to_string(),
            })
        );

        app.handle_ui_msg(&mut ui, UiMsg::DaemonDetached);
        assert!(!app.daemon_attached);
        assert_eq!(app.daemon_pid, None);
        assert_eq!(app.sync_summary, "Local sync");
    }

    #[tokio::test]
    async fn select_conversation_focuses_composer_snapshots_unread_and_resets_transient_state() {
        let (mut app, mut ui) = make_test_app();
        app.conversations = vec![
            ConversationSummary {
                conv_key: "chat:r1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "r1".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Room 1".to_string(),
                topic: None,
                unread_count: 3,
                last_read_at: Some("2026-11-18T09:00:00Z".to_string()),
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            },
            ConversationSummary {
                conv_key: "chat:r2".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "r2".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Room 2".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            },
        ];

        assert_eq!(ui.focus(), Focus::Nav);
        app.select_conversation(
            &mut ui,
            ConversationTarget::Chat {
                chat_id: "r1".to_string(),
            },
        );
        assert_eq!(ui.focus(), Focus::Composer);
        assert_eq!(ui.narrow_pane, NarrowPane::Chat);
        assert_eq!(
            ui.unread_anchor,
            Some(("chat:r1".to_string(), "2026-11-18T09:00:00Z".to_string()))
        );
        assert_eq!(app.conversations[0].unread_count, 0);

        ui.composer.load_text("draft one");
        ui.composer.set_reply(ReplyPreview {
            msg_id: "m1".to_string(),
            author: "Alice".to_string(),
            text: "hi".to_string(),
        });
        ui.composer.subject = "Sub".to_string();
        ui.composer.subject_cursor = 3;
        ui.composer.subject_focused = true;
        ui.composer
            .stage_inline_image(vec![1, 2, 3], "image/png", Some("a.png".to_string()));

        app.select_conversation(
            &mut ui,
            ConversationTarget::Chat {
                chat_id: "r2".to_string(),
            },
        );
        assert_eq!(ui.focus(), Focus::Composer);
        assert!(ui.composer.reply.is_none());
        assert!(ui.composer.subject.is_empty());
        assert_eq!(ui.composer.subject_cursor, 0);
        assert!(!ui.composer.subject_focused);
        assert!(ui.composer.pending_images.is_empty());
        assert_eq!(ui.last_conv_key.as_deref(), Some("chat:r1"));
        assert_eq!(ui.conv_history, vec!["chat:r1", "chat:r2"]);
        assert_eq!(ui.conv_history_pos, 1);

        app.history_back(&mut ui);
        assert_eq!(app.active_conv_key.as_deref(), Some("chat:r1"));
        assert_eq!(ui.composer.text(), "draft one");
        assert_eq!(ui.focus(), Focus::Composer);

        app.history_forward(&mut ui);
        assert_eq!(app.active_conv_key.as_deref(), Some("chat:r2"));
        assert_eq!(ui.focus(), Focus::Composer);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn poll_retry_spinner_and_image_error_cap_and_append_anchor() {
        let (mut app, mut ui) = make_test_app();
        app.sync_summary = "Synced".to_string();

        // Transient poll retry sets poll_retrying without clobbering sync_summary or setting syncing.
        app.handle_core_event(
            &mut ui,
            CoreEvent::SyncStateChanged {
                busy: false,
                summary: crate::models::POLL_RETRY_SUMMARY.to_string(),
            },
        );
        assert!(ui.poll_retrying);
        assert!(!app.syncing);
        assert_eq!(app.sync_summary, "Synced");

        // Synced event clears poll_retrying.
        app.handle_core_event(
            &mut ui,
            CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Synced".to_string(),
            },
        );
        assert!(!ui.poll_retrying);

        // ConversationsUpdated clears poll_retrying.
        ui.poll_retrying = true;
        app.handle_core_event(&mut ui, CoreEvent::ConversationsUpdated(vec![]));
        assert!(!ui.poll_retrying);

        // image_errors is capped at 64 entries and cleared by clear_image_cache.
        for i in 0..70 {
            app.insert_image_error(format!("k{i}"), format!("err{i}"));
        }
        assert_eq!(app.image_errors.len(), 64);
        assert!(!app.image_errors.contains_key("k0"));
        assert!(app.image_errors.contains_key("k69"));

        let gen_before = app.image_cache_gen;
        app.clear_image_cache();
        assert!(app.image_errors.is_empty());
        assert_eq!(app.image_cache_gen, gen_before + 1);

        // Appending messages while scrolled up preserves viewport offset.
        let conv = "chat:c1";
        app.active_conv_key = Some(conv.to_string());
        let m1 = make_test_msg(conv, "m1", "2026-11-18T01:00:00Z", "one");
        let m2 = make_test_msg(conv, "m2", "2026-11-18T02:00:00Z", "two");
        app.storage.upsert_messages(&[m1.clone(), m2]).unwrap();
        app.messages = vec![m1];
        ui.chat_scroll.follow_tail = false;
        ui.chat_scroll.scroll_up = 10;
        ui.chat_scroll.total_lines = 50;
        ui.chat_scroll.offset = 20;

        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![],
                notify_candidate: None,
            },
        );
        assert_eq!(ui.pending_append_anchor, Some(20));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn raw_image_cache_hit_skips_fetch_respects_budget_and_clears() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        // 1. Entry count cap (8 entries) and 16 MiB byte budget enforcement.
        for i in 0..10 {
            let key = AppState::raw_image_cache_key("m", &format!("a{i}"));
            app.insert_raw_image_cache(key, Arc::from(vec![i as u8; 1024]));
        }
        assert_eq!(app.image_raw_cache.len(), MAX_RAW_IMAGE_CACHE_ENTRIES);
        assert_eq!(app.image_raw_bytes, MAX_RAW_IMAGE_CACHE_ENTRIES * 1024);
        assert!(!app.image_raw_cache.contains_key("m:a0"));
        assert!(!app.image_raw_cache.contains_key("m:a1"));
        assert!(app.image_raw_cache.contains_key("m:a9"));

        // Inserting 6 MiB chunks must evict older entries so total stays <= 16 MiB.
        let six_mib = 6 * 1024 * 1024;
        for i in 0..3 {
            let key = AppState::raw_image_cache_key("big", &format!("b{i}"));
            app.insert_raw_image_cache(key, Arc::from(vec![1u8; six_mib]));
        }
        assert!(app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);
        assert_eq!(app.image_raw_cache.len(), 2);
        assert!(!app.image_raw_cache.contains_key("big:b0"));
        assert!(app.image_raw_cache.contains_key("big:b1"));
        assert!(app.image_raw_cache.contains_key("big:b2"));

        // Oversized single entry (>16 MiB) is not cached.
        let seventeen_mib = 17 * 1024 * 1024;
        app.insert_raw_image_cache(
            AppState::raw_image_cache_key("huge", "h0"),
            Arc::from(vec![0u8; seventeen_mib]),
        );
        assert!(app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);
        assert!(!app.image_raw_cache.contains_key("huge:h0"));

        // clear_image_caches resets raw cache, order, and byte counter.
        app.clear_image_caches();
        assert!(app.image_raw_cache.is_empty());
        assert!(app.image_raw_order.is_empty());
        assert_eq!(app.image_raw_bytes, 0);

        // 2. Raw cache hit skips network fetch across zoom & protocol changes.
        let png = super::super::composer::encode_rgba_to_png(4, 4, vec![200u8; 4 * 4 * 4]).unwrap();
        let target = PreviewTarget::Teams {
            msg_id: "msg-offline".to_string(),
            attachment: AttachmentInfo {
                id: "att-1".to_string(),
                content_type: "image/png".to_string(),
                content_url: None,
                name: Some("pic.png".to_string()),
                content_json: None,
                thumbnail_url: None,
            },
        };
        app.insert_raw_image_cache(
            AppState::raw_image_cache_key(target.owner_id(), target.att_id()),
            Arc::from(png),
        );

        let caps_half = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (8, 16),
            cell_px_exact: false,
        };
        app.request_preview_image(&target, 40, 20, caps_half, (37, 42, 61));
        let msg1 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("decode timed out")
            .expect("channel closed");
        match msg1 {
            UiMsg::ImageDecoded { cache_key, rendered } => {
                assert_eq!(
                    cache_key,
                    AppState::preview_image_cache_key("msg-offline", "att-1", 40, 20, TerminalImageProtocol::Halfblock)
                );
                assert_eq!(rendered.protocol, TerminalImageProtocol::Halfblock);
                app.handle_ui_msg(&mut ui, UiMsg::ImageDecoded { cache_key, rendered });
            }
            _ => panic!("expected ImageDecoded on raw cache hit"),
        }

        // Switch protocol to Sixel and zoom to 80x40 offline without any network token!
        let caps_sixel = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: false,
        };
        app.request_preview_image(&target, 80, 40, caps_sixel, (37, 42, 61));
        let msg2 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("sixel decode timed out")
            .expect("channel closed");
        match msg2 {
            UiMsg::ImageDecoded { cache_key, rendered } => {
                assert_eq!(
                    cache_key,
                    AppState::preview_image_cache_key("msg-offline", "att-1", 80, 40, TerminalImageProtocol::Sixel)
                );
                assert_eq!(rendered.protocol, TerminalImageProtocol::Sixel);
                assert!(rendered.pixel_payload.is_some());
            }
            _ => panic!("expected Sixel ImageDecoded on raw cache hit"),
        }

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}
