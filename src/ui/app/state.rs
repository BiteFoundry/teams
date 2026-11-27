//! UI-only state (`UiState`), focus/scroll primitives, and async `UiMsg` messages.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;

use crate::config::AppConfig;
use crate::img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::{
    AttachmentInfo, ConversationSummary, DownloadedFile, MemberInfo, MessageItem, PresenceStatus,
    SearchHit, TeamSummary, ThreadView, UpdateCheckResult, UserProfile,
};

use super::super::composer::ComposerState;
use super::super::hitmap::HitMap;
use super::super::keymap::Keymap;
use super::super::overlays::{Overlay, OverlayKind};
use super::super::theme::Theme;
use super::super::toast::{TaskBoard, ToastKind, ToastQueue};

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
    pub last_click: Option<(std::time::Instant, u16, u16, super::super::hitmap::HitTarget)>,
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
            super::super::action::Action::PrevFocus,
        );
        keymap.bind(
            Some(Focus::Composer),
            KeyCode::BackTab,
            KeyModifiers::NONE,
            super::super::action::Action::PrevFocus,
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

pub(super) struct AttachmentDecodeJob {
    pub(super) key: String,
    pub(super) msg_id: String,
    pub(super) att: AttachmentInfo,
    pub(super) fit: img_preview::FitBox,
    pub(super) cell_px: (u16, u16),
    pub(super) bg: (u8, u8, u8),
    pub(super) caps: Option<TerminalGraphicsCaps>,
}
