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
    PresenceStatus, QuotedReply, SearchHit, SyncCursor, TeamSummary, ThreadView,
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
    PeerPresencesLoaded(HashMap<String, PresenceStatus>),
    MessagesLoaded {
        req_seq: u64,
        conv_key: String,
        messages: Vec<MessageItem>,
        members: Vec<MemberInfo>,
    },
    MembersLoaded {
        req_seq: u64,
        conv_key: String,
        members: Vec<MemberInfo>,
        clear_loading: bool,
    },
    ClipboardImageEncoded {
        req_seq: u64,
        conv_key: Option<String>,
        composer_target_thread: bool,
        width: u32,
        height: u32,
        file_name: String,
        png_bytes: Vec<u8>,
        preview: Option<RenderedTerminalImage>,
    },
    OlderMessagesLoaded {
        req_seq: u64,
        conv_key: String,
        messages: Vec<MessageItem>,
        reached_eof: bool,
    },
    OlderMessagesFailed {
        req_seq: u64,
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
        req_seq: u64,
        conv_key: String,
        root_msg_id: String,
        thread: ThreadView,
    },
    ThreadLoadFailed {
        req_seq: u64,
        conv_key: String,
        root_msg_id: String,
        error: String,
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
        optimistic_id: Option<String>,
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
        req_seq: u64,
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
        req_seq: u64,
        conv_key: Option<String>,
        title: String,
        html: String,
        kind: bitefoundry_html::ContentKind,
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
    /// Optional pre-encoded escape sequence payload (e.g. from `HtmlPreviewOverlay`).
    /// When `None`, `tui.rs` resolves the payload from `app.image_cache` via `cache_key`.
    pub payload: Option<String>,
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
    /// Scroll the selected chat message into view on the next render (line ranges are only
    /// known after layout, so a jump cannot do it itself).
    pub pending_reveal_selection: bool,
    /// Scroll the selected thread message into view on the next thread render.
    pub pending_thread_reveal: bool,
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
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub active_pixel_graphics: Vec<(String, Rect, TerminalImageProtocol)>,
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
            pending_reveal_selection: false,
            pending_thread_reveal: false,
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
            pending_pixel_graphics: Vec::new(),
            active_pixel_graphics: Vec::new(),
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

struct AttachmentDecodeJob {
    key: String,
    msg_id: String,
    att: AttachmentInfo,
    fit: img_preview::FitBox,
    cell_px: (u16, u16),
    bg: (u8, u8, u8),
    caps: Option<TerminalGraphicsCaps>,
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

    fn select_conversation_internal(
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
                futures::future::join_all(futs).await;
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

    pub fn inline_image_bg(theme: &Theme) -> (u8, u8, u8) {
        let fallback = img_preview::color_to_rgb(theme.bg_card, (20, 22, 26));
        img_preview::color_to_rgb(theme.bg_canvas, fallback)
    }

    pub fn modal_image_bg(theme: &Theme) -> (u8, u8, u8) {
        img_preview::color_to_rgb(theme.bg_elevated, (37, 42, 61))
    }

    pub fn image_cache_key(
        owner_id: &str,
        att_id: &str,
        cols: u16,
        rows: u16,
        cell_px: (u16, u16),
        bg_rgb: (u8, u8, u8),
    ) -> String {
        format!(
            "{owner_id}:{att_id}:{cols}x{rows}:{}x{}:{:02x}{:02x}{:02x}",
            cell_px.0, cell_px.1, bg_rgb.0, bg_rgb.1, bg_rgb.2
        )
    }

    pub fn preview_image_cache_key(
        owner_id: &str,
        att_id: &str,
        view_cols: u16,
        view_rows: u16,
        zoom_pct: u16,
        cell_px: (u16, u16),
        protocol: TerminalImageProtocol,
    ) -> String {
        format!(
            "{owner_id}:{att_id}:{view_cols}x{view_rows}:{zoom_pct}%:{}x{}:{}",
            cell_px.0,
            cell_px.1,
            protocol.as_str()
        )
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

    /// Clears decoded/rendered image entries while preserving raw fetched bytes (e.g. on theme switch).
    pub fn clear_rendered_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
        self.image_errors.clear();
        self.image_errors_order.clear();
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
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

    fn spawn_attachment_image_decode(&mut self, job: AttachmentDecodeJob) {
        let AttachmentDecodeJob {
            key,
            msg_id,
            att,
            fit,
            cell_px,
            bg,
            caps,
        } = job;
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
                        Some(c) => img_preview::decode_and_render_for_protocol(&bytes, fit, bg, c),
                        None => img_preview::decode_and_render_halfblocks_with_cell_px(
                            &bytes, fit, bg, cell_px,
                        ),
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
        cell_px: (u16, u16),
        bg_rgb: (u8, u8, u8),
    ) {
        let key = Self::image_cache_key(&msg_id, &att.id, cols, rows, cell_px, bg_rgb);
        self.spawn_attachment_image_decode(AttachmentDecodeJob {
            key,
            msg_id,
            att,
            fit: img_preview::FitBox::fixed(cols, rows),
            cell_px,
            bg: bg_rgb,
            caps: None,
        });
    }

    pub fn request_preview_image(
        &mut self,
        target: &PreviewTarget,
        fit: img_preview::FitBox,
        caps: TerminalGraphicsCaps,
        bg_rgb: (u8, u8, u8),
    ) {
        match target {
            PreviewTarget::Teams { msg_id, attachment } => {
                let key = Self::preview_image_cache_key(
                    msg_id,
                    &attachment.id,
                    fit.view_cols,
                    fit.view_rows,
                    fit.zoom_pct,
                    caps.cell_px,
                    caps.protocol,
                );
                self.spawn_attachment_image_decode(AttachmentDecodeJob {
                    key,
                    msg_id: msg_id.clone(),
                    att: attachment.clone(),
                    fit,
                    cell_px: caps.cell_px,
                    bg: bg_rgb,
                    caps: Some(caps),
                });
            }
        }
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

    /// True when a thread answer belongs to the fetch in flight or to the thread on screen.
    fn thread_msg_is_current(&self, conv_key: &str, root_msg_id: &str) -> bool {
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
    fn cached_history_exhausted(&self, conv_key: &str) -> bool {
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
    /// the loaded window, the cached rows between it and the window are loaded first.
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

    fn prepend_older_messages(&mut self, ui: &mut UiState, messages: Vec<MessageItem>) -> usize {
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
        for msg in messages {
            if let Some(&pos) = existing_pos.get(&msg.msg_id) {
                self.messages[pos] = msg;
            } else {
                existing_pos.insert(msg.msg_id.clone(), usize::MAX);
                new_older.push(msg);
            }
        }
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

    fn apply_active_read_guard(&self, convs: &mut [ConversationSummary]) {
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

    fn update_conversation_preview_from_sent(&mut self, conv_key: &str, sent: &MessageItem) {
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
            }
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

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BootstrapDone {
                me,
                presence,
                mut conversations,
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
                let prev_row = nav_pane::nav_row_at(self, ui, ui.nav_selected);
                self.apply_active_read_guard(&mut conversations);
                self.conversations = conversations;
                self.teams = teams;
                self.activity_mentions = mentions;
                self.presences.extend(presences);
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
            UiMsg::PeerPresencesLoaded(presences) => {
                self.presences.extend(presences);
            }
            UiMsg::MessagesLoaded {
                req_seq,
                conv_key,
                messages,
                members,
            } => {
                if req_seq == self.conv_req_seq
                    && self.active_conv_key.as_deref() == Some(&conv_key)
                {
                    self.loading_messages = false;
                    if !messages.is_empty() {
                        let prev_len = self.messages.len();
                        let prev_last_id = self.messages.last().map(|m| m.msg_id.clone());
                        merge_optimistic_messages(&mut self.messages, messages);
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
            UiMsg::MembersLoaded {
                req_seq,
                conv_key,
                members,
                clear_loading,
            } => {
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
            UiMsg::ClipboardImageEncoded {
                req_seq,
                conv_key,
                composer_target_thread,
                width: _,
                height: _,
                file_name,
                png_bytes,
                preview,
            } => {
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
            UiMsg::OlderMessagesLoaded {
                req_seq,
                conv_key,
                messages,
                reached_eof,
            } => {
                if req_seq == self.older_req_seq
                    && self.active_conv_key.as_deref() == Some(&conv_key)
                {
                    self.loading_older = false;
                    self.oldest_reached = reached_eof;
                    self.history_error = None;
                    self.prepend_older_messages(ui, messages);
                }
            }
            UiMsg::OlderMessagesFailed {
                req_seq,
                conv_key,
                local_messages,
                error,
            } => {
                if req_seq == self.older_req_seq
                    && self.active_conv_key.as_deref() == Some(&conv_key)
                {
                    self.loading_older = false;
                    self.history_error = Some(error);
                    self.prepend_older_messages(ui, local_messages);
                }
            }
            UiMsg::OlderChatsLoaded {
                mut conversations,
                reached_eof,
            } => {
                self.loading_older_chats = false;
                self.chats_oldest_reached = reached_eof;
                if !conversations.is_empty() {
                    let prev_row = nav_pane::nav_row_at(self, ui, ui.nav_selected);
                    self.apply_active_read_guard(&mut conversations);
                    self.conversations = conversations;
                    preserve_nav_selection(self, ui, prev_row);
                }
            }
            UiMsg::OlderChatsFailed => {
                self.loading_older_chats = false;
            }
            UiMsg::ThreadLoaded {
                req_seq,
                conv_key,
                root_msg_id,
                mut thread,
            } => {
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
                        root.reply_count = reply_count;
                    }
                    let target_reply_id = self.pending_thread_reply_id.take();
                    self.active_thread = Some(thread);
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
            UiMsg::ThreadLoadFailed {
                req_seq,
                conv_key,
                root_msg_id,
                error,
            } => {
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
            UiMsg::MessageSent {
                conv_key,
                parent_msg_id,
                optimistic_id,
                sent,
            } => {
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
                            }
                        }
                        if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                            if let Ok(Some(db_root)) = self.storage.get_message(&conv_key, &pid) {
                                root.reply_count = db_root.reply_count;
                            } else {
                                root.reply_count = root.reply_count.saturating_add(1);
                            }
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
                    if let Some(ref opt_id) = optimistic_id {
                        if let Some(ref pid) = parent_msg_id {
                            if let Some(thread) = self.active_thread.as_mut() {
                                if &thread.root.msg_id == pid {
                                    thread.replies.retain(|r| &r.msg_id != opt_id);
                                }
                            }
                            clamp_thread_selection(
                                self.active_thread.as_ref(),
                                &mut ui.thread_scroll.selected_idx,
                            );
                        } else {
                            self.messages.retain(|m| &m.msg_id != opt_id);
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
            UiMsg::AttachmentSent { conv_key, sent } => {
                self.update_conversation_preview_from_sent(&conv_key, &sent);
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = sent.parent_msg_id.clone() {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid
                                && !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                            {
                                thread.replies.push(sent);
                                ui.thread_scroll.bottom();
                            }
                        }
                        if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                            if let Ok(Some(db_root)) = self.storage.get_message(&conv_key, &pid) {
                                root.reply_count = db_root.reply_count;
                            } else {
                                root.reply_count = root.reply_count.saturating_add(1);
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
                    if let Some(ref pid) = parent_msg_id {
                        if let Some(thread) = self.active_thread.as_mut() {
                            for r in &mut thread.replies {
                                if r.msg_id == msg_id {
                                    mark_message_deleted_in_memory(r);
                                }
                            }
                        }
                        if let Some(root) = self.messages.iter_mut().find(|m| &m.msg_id == pid) {
                            if let Ok(Some(db_root)) = self.storage.get_message(&conv_key, pid) {
                                root.reply_count = db_root.reply_count;
                            }
                        }
                    } else {
                        for m in &mut self.messages {
                            if m.msg_id == msg_id {
                                mark_message_deleted_in_memory(m);
                            }
                        }
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == msg_id {
                                mark_message_deleted_in_memory(&mut thread.root);
                            }
                        }
                    }
                }
                if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == conv_key) {
                    if conv.last_msg_id.as_deref() == Some(msg_id.as_str()) {
                        conv.last_msg_preview = Some("(deleted message)".to_string());
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
                    let my_name = self.my_profile.as_ref().map(|p| p.display_name.as_str());
                    if let Some(m) = self.messages.iter_mut().find(|m| m.msg_id == msg_id) {
                        crate::storage::apply_reaction_toggle(m, &reaction_type, added, my_name);
                    }
                    if let Some(thread) = self.active_thread.as_mut() {
                        if thread.root.msg_id == msg_id {
                            crate::storage::apply_reaction_toggle(
                                &mut thread.root,
                                &reaction_type,
                                added,
                                my_name,
                            );
                        }
                        if let Some(r) = thread.replies.iter_mut().find(|r| r.msg_id == msg_id) {
                            crate::storage::apply_reaction_toggle(
                                r,
                                &reaction_type,
                                added,
                                my_name,
                            );
                        }
                    }
                }
            }
            UiMsg::AttachmentDownloaded(_) => {}
            UiMsg::SearchResults { query, hits } => {
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
            UiMsg::UserSearchResults {
                req_seq,
                query,
                users,
            } => {
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
            UiMsg::HtmlAttachmentLoaded {
                req_seq,
                conv_key,
                title,
                html,
                kind,
            } => {
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

fn mark_message_deleted_in_memory(msg: &mut MessageItem) {
    msg.deleted = true;
    msg.body_html.clear();
    msg.body_plain = "(deleted message)".to_string();
    msg.attachments.clear();
    msg.reactions.clear();
    msg.mentions.clear();
    msg.subject = None;
}

fn clamp_chat_selection(messages_len: usize, selected_idx: &mut Option<usize>) {
    if let Some(sel) = *selected_idx {
        *selected_idx = if messages_len == 0 {
            None
        } else {
            Some(sel.min(messages_len - 1))
        };
    }
}

fn clamp_thread_selection(thread: Option<&ThreadView>, selected_idx: &mut Option<usize>) {
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
fn is_expired_page_link_error(err: &anyhow::Error) -> bool {
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

fn cmp_message_order(a: &MessageItem, b: &MessageItem) -> std::cmp::Ordering {
    a.created_at
        .cmp(&b.created_at)
        .then_with(|| a.msg_id.cmp(&b.msg_id))
}

fn find_incremental_insert_pos(target: &[MessageItem], msg: &MessageItem) -> usize {
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

fn apply_changed_messages_incremental(
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
pub(super) mod tests {
    use super::*;
    use crate::sync::SyncCommand;

    #[test]
    fn image_cache_key_formats_stably() {
        assert_eq!(
            AppState::image_cache_key("msg-1", "att-9", 40, 14, (10, 20), (30, 34, 50)),
            "msg-1:att-9:40x14:10x20:1e2232"
        );
    }

    #[test]
    fn test_preview_cache_key_includes_cell_px() {
        let k1 = AppState::preview_image_cache_key(
            "msg-1",
            "att-9",
            100,
            30,
            100,
            (10, 20),
            TerminalImageProtocol::Sixel,
        );
        let k2 = AppState::preview_image_cache_key(
            "msg-1",
            "att-9",
            100,
            30,
            100,
            (16, 32),
            TerminalImageProtocol::Sixel,
        );
        let k3 = AppState::preview_image_cache_key(
            "msg-1",
            "att-9",
            100,
            30,
            150,
            (10, 20),
            TerminalImageProtocol::Sixel,
        );
        assert_ne!(k1, k2);
        assert_ne!(k1, k3);
        assert!(k1.contains("10x20"));
        assert!(k2.contains("16x32"));
        assert!(k3.contains("150%"));
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

    pub(in crate::ui) fn make_test_app() -> (AppState, UiState) {
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
                req_seq: app.conv_req_seq,
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
                req_seq: app.conv_req_seq,
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
                optimistic_id: Some(failed_opt_id),
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

    fn seed_root_messages(app: &AppState, conv: &str, count: usize) {
        let base = chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z").unwrap();
        let msgs: Vec<MessageItem> = (0..count)
            .map(|i| {
                let ts = (base + chrono::Duration::minutes(i as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                make_test_msg(conv, &format!("m{i:04}"), &ts, "body")
            })
            .collect();
        app.storage.upsert_messages(&msgs).unwrap();
    }

    fn save_history_eof(app: &AppState, conv: &str) {
        app.storage
            .save_sync_cursor(&SyncCursor {
                scope_key: SyncCursor::history_scope(conv),
                delta_link: None,
                oldest_msg_ts: Some(SyncCursor::HISTORY_EOF.to_string()),
                synced_at: "2026-11-20T00:00:00Z".to_string(),
            })
            .unwrap();
    }

    // TEAMS-04: an EOF cursor means Graph has no older page, not that the 80-row window
    // shows everything that is cached.
    #[tokio::test]
    async fn regression_teams_04_oldest_reached_needs_exhausted_cache() {
        let (mut app, mut ui) = make_test_app();
        let chat = |id: &str| ConversationTarget::Chat {
            chat_id: id.to_string(),
        };
        seed_root_messages(&app, "chat:big", 200);
        save_history_eof(&app, "chat:big");
        seed_root_messages(&app, "chat:small", 50);
        save_history_eof(&app, "chat:small");
        seed_root_messages(&app, "chat:open", 200);

        app.select_conversation(&mut ui, chat("big"));
        assert_eq!(app.messages.len(), 80);
        assert!(!app.oldest_reached, "120 older rows are still in the cache");

        app.select_conversation(&mut ui, chat("small"));
        assert_eq!(app.messages.len(), 50);
        assert!(app.oldest_reached);

        app.select_conversation(&mut ui, chat("open"));
        assert_eq!(app.messages.len(), 80);
        assert!(!app.oldest_reached, "no EOF cursor: unchanged");

        // An empty window with an EOF cursor stays "reached".
        save_history_eof(&app, "chat:empty");
        app.messages.clear();
        assert!(app.cached_history_exhausted("chat:empty"));
        assert!(!app.cached_history_exhausted("chat:never-synced"));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    // TEAMS-11: the drawer must leave "loading" whether the fetch answers or fails, also
    // when the root was not cached when it opened.
    #[tokio::test]
    async fn regression_teams_11_thread_drawer_leaves_loading_state() {
        let (mut app, mut ui) = make_test_app();
        let conv = "chan:t1:ch1";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Channel {
            team_id: "t1".to_string(),
            channel_id: "ch1".to_string(),
        });
        let thread = |root_id: &str| {
            let mut reply = make_test_msg(conv, &format!("{root_id}-r1"), "2026-11-18T01:01:00Z", "reply");
            reply.parent_msg_id = Some(root_id.to_string());
            ThreadView {
                conv_key: conv.to_string(),
                root: make_test_msg(conv, root_id, "2026-11-18T01:00:00Z", "root"),
                replies: vec![reply],
            }
        };
        let loaded = |req_seq: u64, root_id: &str| UiMsg::ThreadLoaded {
            req_seq,
            conv_key: conv.to_string(),
            root_msg_id: root_id.to_string(),
            thread: thread(root_id),
        };
        let failed = |req_seq: u64, root_id: &str| UiMsg::ThreadLoadFailed {
            req_seq,
            conv_key: conv.to_string(),
            root_msg_id: root_id.to_string(),
            error: "503 Service Unavailable".to_string(),
        };
        let shown = |app: &AppState| app.active_thread.as_ref().map(|t| t.root.msg_id.clone());

        // 1) Uncached root, then the answer arrives.
        app.open_thread_by_id(&mut ui, "root-a".to_string());
        let seq_a = app.thread_req_seq;
        assert!(app.loading_thread);
        assert!(app.active_thread.is_none());
        assert_eq!(app.pending_thread, Some((conv.to_string(), "root-a".to_string())));
        app.handle_ui_msg(&mut ui, loaded(seq_a, "root-a"));
        assert!(!app.loading_thread);
        assert!(app.pending_thread.is_none());
        assert_eq!(shown(&app).as_deref(), Some("root-a"));

        // 2) Another uncached root: the previous thread is not shown under it, and a late
        //    answer for the thread the user left is dropped.
        app.open_thread_by_id(&mut ui, "root-b".to_string());
        let seq_b = app.thread_req_seq;
        assert!(app.active_thread.is_none());
        app.handle_ui_msg(&mut ui, loaded(seq_a, "root-a"));
        assert!(app.active_thread.is_none());
        assert!(app.loading_thread);

        // 3) The fetch fails with nothing to show: loading ends, drawer closes, error shown.
        app.handle_ui_msg(&mut ui, failed(seq_b, "root-b"));
        assert!(!app.loading_thread);
        assert!(app.pending_thread.is_none());
        assert!(app.active_thread.is_none());
        assert!(!ui.thread_drawer_open);
        assert!(ui
            .toasts
            .current()
            .is_some_and(|t| t.message.contains("Thread sync failed")));

        // 4) The fetch fails while a cached thread is on screen: it stays.
        let cached = thread("root-c");
        let mut rows = vec![cached.root.clone()];
        rows.extend(cached.replies.clone());
        app.storage.upsert_messages(&rows).unwrap();
        app.open_thread_by_id(&mut ui, "root-c".to_string());
        let seq_c = app.thread_req_seq;
        assert_eq!(shown(&app).as_deref(), Some("root-c"));
        app.handle_ui_msg(&mut ui, failed(seq_c, "root-c"));
        assert!(!app.loading_thread);
        assert!(ui.thread_drawer_open);
        assert_eq!(shown(&app).as_deref(), Some("root-c"));

        // 5) Switching conversation drops the answer of the fetch in flight.
        app.close_thread(&mut ui);
        app.open_thread_by_id(&mut ui, "root-d".to_string());
        let seq_d = app.thread_req_seq;
        seed_root_messages(&app, "chat:other", 3);
        app.select_conversation(
            &mut ui,
            ConversationTarget::Chat {
                chat_id: "other".to_string(),
            },
        );
        assert!(app.pending_thread.is_none());
        assert!(!app.loading_thread);
        app.handle_ui_msg(&mut ui, loaded(seq_d, "root-d"));
        assert!(app.active_thread.is_none());

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
                req_seq: app.older_req_seq,
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
                req_seq: app.older_req_seq,
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
                changed_msg_ids: None,
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
        let fit_half = img_preview::FitBox::new(40, 20, 100, true);
        app.request_preview_image(&target, fit_half, caps_half, (37, 42, 61));
        let msg1 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("decode timed out")
            .expect("channel closed");
        match msg1 {
            UiMsg::ImageDecoded { cache_key, rendered } => {
                assert_eq!(
                    cache_key,
                    AppState::preview_image_cache_key(
                        "msg-offline",
                        "att-1",
                        40,
                        20,
                        100,
                        (8, 16),
                        TerminalImageProtocol::Halfblock
                    )
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
        let fit_sixel = img_preview::FitBox::new(80, 40, 125, true);
        app.request_preview_image(&target, fit_sixel, caps_sixel, (37, 42, 61));
        let msg2 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("sixel decode timed out")
            .expect("channel closed");
        match msg2 {
            UiMsg::ImageDecoded { cache_key, rendered } => {
                assert_eq!(
                    cache_key,
                    AppState::preview_image_cache_key(
                        "msg-offline",
                        "att-1",
                        80,
                        40,
                        125,
                        (8, 16),
                        TerminalImageProtocol::Sixel
                    )
                );
                assert_eq!(rendered.protocol, TerminalImageProtocol::Sixel);
                assert!(rendered.pixel_payload.is_some());
            }
            _ => panic!("expected Sixel ImageDecoded on raw cache hit"),
        }

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn incremental_chat_updates_read_only_changed_rows_for_80_and_830_windows() {
        use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

        for initial_count in [80usize, 830usize] {
            let (mut app, mut ui) = make_test_app();
            let conv = "chat:perf-room";
            app.active_conv_key = Some(conv.to_string());
            app.active_target = Some(ConversationTarget::Chat {
                chat_id: "perf-room".to_string(),
            });

            let mut seed = Vec::with_capacity(initial_count);
            for i in 0..initial_count {
                let ts = format!(
                    "2026-11-20T{:02}:{:02}:{:02}Z",
                    i / 3600,
                    (i / 60) % 60,
                    i % 60
                );
                seed.push(make_test_msg(
                    conv,
                    &format!("m{:04}", i + 1),
                    &ts,
                    &format!("msg {}", i + 1),
                ));
            }
            app.storage.upsert_messages(&seed).unwrap();
            app.messages = seed;

            // 1) 0 changed rows -> 0 SQLite row reads.
            reset_mapped_message_rows();
            app.handle_core_event(
                &mut ui,
                CoreEvent::MessagesUpdated {
                    conv_key: conv.to_string(),
                    parent_msg_id: None,
                    messages: Vec::new(),
                    notify_candidate: None,
                    changed_msg_ids: Some(Vec::new()),
                },
            );
            assert_eq!(mapped_message_rows(), 0, "0 changes must read 0 rows");
            assert_eq!(app.messages.len(), initial_count);

            // 2) 1 new root message while scrolled up -> reads exactly 1 row & sets append anchor.
            let new_msg = make_test_msg(
                conv,
                &format!("m{:04}", initial_count + 1),
                "2026-11-20T23:00:00Z",
                "newest arrival",
            );
            app.storage
                .upsert_messages(std::slice::from_ref(&new_msg))
                .unwrap();
            ui.chat_scroll.follow_tail = false;
            ui.chat_scroll.scroll_up = 12;
            ui.chat_scroll.total_lines = 400;
            ui.chat_scroll.offset = 100;
            ui.pending_append_anchor = None;

            reset_mapped_message_rows();
            app.handle_core_event(
                &mut ui,
                CoreEvent::MessagesUpdated {
                    conv_key: conv.to_string(),
                    parent_msg_id: None,
                    messages: vec![new_msg.clone()],
                    notify_candidate: None,
                    changed_msg_ids: Some(vec![new_msg.msg_id.clone()]),
                },
            );
            assert_eq!(
                mapped_message_rows(),
                1,
                "1 new message in {initial_count}-row chat must read exactly 1 row"
            );
            assert_eq!(app.messages.len(), initial_count + 1);
            assert_eq!(app.messages.last().unwrap().msg_id, new_msg.msg_id);
            assert_eq!(ui.pending_append_anchor, Some(100));

            app.sync_ctrl.request(SyncCommand::Shutdown);
        }
    }

    #[tokio::test]
    async fn incremental_chat_updates_preserve_order_edits_deletes_optimistic_and_snapshots() {
        use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

        let (mut app, mut ui) = make_test_app();
        let conv = "chat:group-1";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "group-1".to_string(),
        });
        app.conversations.push(ConversationSummary {
            conv_key: conv.to_string(),
            target: ConversationTarget::Chat {
                chat_id: "group-1".to_string(),
            },
            kind: ConversationKind::Group,
            team_id: None,
            team_name: None,
            title: "Group Chat".to_string(),
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
        });

        // 1) Empty window uses snapshot fallback even if changed_msg_ids is Some([]).
        let m1 = make_test_msg(conv, "m1", "2026-11-20T01:00:00Z", "one");
        let m3 = make_test_msg(conv, "m3", "2026-11-20T02:00:00Z", "three");
        app.storage
            .upsert_messages(&[m1.clone(), m3.clone()])
            .unwrap();
        assert!(app.messages.is_empty());
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: Vec::new(),
                notify_candidate: None,
                changed_msg_ids: Some(Vec::new()),
            },
        );
        assert_eq!(
            app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m3"]
        );

        // 2) Same-timestamp tie-break (m2 < m3 at identical timestamp) + selection shift.
        let m2 = make_test_msg(conv, "m2", "2026-11-20T02:00:00Z", "two");
        app.storage.upsert_messages(std::slice::from_ref(&m2)).unwrap();
        ui.chat_scroll.selected_idx = Some(1); // pointing at m3
        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![m2.clone()],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["m2".to_string()]),
            },
        );
        assert_eq!(mapped_message_rows(), 1);
        assert_eq!(
            app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2", "m3"]
        );
        assert_eq!(ui.chat_scroll.selected_idx, Some(2)); // still pointing at m3

        // 3) Edit, soft-delete, and duplicate delivery update in-place without duplicating.
        let mut m2_edited = m2.clone();
        m2_edited.body_plain = "two edited".to_string();
        m2_edited.body_html = "<p>two edited</p>".to_string();
        m2_edited.last_modified_at = Some("2026-11-20T02:05:00Z".to_string());
        let mut m3_deleted = m3.clone();
        m3_deleted.deleted = true;
        app.storage
            .upsert_messages(&[m2_edited.clone(), m3_deleted.clone()])
            .unwrap();
        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![m2_edited, m3_deleted],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["m2".to_string(), "m3".to_string(), "m2".to_string()]),
            },
        );
        assert_eq!(mapped_message_rows(), 2);
        assert_eq!(app.messages.len(), 3);
        assert_eq!(app.messages[1].body_plain, "two edited");
        assert!(app.messages[2].deleted);

        // 4) Optimistic reconciliation:
        //    (a) Poll arrives before MessageSent -> server row inserted before optimistic, then MessageSent removes optimistic.
        let opt = make_test_msg(conv, "optimistic-1", "2026-11-20T03:00:00Z", "sending");
        app.messages.push(opt);
        let m4 = make_test_msg(conv, "m4", "2026-11-20T02:59:59Z", "sending");
        app.storage.upsert_messages(std::slice::from_ref(&m4)).unwrap();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![m4.clone()],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["m4".to_string()]),
            },
        );
        assert_eq!(
            app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2", "m3", "m4", "optimistic-1"]
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSent {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                optimistic_id: Some("optimistic-1".to_string()),
                sent: m4.clone(),
            },
        );
        assert_eq!(
            app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2", "m3", "m4"]
        );

        //    (b) MessageSent arrives before duplicate poll -> poll replaces in-place without duplicating.
        let opt2 = make_test_msg(conv, "optimistic-2", "2026-11-20T04:00:00Z", "second");
        app.messages.push(opt2);
        let m5 = make_test_msg(conv, "m5", "2026-11-20T04:00:01Z", "second");
        app.storage.upsert_messages(std::slice::from_ref(&m5)).unwrap();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSent {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                optimistic_id: Some("optimistic-2".to_string()),
                sent: m5.clone(),
            },
        );
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![m5.clone()],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["m5".to_string()]),
            },
        );
        assert_eq!(
            app.messages.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2", "m3", "m4", "m5"]
        );

        // 5) Out-of-window old message is NOT injected into a full (>=80) window when !oldest_reached.
        let mut full_window = Vec::with_capacity(80);
        for i in 100..180 {
            full_window.push(make_test_msg(
                conv,
                &format!("w{i:03}"),
                &format!("2026-11-20T10:{:02}:{:02}Z", (i - 100) / 60, (i - 100) % 60),
                "win",
            ));
        }
        let very_old = make_test_msg(conv, "w001", "2026-11-19T00:00:00Z", "ancient");
        app.storage.upsert_messages(&full_window).unwrap();
        app.storage
            .upsert_messages(std::slice::from_ref(&very_old))
            .unwrap();
        app.messages = full_window;
        app.oldest_reached = false;
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![very_old],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["w001".to_string()]),
            },
        );
        assert_eq!(app.messages.len(), 80);
        assert_eq!(app.messages[0].msg_id, "w100");

        // 6) Legacy IPC event (changed_msg_ids: None) falls back to snapshot list_root_messages.
        let m_snap = make_test_msg(conv, "w999", "2026-11-20T23:59:59Z", "snapshot fallback");
        app.storage
            .upsert_messages(std::slice::from_ref(&m_snap))
            .unwrap();
        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: Vec::new(),
                notify_candidate: None,
                changed_msg_ids: None,
            },
        );
        assert!(mapped_message_rows() >= 80);
        assert_eq!(app.messages.last().unwrap().msg_id, "w999");

        // 7) Mentions refresh on changed mention row, skipped on 0-change event.
        app.my_user_id = "me-mention".to_string();
        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: Vec::new(),
                notify_candidate: None,
                changed_msg_ids: Some(Vec::new()),
            },
        );
        assert_eq!(mapped_message_rows(), 0);
        assert!(app.activity_mentions.is_empty());

        let mut mention_msg = make_test_msg(conv, "w1000", "2026-11-20T23:59:59.5Z", "@Me check");
        mention_msg.mentions.push(crate::models::MentionInfo {
            id: 0,
            user_id: "me-mention".to_string(),
            display_name: "Me".to_string(),
        });
        app.storage
            .upsert_messages(std::slice::from_ref(&mention_msg))
            .unwrap();
        ui.notify = false;
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![mention_msg.clone()],
                notify_candidate: Some(NotificationCandidate {
                    conv_key: conv.to_string(),
                    conv_title: "Group Chat".to_string(),
                    sender: "Alice".to_string(),
                    preview: "@Me check".to_string(),
                }),
                changed_msg_ids: Some(vec!["w1000".to_string()]),
            },
        );
        assert_eq!(app.activity_mentions.len(), 1);
        assert_eq!(app.activity_mentions[0].msg_id, "w1000");

        // 8) Channel conversation still uses snapshot path + refreshes open thread.
        let chan_key = "chan:t1:ch1";
        let root = make_test_msg(chan_key, "r1", "2026-11-20T01:00:00Z", "root");
        let mut reply = make_test_msg(chan_key, "r1-rep1", "2026-11-20T01:05:00Z", "reply");
        reply.parent_msg_id = Some("r1".to_string());
        app.storage
            .upsert_messages(&[root.clone(), reply])
            .unwrap();
        app.active_conv_key = Some(chan_key.to_string());
        app.active_target = Some(ConversationTarget::Channel {
            team_id: "t1".to_string(),
            channel_id: "ch1".to_string(),
        });
        app.messages = vec![root.clone()];
        app.active_thread = Some(ThreadView {
            conv_key: chan_key.to_string(),
            root,
            replies: Vec::new(),
        });
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: chan_key.to_string(),
                parent_msg_id: Some("r1".to_string()),
                messages: Vec::new(),
                notify_candidate: None,
                changed_msg_ids: Some(vec!["r1-rep1".to_string()]),
            },
        );
        assert_eq!(app.active_thread.as_ref().unwrap().replies.len(), 1);
        assert_eq!(app.active_thread.as_ref().unwrap().replies[0].msg_id, "r1-rep1");

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn channel_root_reaction_maps_one_row_and_reply_deletion_refreshes_parent() {
        use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

        let (mut app, mut ui) = make_test_app();
        let conv_key = "chan:t1:ch1";
        app.active_conv_key = Some(conv_key.into());
        app.active_target = ConversationTarget::from_conv_key(conv_key);
        let roots: Vec<_> = (1..=1580)
            .map(|i| {
                make_test_msg(
                    conv_key,
                    &format!("root-{i:04}"),
                    "2026-11-20T01:00:00Z",
                    "root",
                )
            })
            .collect();
        let root_index = roots.len() - 1;
        let mut changed_root = roots[root_index].clone();
        let mut reply = make_test_msg(conv_key, "reply", "2026-11-20T01:05:00Z", "reply");
        reply.parent_msg_id = Some(changed_root.msg_id.clone());
        app.storage.upsert_messages(&roots).unwrap();
        app.storage.upsert_messages(&[reply.clone()]).unwrap();
        app.messages = app.storage.list_root_messages(conv_key, 1580, None).unwrap();
        ui.chat_scroll.selected_idx = Some(root_index);
        assert_eq!(app.messages[root_index].reply_count, 1);

        let event = |message: MessageItem| CoreEvent::MessagesUpdated {
            conv_key: conv_key.into(),
            parent_msg_id: None,
            changed_msg_ids: Some(vec![message.msg_id.clone()]),
            messages: vec![message],
            notify_candidate: None,
        };
        changed_root.reactions.push(crate::models::ReactionSummary {
            reaction_type: "like".into(),
            count: 1,
            me_reacted: false,
            users: vec!["someone".into()],
        });
        app.storage.upsert_messages(&[changed_root.clone()]).unwrap();
        reset_mapped_message_rows();
        app.handle_core_event(&mut ui, event(changed_root));
        assert_eq!(mapped_message_rows(), 1);
        assert_eq!(app.messages[root_index].reactions[0].count, 1);
        assert_eq!(app.messages[root_index].reply_count, 1);
        assert_eq!(ui.chat_scroll.selected_idx, Some(root_index));
        assert!(app.tail_message_updated);

        reply.deleted = true;
        reply.parent_msg_id = None;
        reply.created_at.clear();
        reply.body_html.clear();
        reply.body_plain.clear();
        app.storage.upsert_messages(&[reply.clone()]).unwrap();
        reset_mapped_message_rows();
        app.handle_core_event(&mut ui, event(reply));
        assert_eq!(mapped_message_rows(), 1580);
        assert_eq!(app.messages[root_index].reply_count, 0);
        assert_eq!(app.messages[root_index].reactions[0].count, 1);
        assert!(!app.tail_message_updated);
        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn tui_preview_mention_removal_delete_and_active_read_guard() {
        use crate::storage::repo::{mapped_message_rows, reset_mapped_message_rows};

        let (mut app, mut ui) = make_test_app();
        let conv = "chat:c-tui";
        app.my_user_id = "me-1".to_string();
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "c-tui".to_string(),
        });
        let summary = ConversationSummary {
            conv_key: conv.to_string(),
            target: ConversationTarget::Chat {
                chat_id: "c-tui".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Alice".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: Some("2026-11-20T05:00:00Z".to_string()),
            last_msg_id: Some("m1".to_string()),
            last_msg_author: Some("Alice".to_string()),
            last_msg_preview: Some("@Me original".to_string()),
            last_updated_at: Some("2026-11-20T05:00:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u2".to_string()),
            web_url: None,
        };
        app.storage
            .upsert_conversations(std::slice::from_ref(&summary))
            .unwrap();
        app.conversations = vec![summary];

        // 1) Message with @me mention -> edited to remove @me -> activity_mentions must clear!
        let mut m1 = make_test_msg(conv, "m1", "2026-11-20T05:00:00Z", "@Me original");
        m1.mentions.push(crate::models::MentionInfo {
            id: 0,
            user_id: "me-1".to_string(),
            display_name: "Me".to_string(),
        });
        app.storage.upsert_messages(std::slice::from_ref(&m1)).unwrap();
        app.messages = vec![m1.clone()];
        app.activity_mentions = vec![m1.clone()];

        let mut m1_no_mention = m1.clone();
        m1_no_mention.mentions.clear();
        m1_no_mention.body_plain = "edited without mention".to_string();
        m1_no_mention.body_html = "<p>edited without mention</p>".to_string();
        m1_no_mention.last_modified_at = Some("2026-11-20T05:01:00Z".to_string());
        app.storage
            .upsert_messages(std::slice::from_ref(&m1_no_mention))
            .unwrap();

        reset_mapped_message_rows();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: conv.to_string(),
                parent_msg_id: None,
                messages: vec![m1_no_mention],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["m1".to_string()]),
            },
        );
        assert_eq!(mapped_message_rows(), 1, "single edited row must read 1 message row");
        assert!(
            app.activity_mentions.is_empty(),
            "removing @me via edit must clear activity_mentions"
        );
        assert_eq!(app.messages[0].body_plain, "edited without mention");

        // 2) Active read guard: stale ConversationsUpdated with unread_count=1 must not resurrect UNREAD.
        app.active_marked_read_at = Some((conv.to_string(), "2026-11-20T05:05:00Z".to_string()));
        let mut stale_conv = app.conversations[0].clone();
        stale_conv.unread_count = 1;
        stale_conv.last_updated_at = Some("2026-11-20T05:00:00Z".to_string());
        app.handle_core_event(&mut ui, CoreEvent::ConversationsUpdated(vec![stale_conv]));
        assert_eq!(app.conversations[0].unread_count, 0);

        // 3) UiMsg::MessageDeleted updates active message, thread root, and conversation preview.
        app.active_thread = Some(ThreadView {
            conv_key: conv.to_string(),
            root: app.messages[0].clone(),
            replies: Vec::new(),
        });
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageDeleted {
                conv_key: conv.to_string(),
                msg_id: "m1".to_string(),
                parent_msg_id: None,
            },
        );
        assert!(app.messages[0].deleted);
        assert_eq!(app.messages[0].body_plain, "(deleted message)");
        assert!(app.active_thread.as_ref().unwrap().root.deleted);
        assert_eq!(
            app.conversations[0].last_msg_preview.as_deref(),
            Some("(deleted message)")
        );

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn background_channel_events_update_sidebar_without_disturbing_open_conv_thread_or_draft() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (core_tx, _core_rx) = mpsc::unbounded_channel();
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), core_tx);

        let team = TeamSummary {
            id: "t1".to_string(),
            display_name: "Eng".to_string(),
            description: None,
            channels: vec![ChannelSummary {
                id: "ch1".to_string(),
                team_id: "t1".to_string(),
                team_name: "Eng".to_string(),
                display_name: "General".to_string(),
                description: None,
                membership_type: "standard".to_string(),
                web_url: None,
            }],
        };
        storage.upsert_teams(std::slice::from_ref(&team)).unwrap();
        let active_chat_target = ConversationTarget::Chat {
            chat_id: "c-open".to_string(),
        };
        let active_key = active_chat_target.conv_key();
        storage
            .upsert_conversations(&[ConversationSummary {
                conv_key: active_key.clone(),
                target: active_chat_target.clone(),
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Alice".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: Some("a1".to_string()),
                last_msg_author: Some("Alice".to_string()),
                last_msg_preview: Some("hello".to_string()),
                last_updated_at: Some("2026-11-20T05:00:00Z".to_string()),
                pinned: false,
                peer_user_id: Some("u2".to_string()),
                web_url: None,
            }])
            .unwrap();
        let active_msg = make_test_msg(&active_key, "a1", "2026-11-20T05:00:00Z", "hello");
        storage
            .upsert_messages(std::slice::from_ref(&active_msg))
            .unwrap();

        let mut app = AppState::new(cfg.clone(), auth, graph, storage.clone(), sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        app.teams = vec![team];
        app.conversations = storage.list_conversations().unwrap();
        app.active_conv_key = Some(active_key.clone());
        app.active_target = Some(active_chat_target);
        app.messages = vec![active_msg.clone()];
        app.active_thread = Some(ThreadView {
            conv_key: active_key.clone(),
            root: active_msg,
            replies: Vec::new(),
        });
        ui.chat_scroll.selected_idx = Some(0);
        ui.composer.load_text("draft in progress");

        let bg_chan_key = "chan:t1:ch1".to_string();
        let bg_msg = make_test_msg(&bg_chan_key, "cm-1", "2026-11-20T05:10:00Z", "Channel news");
        storage
            .upsert_messages(std::slice::from_ref(&bg_msg))
            .unwrap();
        storage.increment_conversation_unread(&bg_chan_key, 1).unwrap();
        let updated_convs = storage.list_conversations().unwrap();

        app.handle_core_event(
            &mut ui,
            CoreEvent::MessagesUpdated {
                conv_key: bg_chan_key.clone(),
                parent_msg_id: None,
                messages: vec![bg_msg],
                notify_candidate: None,
                changed_msg_ids: Some(vec!["cm-1".to_string()]),
            },
        );
        app.handle_core_event(&mut ui, CoreEvent::ConversationsUpdated(updated_convs));

        assert_eq!(app.active_conv_key.as_deref(), Some(active_key.as_str()));
        assert_eq!(ui.chat_scroll.selected_idx, Some(0));
        assert_eq!(ui.composer.text(), "draft in progress");
        assert_eq!(
            app.active_thread.as_ref().map(|t| t.root.msg_id.as_str()),
            Some("a1")
        );
        assert_eq!(app.unread_channel_count(), 1);
        ui.active_nav_tab = crate::ui::nav_pane::TAB_TEAMS;
        let rows = crate::ui::nav_pane::nav_rows_cached(&app, &ui);
        assert!(rows.iter().any(|r| matches!(
            r,
            crate::ui::nav_pane::NavRow::TeamChannel { conv_key, unread: 1, .. }
            if conv_key == &bg_chan_key
        )));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn async_loader_seq_guards_and_thread_optimistic_merge() {
        let (mut app, mut ui) = make_test_app();
        let conv = "chan:t1:ch1";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Channel {
            team_id: "t1".into(),
            channel_id: "ch1".into(),
        });

        // 1) UserSearchResults: stale seq or mismatched NewChat query must be ignored.
        app.user_search_seq = 5;
        app.user_searching = true;
        let mut nc = super::super::overlays::new_chat::NewChatState::new();
        nc.peers_input = "bob".into();
        ui.push_overlay(Overlay::NewChat(nc));
        let alice = UserProfile {
            id: "u1".into(),
            display_name: "Alice".into(),
            mail: None,
            user_principal_name: None,
        };
        let bob = UserProfile {
            id: "u2".into(),
            display_name: "Bob".into(),
            mail: None,
            user_principal_name: None,
        };
        app.handle_ui_msg(
            &mut ui,
            UiMsg::UserSearchResults {
                req_seq: 4,
                query: "bob".into(),
                users: vec![alice.clone()],
            },
        );
        assert!(app.user_searching);
        assert!(app.user_search_results.is_empty());

        app.handle_ui_msg(
            &mut ui,
            UiMsg::UserSearchResults {
                req_seq: 5,
                query: "alice".into(),
                users: vec![alice],
            },
        );
        assert!(app.user_searching);
        assert!(app.user_search_results.is_empty());

        app.handle_ui_msg(
            &mut ui,
            UiMsg::UserSearchResults {
                req_seq: 5,
                query: "bob".into(),
                users: vec![bob],
            },
        );
        assert!(!app.user_searching);
        assert_eq!(app.user_search_results.len(), 1);
        assert_eq!(app.user_search_results[0].id, "u2");
        ui.overlays.clear();

        // 2) ThreadLoaded: stale req_seq ignored; current req_seq merges optimistic replies & reveals target reply_id.
        let root = make_test_msg(conv, "root-1", "2026-11-20T00:00:00Z", "root");
        let mut opt_reply = make_test_msg(conv, "optimistic-1", "2026-11-20T00:05:00Z", "pending");
        opt_reply.parent_msg_id = Some("root-1".into());
        app.messages = vec![root.clone()];
        app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), Some("rep-2".into()));
        let seq = app.thread_req_seq;
        app.active_thread.as_mut().unwrap().replies.push(opt_reply);

        let mut rep1 = make_test_msg(conv, "rep-1", "2026-11-20T00:01:00Z", "first");
        rep1.parent_msg_id = Some("root-1".into());
        let mut rep2 = make_test_msg(conv, "rep-2", "2026-11-20T00:02:00Z", "target");
        rep2.parent_msg_id = Some("root-1".into());
        let fetched = ThreadView {
            conv_key: conv.to_string(),
            root: root.clone(),
            replies: vec![rep1, rep2],
        };

        // Stale seq must be rejected.
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ThreadLoaded {
                req_seq: seq - 1,
                conv_key: conv.to_string(),
                root_msg_id: "root-1".into(),
                thread: fetched.clone(),
            },
        );
        assert!(app.loading_thread);

        // Current seq merges optimistic reply AND selects/reveals rep-2 (index 1 + 1 = 2).
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ThreadLoaded {
                req_seq: seq,
                conv_key: conv.to_string(),
                root_msg_id: "root-1".into(),
                thread: fetched,
            },
        );
        assert!(!app.loading_thread);
        let reply_ids: Vec<&str> = app
            .active_thread
            .as_ref()
            .unwrap()
            .replies
            .iter()
            .map(|r| r.msg_id.as_str())
            .collect();
        assert_eq!(reply_ids, vec!["rep-1", "rep-2", "optimistic-1"]);
        assert_eq!(ui.thread_scroll.selected_idx, Some(2));
        assert!(ui.pending_thread_reveal);

        // 3) ClipboardImageEncoded & HtmlAttachmentLoaded with stale seq/conv_key are ignored.
        app.clipboard_paste_seq = 10;
        app.html_fetch_seq = 20;
        ui.set_focus(Focus::Chat);
        let target_thread = ui.composer_target_thread;
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ClipboardImageEncoded {
                req_seq: 9,
                conv_key: Some(conv.to_string()),
                composer_target_thread: target_thread,
                width: 10,
                height: 10,
                file_name: "stale.png".into(),
                png_bytes: vec![1, 2, 3],
                preview: None,
            },
        );
        assert!(ui.composer.pending_images.is_empty());
        assert_eq!(ui.focus(), Focus::Chat);

        app.handle_ui_msg(
            &mut ui,
            UiMsg::HtmlAttachmentLoaded {
                req_seq: 20,
                conv_key: Some("chat:other".into()),
                title: "stale".into(),
                html: "<p>stale</p>".into(),
                kind: bitefoundry_html::ContentKind::Html,
            },
        );
        assert!(ui.overlays.is_empty());

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn message_deleted_clears_metadata_and_failed_send_restores_original_target() {
        let (mut app, mut ui) = make_test_app();
        let conv = "chan:t1:ch1";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Channel {
            team_id: "t1".into(),
            channel_id: "ch1".into(),
        });
        let mut msg = make_test_msg(conv, "m1", "2026-11-20T00:00:00Z", "with att");
        msg.subject = Some("Sub".into());
        msg.attachments.push(AttachmentInfo {
            id: "a1".into(),
            content_type: "application/pdf".into(),
            content_url: None,
            name: Some("doc.pdf".into()),
            content_json: None,
            thumbnail_url: None,
        });
        app.messages = vec![msg];

        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageDeleted {
                conv_key: conv.to_string(),
                msg_id: "m1".into(),
                parent_msg_id: None,
            },
        );
        assert!(app.messages[0].deleted);
        assert!(app.messages[0].attachments.is_empty());
        assert!(app.messages[0].reactions.is_empty());
        assert!(app.messages[0].mentions.is_empty());
        assert!(app.messages[0].subject.is_none());
        assert!(
            super::super::overlays::attachments::rows_from(&app.messages, MessageScope::Chat)
                .is_empty()
        );

        // Failed thread reply while user is now in main composer must NOT clobber main composer;
        // it must restore into the thread draft key instead.
        ui.composer_target_thread = false;
        ui.composer.load_text("");
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSendFailed {
                conv_key: conv.to_string(),
                parent_msg_id: Some("root-9".into()),
                optimistic_id: None,
                text: "caption for failed thread image".into(),
            },
        );
        assert!(ui.composer.text().is_empty());
        assert_eq!(
            app.drafts.get(&format!("{conv}#thread:root-9")).map(String::as_str),
            Some("caption for failed thread image")
        );

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn bootstrap_emits_before_peer_presence_and_delivers_peer_presences_async() {
        use axum::{routing::{get, post}, Json, Router};
        use tokio::sync::Notify;

        let release_presence = Arc::new(Notify::new());
        let rel_clone = Arc::clone(&release_presence);

        let router = Router::new()
            .route(
                "/v1.0/me",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "me-1",
                        "displayName": "Me",
                        "mail": "me@example.com"
                    }))
                }),
            )
            .route(
                "/v1.0/me/presence",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "me-1",
                        "availability": "Available",
                        "activity": "Available"
                    }))
                }),
            )
            .route(
                "/v1.0/me/chats",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "c-dm",
                            "chatType": "oneOnOne",
                            "members": [
                                { "userId": "me-1", "displayName": "Me" },
                                { "userId": "peer-1", "displayName": "Alice" }
                            ]
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/communications/getPresencesByUserId",
                post(move || {
                    let release = Arc::clone(&rel_clone);
                    async move {
                        release.notified().await;
                        Json(serde_json::json!({
                            "value": [{
                                "id": "peer-1",
                                "availability": "Busy",
                                "activity": "InACall"
                            }]
                        }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..Default::default()
        };
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let graph = GraphClient::new(cfg.clone(), auth.clone())
            .unwrap()
            .with_my_user_id("me-1");
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);
        assert_eq!(app.my_user_id, "me-1");
        let mut ui = UiState::new();
        app.spawn_teams_bootstrap(None);

        // BootstrapDone must arrive while getPresencesByUserId is still blocked on Notify.
        let first_msg = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("BootstrapDone must not block behind peer presence RTT")
            .unwrap();
        assert!(matches!(first_msg, UiMsg::BootstrapDone { .. }));
        app.handle_ui_msg(&mut ui, first_msg);
        assert_eq!(app.conversations.len(), 1);
        assert!(!app.presences.contains_key("peer-1"));

        // Releasing peer presence delivers PeerPresencesLoaded asynchronously (ignoring any
        // MembersLoaded/MessagesLoaded emitted by auto-selecting the first conversation).
        release_presence.notify_one();
        let peer_msg = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let msg = ui_rx.recv().await.unwrap();
                if matches!(msg, UiMsg::PeerPresencesLoaded(_)) {
                    break msg;
                }
                app.handle_ui_msg(&mut ui, msg);
            }
        })
        .await
        .expect("PeerPresencesLoaded should arrive once presence finishes");
        app.handle_ui_msg(&mut ui, peer_msg);
        assert_eq!(
            app.presences.get("peer-1").map(|p| p.availability.as_str()),
            Some("Busy")
        );

        server.abort();
    }

    #[tokio::test]
    async fn select_conversation_fetches_single_50_page_and_interactive_prefetch_warms_uncached_neighbors() {
        use axum::{routing::{get, post}, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let c1_msg_hits = Arc::new(AtomicUsize::new(0));
        let batch_hits = Arc::new(AtomicUsize::new(0));
        let batch_urls = Arc::new(Mutex::new(Vec::<String>::new()));

        let c1_hits_cl = Arc::clone(&c1_msg_hits);
        let batch_hits_cl = Arc::clone(&batch_hits);
        let batch_urls_cl = Arc::clone(&batch_urls);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let c1_next = format!("{base}/chats/c1/messages?page=2");

        let router = Router::new()
            .route(
                "/v1.0/chats/c1/messages",
                get(move || {
                    let hits = Arc::clone(&c1_hits_cl);
                    let next = c1_next.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let vals: Vec<serde_json::Value> = (0..50)
                            .map(|i| {
                                serde_json::json!({
                                    "id": format!("c1-m{i:02}"),
                                    "messageType": "message",
                                    "createdDateTime": format!("2026-11-20T01:{:02}:00Z", i),
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "text", "content": format!("msg {i}") }
                                })
                            })
                            .collect();
                        Json(serde_json::json!({ "value": vals, "@odata.nextLink": next }))
                    }
                }),
            )
            .route(
                "/v1.0/chats/{id}/members",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/chats/{id}/markChatReadForUser",
                post(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(payload): Json<serde_json::Value>| {
                    let hits = Arc::clone(&batch_hits_cl);
                    let urls = Arc::clone(&batch_urls_cl);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                        let mut responses = Vec::new();
                        for r in reqs {
                            let id = r["id"].as_str().unwrap_or("").to_string();
                            let url = r["url"].as_str().unwrap_or("").to_string();
                            urls.lock().unwrap().push(url.clone());
                            let chat_id = url
                                .strip_prefix("/chats/")
                                .and_then(|s| s.split('/').next())
                                .unwrap_or("unknown");
                            responses.push(serde_json::json!({
                                "id": id,
                                "status": 200,
                                "body": {
                                    "value": [{
                                        "id": format!("{chat_id}-warm-1"),
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-20T02:00:00Z",
                                        "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                        "body": { "contentType": "text", "content": "prefetched" }
                                    }]
                                }
                            }));
                        }
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig {
            graph_base_url: base,
            ..Default::default()
        };
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let graph = GraphClient::new(cfg.clone(), auth.clone())
            .unwrap()
            .with_my_user_id("me-1");
        let storage = Storage::open_in_memory().unwrap();
        let mk_chat = |id: &str, title: &str, ts: &str| ConversationSummary {
            conv_key: format!("chat:{id}"),
            target: ConversationTarget::Chat {
                chat_id: id.into(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: title.into(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: Some(ts.into()),
            pinned: false,
            peer_user_id: Some(format!("peer-{id}")),
            web_url: None,
        };
        storage
            .upsert_conversations(&[
                mk_chat("c0", "Zero", "2026-11-20T05:00:00Z"),
                mk_chat("c1", "One", "2026-11-20T04:00:00Z"),
                mk_chat("c2", "Two", "2026-11-20T03:00:00Z"),
                mk_chat("c3", "Three", "2026-11-20T02:00:00Z"),
            ])
            .unwrap();
        // Pre-cache c0 and c2 so auto-select renders c0 immediately and prefetch skips cached c2.
        storage
            .upsert_messages(&[
                make_test_msg("chat:c0", "c0-m1", "2026-11-20T05:00:00Z", "cached c0"),
                make_test_msg("chat:c2", "c2-m1", "2026-11-20T03:00:00Z", "cached c2"),
            ])
            .unwrap();

        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new(cfg, auth, graph, storage.clone(), sync_ctrl, ui_tx);
        let mut ui = UiState::new();
        app.load_cached_teams();

        // 1) Initial cached auto-selection renders c0 immediately without prefetch.
        app.select_initial_cached_conversation(&mut ui);
        assert_eq!(app.active_conv_key.as_deref(), Some("chat:c0"));
        assert_eq!(app.messages.len(), 1);
        assert!(!app.loading_messages);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            batch_hits.load(Ordering::SeqCst),
            0,
            "startup auto-selection must not trigger speculative prefetch"
        );

        // 2) Interactive switch to cold c1 fetches exactly 1 Graph page ($top=50) and saves cursor,
        //    while prefetching uncached neighbor c3 (skipping already-cached c0 and c2).
        app.select_conversation(
            &mut ui,
            ConversationTarget::Chat {
                chat_id: "c1".into(),
            },
        );
        let loaded = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let msg = ui_rx.recv().await.unwrap();
                if matches!(&msg, UiMsg::MessagesLoaded { conv_key, .. } if conv_key == "chat:c1") {
                    break msg;
                }
                app.handle_ui_msg(&mut ui, msg);
            }
        })
        .await
        .expect("MessagesLoaded for chat:c1 should arrive");
        app.handle_ui_msg(&mut ui, loaded);
        assert_eq!(app.messages.len(), 50);
        assert_eq!(
            c1_msg_hits.load(Ordering::SeqCst),
            1,
            "cold conversation open must issue 1 single $top=50 page, not 2 sequential pages"
        );
        let cursor = storage
            .get_sync_cursor(&SyncCursor::history_scope("chat:c1"))
            .unwrap()
            .expect("initial history cursor must be saved on cold open");
        assert!(cursor.delta_link.as_deref().is_some_and(|l| l.contains("page=2")));

        // Wait briefly for background neighbor prefetch to populate c3 in SQLite.
        let mut warmed = false;
        for _ in 0..40 {
            if !storage.list_root_messages("chat:c3", 10, None).unwrap().is_empty() {
                warmed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(warmed, "uncached adjacent chat c3 should be warmed in SQLite");
        assert_eq!(batch_hits.load(Ordering::SeqCst), 1);
        let fetched_urls = batch_urls.lock().unwrap().clone();
        assert_eq!(fetched_urls.len(), 1, "already-cached neighbors c0/c2 must not be fetched");
        assert!(fetched_urls[0].contains("/chats/c3/messages"));

        // 3) Switching to c3 now shows cached messages immediately with loading_messages == false,
        //    and respects the 3s prefetch cooldown (still 1 batch hit total).
        app.select_conversation(
            &mut ui,
            ConversationTarget::Chat {
                chat_id: "c3".into(),
            },
        );
        assert!(!app.loading_messages);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].msg_id, "c3-warm-1");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(batch_hits.load(Ordering::SeqCst), 1);

        server.abort();
    }

    fn mock_graph_auth(cfg: &AppConfig) -> AuthManager {
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        AuthManager::from_tokens(cfg.clone(), tok)
    }

    /// An app wired to a mock Graph at base_url, keeping the UiMsg receiver for the test.
    fn make_mock_graph_app(
        base_url: &str,
    ) -> (AppState, UiState, mpsc::UnboundedReceiver<UiMsg>) {
        let cfg = AppConfig {
            graph_base_url: base_url.to_string(),
            ..Default::default()
        };
        let auth = mock_graph_auth(&cfg);
        let graph = GraphClient::new(cfg.clone(), auth.clone())
            .unwrap()
            .with_my_user_id("me-1");
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) =
            tokio::sync::watch::channel(crate::sync::FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui, ui_rx)
    }

    // Track 5: delta sync leaves channel roots at reply_count 0, so a loaded thread must carry
    // its count back to the root the channel view draws the reply pill from.
    #[tokio::test]
    async fn thread_loaded_backfills_reply_count_into_channel_root() {
        let (mut app, mut ui) = make_test_app();
        let conv = "chan:t1:ch1";
        app.active_conv_key = Some(conv.to_string());
        app.active_target = Some(ConversationTarget::Channel {
            team_id: "t1".into(),
            channel_id: "ch1".into(),
        });
        let root = make_test_msg(conv, "root-1", "2026-11-20T00:00:00Z", "root");
        let other = make_test_msg(conv, "root-2", "2026-11-20T00:10:00Z", "other");
        app.messages = vec![root.clone(), other];
        let reply = |id: &str, minute: u32| {
            let mut r = make_test_msg(conv, id, &format!("2026-11-20T00:0{minute}:00Z"), id);
            r.parent_msg_id = Some("root-1".into());
            r
        };

        // 1) Graph's count on the fetched root covers replies beyond the loaded window.
        app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), None);
        let mut fetched_root = root.clone();
        fetched_root.reply_count = 5;
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ThreadLoaded {
                req_seq: app.thread_req_seq,
                conv_key: conv.to_string(),
                root_msg_id: "root-1".into(),
                thread: ThreadView {
                    conv_key: conv.to_string(),
                    root: fetched_root,
                    replies: vec![reply("rep-1", 1), reply("rep-2", 2)],
                },
            },
        );
        assert_eq!(app.messages[0].reply_count, 5);
        assert_eq!(app.messages[1].reply_count, 0, "other roots keep their count");

        // 2) A root still at 0 (delta-sourced) takes the number of live replies loaded.
        app.messages[0].reply_count = 0;
        app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), None);
        let mut deleted = reply("rep-3", 3);
        deleted.deleted = true;
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ThreadLoaded {
                req_seq: app.thread_req_seq,
                conv_key: conv.to_string(),
                root_msg_id: "root-1".into(),
                thread: ThreadView {
                    conv_key: conv.to_string(),
                    root: root.clone(),
                    replies: vec![reply("rep-1", 1), reply("rep-2", 2), deleted],
                },
            },
        );
        assert_eq!(app.messages[0].reply_count, 2);
        assert_eq!(app.messages[1].reply_count, 0);

        // 3) A stale load for an older request leaves the count alone.
        app.messages[0].reply_count = 0;
        app.open_thread_by_id_with_reply(&mut ui, "root-1".into(), None);
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ThreadLoaded {
                req_seq: app.thread_req_seq.wrapping_sub(1),
                conv_key: conv.to_string(),
                root_msg_id: "root-1".into(),
                thread: ThreadView {
                    conv_key: conv.to_string(),
                    root: root.clone(),
                    replies: vec![reply("rep-1", 1)],
                },
            },
        );
        assert_eq!(app.messages[0].reply_count, 0);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    // Track 5: a channel cached only from delta sync shows no reply pills; the first open this
    // session refreshes one $expand=replies page in the background, and later opens do not.
    #[tokio::test]
    async fn channel_open_hydrates_delta_cached_reply_counts_once_per_session() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let page_hits = Arc::new(AtomicUsize::new(0));
        let page_hits_c = Arc::clone(&page_hits);
        let router = Router::new().route(
            "/v1.0/teams/t1/channels/ch1/messages",
            get(move || {
                let hits = Arc::clone(&page_hits_c);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let reply = |id: &str, ts: &str| {
                        serde_json::json!({
                            "id": id,
                            "messageType": "message",
                            "createdDateTime": ts,
                            "from": { "user": { "id": "u2", "displayName": "Bob" } },
                            "body": { "contentType": "text", "content": id }
                        })
                    };
                    Json(serde_json::json!({
                        "value": [{
                            "id": "root-1",
                            "messageType": "message",
                            "createdDateTime": "2026-11-20T00:00:00Z",
                            "from": { "user": { "id": "u1", "displayName": "Alice" } },
                            "body": { "contentType": "text", "content": "root" },
                            "replies": [
                                reply("rep-1", "2026-11-20T00:01:00Z"),
                                reply("rep-2", "2026-11-20T00:02:00Z")
                            ]
                        }]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let (mut app, mut ui, mut ui_rx) = make_mock_graph_app(&format!("http://{addr}/v1.0"));
        let conv = "chan:t1:ch1";
        // The delta-synced root: no replies cached, so its count is 0.
        app.storage
            .upsert_messages(&[make_test_msg(conv, "root-1", "2026-11-20T00:00:00Z", "root")])
            .unwrap();
        let channel = ConversationTarget::Channel {
            team_id: "t1".into(),
            channel_id: "ch1".into(),
        };

        app.select_conversation(&mut ui, channel.clone());
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].reply_count, 0);
        assert!(!app.loading_messages, "cached roots render without a loading state");
        let loaded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = ui_rx.recv().await.unwrap();
                if matches!(&msg, UiMsg::MessagesLoaded { conv_key, .. } if conv_key == conv) {
                    break msg;
                }
                app.handle_ui_msg(&mut ui, msg);
            }
        })
        .await
        .expect("the reply hydration page should load");
        app.handle_ui_msg(&mut ui, loaded);
        assert_eq!(page_hits.load(Ordering::SeqCst), 1);
        let root = app
            .messages
            .iter()
            .find(|m| m.msg_id == "root-1")
            .expect("root stays in the channel view");
        assert_eq!(root.reply_count, 2);
        assert_eq!(
            app.storage.get_message(conv, "root-1").unwrap().unwrap().reply_count,
            2
        );

        // Reopening the hydrated channel in the same session does not fetch the page again.
        app.select_conversation(
            &mut ui,
            ConversationTarget::Chat {
                chat_id: "c-other".into(),
            },
        );
        app.select_conversation(&mut ui, channel);
        assert_eq!(app.messages[0].reply_count, 2);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(page_hits.load(Ordering::SeqCst), 1);

        server.abort();
    }

    // Track 5: the stored CHATS_LIST_SCOPE cursor decides paging, not the cached flag, and an
    // expired $skiptoken is dropped instead of being retried forever.
    #[tokio::test]
    async fn load_older_chats_recovers_from_stale_eof_and_expired_skiptoken() {
        use axum::{extract::RawQuery, http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dead_hits = Arc::new(AtomicUsize::new(0));
        let list_hits = Arc::new(AtomicUsize::new(0));
        let dead_hits_c = Arc::clone(&dead_hits);
        let list_hits_c = Arc::clone(&list_hits);
        let router = Router::new().route(
            "/v1.0/me/chats",
            get(move |RawQuery(q): RawQuery| {
                let dead_hits = Arc::clone(&dead_hits_c);
                let list_hits = Arc::clone(&list_hits_c);
                async move {
                    if q.unwrap_or_default().contains("skiptoken=dead") {
                        dead_hits.fetch_add(1, Ordering::SeqCst);
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": { "code": "BadRequest", "message": "Invalid skip token." }
                            })),
                        );
                    }
                    list_hits.fetch_add(1, Ordering::SeqCst);
                    // A full page of 50 chats with no nextLink.
                    let chats: Vec<serde_json::Value> = (0..50)
                        .map(|i| {
                            serde_json::json!({
                                "id": format!("c-{i:02}"),
                                "chatType": "group",
                                "topic": format!("Chat {i:02}"),
                                "members": []
                            })
                        })
                        .collect();
                    (StatusCode::OK, Json(serde_json::json!({ "value": chats })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let base = format!("http://{addr}/v1.0");
        let (mut app, mut ui, mut ui_rx) = make_mock_graph_app(&base);
        let chats_cursor =
            |app: &AppState| app.storage.get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE).unwrap();
        let save_cursor = |app: &AppState, delta_link: Option<String>, oldest: Option<&str>| {
            app.storage
                .save_sync_cursor(&SyncCursor {
                    scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
                    delta_link,
                    oldest_msg_ts: oldest.map(str::to_string),
                    synced_at: Utc::now().to_rfc3339(),
                })
                .unwrap();
        };
        async fn next_older_chats_msg(ui_rx: &mut mpsc::UnboundedReceiver<UiMsg>) -> UiMsg {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let msg = ui_rx.recv().await.unwrap();
                    if matches!(&msg, UiMsg::OlderChatsLoaded { .. } | UiMsg::OlderChatsFailed) {
                        break msg;
                    }
                }
            })
            .await
            .expect("older chats load should finish")
        }
        let loaded_summary = |msg: &UiMsg| match msg {
            UiMsg::OlderChatsLoaded {
                conversations,
                reached_eof,
            } => (conversations.len(), *reached_eof),
            _ => panic!("expected OlderChatsLoaded"),
        };

        // 1) A stored EOF ends paging without a request.
        save_cursor(&app, None, Some(SyncCursor::HISTORY_EOF));
        app.load_older_chats();
        assert!(app.chats_oldest_reached);
        assert!(!app.loading_older_chats);
        assert_eq!(list_hits.load(Ordering::SeqCst), 0);

        // 2) A fresh page-1 sync replaced that EOF with a nextLink (the stored EOF timestamp
        //    stays beside it): the cached flag must not block the link. The link has expired
        //    (400), so the cursor is dropped and the count-based listing loads chats instead.
        let dead_link = format!("{base}/me/chats?$skiptoken=dead");
        save_cursor(&app, Some(dead_link.clone()), None);
        app.load_older_chats();
        assert!(app.loading_older_chats);
        let loaded = next_older_chats_msg(&mut ui_rx).await;
        assert_eq!(loaded_summary(&loaded), (50, false));
        assert_eq!(dead_hits.load(Ordering::SeqCst), 1);
        assert_eq!(list_hits.load(Ordering::SeqCst), 1);
        // Clearing only the link would have revived the old EOF and stopped paging for good.
        let cursor = chats_cursor(&app);
        assert!(
            cursor.as_ref().is_none_or(|c| !c.is_history_eof()
                && c.delta_link.as_deref() != Some(dead_link.as_str())),
            "the dead cursor must be gone, not turned back into EOF: {cursor:?}"
        );
        app.handle_ui_msg(&mut ui, loaded);
        assert!(!app.loading_older_chats);
        assert!(!app.chats_oldest_reached);
        assert_eq!(app.conversations.len(), 50);

        // 3) Paging continues without the dead link: no second request for it.
        app.load_older_chats();
        assert!(app.loading_older_chats);
        let again = next_older_chats_msg(&mut ui_rx).await;
        assert_eq!(loaded_summary(&again), (50, true));
        assert_eq!(dead_hits.load(Ordering::SeqCst), 1);
        assert_eq!(list_hits.load(Ordering::SeqCst), 2);
        assert!(chats_cursor(&app).is_some_and(|c| c.is_history_eof()));
        app.handle_ui_msg(&mut ui, again);
        assert!(app.chats_oldest_reached);

        // 4) A cursor whose link was replaced meanwhile is not deleted by a stale expiry.
        let fresh = format!("{base}/me/chats?$skiptoken=fresh");
        save_cursor(&app, Some(fresh.clone()), None);
        assert!(!app
            .storage
            .delete_sync_cursor_with_link(SyncCursor::CHATS_LIST_SCOPE, &dead_link)
            .unwrap());
        assert_eq!(
            chats_cursor(&app).and_then(|c| c.delta_link).as_deref(),
            Some(fresh.as_str())
        );

        server.abort();
    }
}
